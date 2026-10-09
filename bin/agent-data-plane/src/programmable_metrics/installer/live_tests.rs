//! Live configuration changes on the connected configuration path, in a running plan-driven topology.
//!
//! The configuration system is started as `run.rs` starts it with the Core Agent, with `LoadedConfiguration::run` over
//! a configuration stream, and `ConfigUpdatesWorker` applies what the stream sends next. Tests push
//! `ConfigUpdate::Partial` updates of Datadog keys into the stream, while scripted DogStatsD batches flow through the
//! topology (see [`test_topology`](crate::programmable_metrics::test_topology)). The batches are timestamped counters,
//! which the no-aggregation pipeline sends past aggregation, so each reaches the main output with the header it was
//! admitted with: the generation that processed it.

use std::{collections::BTreeMap, time::Duration};

use saluki_core::data_model::{
    event::metric::{context::Context, Metric},
    tags::{Tag, TagSet},
};
use serde_json::json;

use super::*;
use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        test_support::{connected_configuration_system, update},
        test_topology::{Aggregation, Delivery, MetricsPath, Options, TestTopology, PATIENCE},
    },
};

/// The metrics of every batch that no change in these tests blocks.
const KEPT: [&str; 3] = ["kept.a", "kept.b", "kept.c"];

/// Returns batch `index`: a timestamped counter of every name in `names`, each tagged with the batch.
fn batch(index: usize, names: &[&str]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| {
            let tags = TagSet::from_iter([Tag::from(format!("batch:{}", index))]);
            Metric::counter(
                Context::from_parts(name.to_string(), tags),
                (1_700_000_000 + index as u64, 1.0),
            )
        })
        .collect()
}

/// Returns the batch a metric of [`batch`] belongs to.
fn batch_of(metric: &Metric) -> usize {
    metric
        .context()
        .tags()
        .into_iter()
        .find_map(|tag| tag.as_str().strip_prefix("batch:").map(str::to_string))
        .and_then(|index| index.parse().ok())
        .unwrap_or_else(|| panic!("`{}` belongs to no batch", metric.context().name()))
}

/// Returns, for each metric name and batch, the generation of every buffer the main output received it in.
fn received(deliveries: &[Delivery]) -> BTreeMap<(String, usize), Vec<Option<u64>>> {
    let mut received = BTreeMap::<_, Vec<_>>::new();
    for delivery in deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode")
    {
        for metric in &delivery.metrics {
            received
                .entry((metric.context().name().to_string(), batch_of(metric)))
                .or_default()
                .push(delivery.generation);
        }
    }
    received
}

/// Sends batch `index` of `names` to the DogStatsD source, waits until the main output has received every metric of
/// it named in `delivered`, and returns the generation that processed the batch.
async fn send(topology: &mut TestTopology, index: usize, names: &[&str], delivered: &[&str]) -> Option<u64> {
    topology.feed("dsd_in.metrics", batch(index, names));
    let deliveries = topology
        .wait_for(&format!("batch {}", index), |deliveries| {
            let received = received(deliveries);
            delivered
                .iter()
                .all(|name| received.contains_key(&(name.to_string(), index)))
        })
        .await;
    received(deliveries)[&(delivered[0].to_string(), index)][0]
}

async fn status_where(status: &ArcSwap<PlanStatus>, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let current = status.load_full();
            if done(&current) {
                return PlanStatus::clone(&current);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the installer's status never changed as expected: {}", status.load()))
}

fn active(status: &PlanStatus) -> Option<u64> {
    status.installer.active.as_ref().map(|active| active.id.get())
}

fn live(status: &PlanStatus) -> Vec<u64> {
    status
        .installer
        .live
        .iter()
        .map(|generation| generation.get())
        .collect()
}

fn state_of(status: &PlanStatus, generation: u64) -> Option<GenerationState> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .map(|record| record.state)
}

/// Starts the plan-driven topology of `yaml` on the connected configuration path.
async fn start(yaml: &str) -> (TestTopology, mpsc::Sender<saluki_config::dynamic::ConfigUpdate>) {
    let (system, updates, stream) = connected_configuration_system(yaml).await;
    let topology = TestTopology::start(
        &system,
        Options::new(MetricsPath::Plan, Aggregation::Native),
        |supervisor| supervisor.add_worker(ConfigUpdatesWorker::new(updates)),
    )
    .await;
    (topology, stream)
}

#[tokio::test]
async fn a_live_filterlist_change_installs_a_generation_that_applies_at_a_batch_boundary_without_loss() {
    let (mut topology, stream) = start("").await;
    let status = topology.status().expect("the plan-driven path has an installer");
    let names = [KEPT[0], KEPT[1], KEPT[2], "blocked"];

    // Generation 1 runs the boot configuration, which blocks nothing. Each batch is delivered before the next is sent.
    let mut sent = 0;
    for _ in 0..5 {
        assert_eq!(send(&mut topology, sent, &names, &KEPT).await, Some(1));
        sent += 1;
    }

    // The Core Agent changes the filterlist. Batches keep flowing while the installer lowers the new configuration,
    // prepares it in both regions, and commits it, until one is admitted with generation 2.
    stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    loop {
        let generation = send(&mut topology, sent, &names, &KEPT).await;
        sent += 1;
        if generation == Some(2) {
            break;
        }
        assert!(sent < 10_000, "generation 2 never processed a batch");
    }
    let switched = sent - 1;
    for _ in 0..5 {
        assert_eq!(send(&mut topology, sent, &names, &KEPT).await, Some(2));
        sent += 1;
    }

    // Generation 1 retires once its last batch is delivered.
    let settled = status_where(&status, |status| live(status) == [2]).await;
    assert_eq!(active(&settled), Some(2));
    assert_eq!(state_of(&settled, 1), Some(GenerationState::Retired));
    assert_eq!(settled.config_changes, 1);
    let deliveries = topology.stop().await;

    // Every unrelated metric arrived exactly once. Each batch was processed whole by one generation: its metrics
    // arrived in buffers of that generation, and the filterlist blocked it exactly when it was generation 2's, from
    // the first batch admitted after the commit on.
    let received = received(&deliveries);
    for index in 0..sent {
        let generations = KEPT
            .iter()
            .map(|name| {
                let generations = &received[&(name.to_string(), index)];
                assert_eq!(
                    generations.len(),
                    1,
                    "batch {}: `{}` arrived {:?}",
                    index,
                    name,
                    generations
                );
                generations[0]
            })
            .collect::<Vec<_>>();
        let generation = generations[0];
        assert!(
            generations.iter().all(|other| *other == generation),
            "batch {}: {:?}",
            index,
            generations
        );
        let expected = if index < switched { Some(1) } else { Some(2) };
        assert_eq!(generation, expected, "batch {}", index);
        match received.get(&("blocked".to_string(), index)) {
            Some(blocked) => {
                assert_eq!(blocked, &[Some(1)], "batch {}: the blocked metric", index);
            }
            None => assert_eq!(generation, Some(2), "batch {} lost its unblocked metric", index),
        }
    }
}

#[tokio::test]
async fn a_change_to_a_startup_only_setting_leaves_the_plan_unchanged() {
    let (mut topology, stream) = start("statsd_metric_namespace: tenant").await;
    let status = topology.status().expect("the plan-driven path has an installer");
    let prefixed = ["tenant.kept.a"];
    assert_eq!(send(&mut topology, 0, &["kept.a"], &prefixed).await, Some(1));

    // The namespace prefix is read only at startup: the change is applied to the configuration, and the installer
    // lowers it, but the plan it lowers is the active one, so no generation is created.
    stream
        .send(update("statsd_metric_namespace", json!("other")))
        .await
        .unwrap();
    let observed = status_where(&status, |status| status.config_changes == 1).await;
    assert_eq!((active(&observed), live(&observed)), (Some(1), vec![1]));
    assert!(observed.installer.preparing.is_none() && observed.installer.pending.is_none());
    assert_eq!(
        observed
            .installer
            .generations
            .iter()
            .map(|record| record.id.get())
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(send(&mut topology, 1, &["kept.a"], &prefixed).await, Some(1));
    let deliveries = topology.stop().await;
    let names = received(&deliveries)
        .into_keys()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["tenant.kept.a", "tenant.kept.a"]);
}

#[tokio::test]
async fn a_burst_of_live_changes_stays_within_the_overlap_budget_and_ends_on_the_latest_configuration() {
    // The main output reads nothing until its gate opens, so every batch sent to it stays in flight, and keeps the
    // generation that processed it alive.
    let (system, updates, stream) = connected_configuration_system("").await;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.gated.insert("dd_metrics_encode".to_string());
    let mut topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    let status = topology.status().expect("the plan-driven path has an installer");
    status_where(&status, |status| active(status) == Some(1)).await;

    // Thirty changes in quick succession, each blocking another name, with a batch after each. Each change is
    // observed before the next is sent. Generations whose batches are still in flight stay alive, so once the
    // overlap budget's worth are alive, the latest change waits, replacing any change that waited before it.
    const CHANGES: usize = 30;
    let budget = InstallerConfig::default().overlap_budget;
    let names = ["kept.a", "blocked.28", "blocked.29"];
    let mut most_live = 0;
    for change in 0..CHANGES {
        stream
            .send(update("metric_filterlist", json!([format!("blocked.{}", change)])))
            .await
            .unwrap();
        topology.feed("dsd_in.metrics", batch(change, &names));
        let observed = status_where(&status, |status| status.config_changes == change as u64 + 1).await;
        most_live = most_live.max(observed.installer.live.len());
    }
    let waiting = status_where(&status, |status| {
        status.installer.preparing.is_none() && status.installer.pending.is_some()
    })
    .await;
    assert_eq!(waiting.installer.live.len(), budget, "{}", waiting);
    assert!(most_live <= budget, "{} generations were alive at once", most_live);
    assert!(waiting.last_error.is_none(), "{}", waiting);

    // Once the batches are delivered, the replaced generations retire, and the latest change installs: a batch is
    // processed by a generation that blocks `blocked.29` and lets `blocked.28` through.
    topology.open_gate("dd_metrics_encode");
    let settled = status_where(&status, |status| {
        status.installer.preparing.is_none() && status.installer.pending.is_none() && status.installer.live.len() == 1
    })
    .await;
    assert!(settled.last_error.is_none(), "{}", settled);
    for record in &settled.installer.generations {
        if Some(record.id.get()) != active(&settled) {
            assert!(record.state.is_final(), "{}: {:?}", record.id, record.state);
        }
    }
    send(&mut topology, CHANGES, &names, &["kept.a", "blocked.28"]).await;
    let deliveries = topology.stop().await;

    // The latest configuration applies, and no unrelated metric was lost or duplicated.
    let received = received(&deliveries);
    assert!(!received.contains_key(&("blocked.29".to_string(), CHANGES)));
    for index in 0..=CHANGES {
        assert_eq!(
            received[&("kept.a".to_string(), index)].len(),
            1,
            "batch {}: `kept.a`",
            index
        );
    }
}

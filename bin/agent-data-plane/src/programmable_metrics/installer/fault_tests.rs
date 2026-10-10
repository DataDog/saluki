//! Faults around installation, in a running plan-driven topology on the connected configuration path: a region whose
//! exit waits for a full downstream channel, a region that never acknowledges, a restarted installer worker, and many
//! installs in a row, after which the process must look as it did before them.

use std::collections::BTreeSet;

use http::{Request, StatusCode};
use http_body_util::Empty;
use hyper::body::Bytes;
use saluki_api::APIHandler as _;
use saluki_common::resource_tracking::ResourceGroupRegistry;
use saluki_core::{
    accounting::ComponentBounds,
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{Tag, TagSet},
    },
    health::HealthRegistry,
};
use saluki_metrics::test::TestRecorder;
use serde_json::{json, Value};
use tower::ServiceExt as _;

use super::*;
use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        test_support::{connected_configuration_system, update},
        test_topology::{Aggregation, Delivery, MetricsPath, Options, TestTopology, PATIENCE},
    },
};

/// A timestamped counter, which the no-aggregation pipeline sends past aggregation, tagged with its batch.
fn counter(name: &'static str, batch: usize) -> Metric {
    Metric::counter(
        Context::from_parts(name, TagSet::from_iter([Tag::from(format!("batch:{}", batch))])),
        (1_700_000_000 + batch as u64, 1.0),
    )
}

/// Returns how many metrics named `name` `output` received.
fn count(deliveries: &[Delivery], output: &str, name: &str) -> usize {
    deliveries
        .iter()
        .filter(|delivery| delivery.output == output)
        .flat_map(|delivery| &delivery.metrics)
        .filter(|metric| metric.context().name() == name)
        .count()
}

/// Returns the batches of the metrics named `name` that `output` received, sorted, repeats included.
fn batches(deliveries: &[Delivery], output: &str, name: &str) -> Vec<usize> {
    let mut batches = deliveries
        .iter()
        .filter(|delivery| delivery.output == output)
        .flat_map(|delivery| &delivery.metrics)
        .filter(|metric| metric.context().name() == name)
        .map(|metric| {
            metric
                .context()
                .tags()
                .into_iter()
                .find_map(|tag| tag.as_str().strip_prefix("batch:").and_then(|batch| batch.parse().ok()))
                .expect("every metric belongs to a batch")
        })
        .collect::<Vec<usize>>();
    batches.sort_unstable();
    batches
}

async fn status_where(status: &ArcSwap<PlanStatus>, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let current = status.load_full();
            if done(&current) {
                return PlanStatus::clone(&current);
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
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

fn record(status: &PlanStatus, generation: u64) -> Option<(GenerationState, Option<String>)> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .map(|record| (record.state, record.reason.clone()))
}

/// Starts the plan-driven topology of the default configuration on the connected configuration path, with `options`.
async fn start(
    options: Options,
) -> (
    TestTopology,
    tokio::sync::mpsc::Sender<saluki_config::dynamic::ConfigUpdate>,
) {
    let (system, updates, stream) = connected_configuration_system("").await;
    let topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    (topology, stream)
}

/// Changes the metric filterlist through the configuration stream.
async fn block(stream: &tokio::sync::mpsc::Sender<saluki_config::dynamic::ConfigUpdate>, names: &[&str]) {
    stream
        .send(update("metric_filterlist", json!(names)))
        .await
        .expect("the configuration stream is open");
}

#[tokio::test]
async fn a_preparation_completes_while_a_region_waits_to_send_to_a_full_downstream_channel() {
    // The main output reads nothing until its gate opens. More batches than its channel holds leave the enrichment
    // region waiting to send to it, with the rest queued in front of the region.
    const BATCHES: usize = 300;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.gated.insert("dd_metrics_encode".to_string());
    let (mut topology, stream) = start(options).await;
    let status = topology.status().expect("the plan-driven path has an installer");
    for batch in 0..BATCHES {
        topology.feed(
            "dsd_in.metrics",
            vec![counter("kept", batch), counter("blocked", batch)],
        );
    }
    // The client telemetry tap, fed by the same region ahead of the main output, stops receiving once the region
    // waits on the main output's full channel.
    topology
        .wait_for("the client telemetry tap to receive a channel's worth", |deliveries| {
            count(deliveries, "dsd_client_telemetry_out", "kept") > 128
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let stalled = count(
        topology.wait_for("nothing", |_| true).await,
        "dsd_client_telemetry_out",
        "kept",
    );
    assert!(stalled < BATCHES, "the region never waited: {} batches passed", stalled);

    // A filterlist change prepares and commits while the region waits: the region acknowledges between its attempts to
    // send. Generation 1 drains, since its batches are still in flight.
    block(&stream, &["blocked"]).await;
    let committed = status_where(&status, |status| active(status) == Some(2)).await;
    assert_eq!(
        record(&committed, 1).map(|(state, _)| state),
        Some(GenerationState::Draining)
    );
    assert_eq!(live(&committed), [1, 2]);
    assert_eq!(
        count(
            topology.wait_for("nothing", |_| true).await,
            "dsd_client_telemetry_out",
            "kept"
        ),
        stalled,
        "the region was still waiting"
    );

    // Once the main output reads, everything sent before the change arrives once, unfiltered, and what is sent after
    // it arrives filtered, under generation 2; generation 1 retires.
    topology.open_gate("dd_metrics_encode");
    topology.feed(
        "dsd_in.metrics",
        vec![counter("kept", BATCHES), counter("blocked", BATCHES)],
    );
    topology
        .wait_for("every batch", |deliveries| {
            count(deliveries, "dd_metrics_encode", "kept") == BATCHES + 1
        })
        .await;
    let retired = status_where(&status, |status| live(status) == [2]).await;
    assert_eq!(
        record(&retired, 1).map(|(state, _)| state),
        Some(GenerationState::Retired)
    );
    let deliveries = topology.stop().await;
    assert_eq!(
        batches(&deliveries, "dd_metrics_encode", "kept"),
        (0..=BATCHES).collect::<Vec<_>>()
    );
    assert_eq!(
        batches(&deliveries, "dd_metrics_encode", "blocked"),
        (0..BATCHES).collect::<Vec<_>>()
    );
    let last = deliveries
        .iter()
        .find(|delivery| batches(std::slice::from_ref(delivery), "dd_metrics_encode", "kept") == [BATCHES])
        .expect("the last batch arrived");
    assert_eq!(last.generation, Some(2));
}

#[tokio::test(start_paused = true)]
async fn a_region_that_never_acknowledges_times_out_and_its_unread_preparations_stay_within_the_overlap_budget() {
    // The enrichment region runs with a mailbox of its own, and the installer's sender for it reaches a mailbox that
    // nobody reads: the region never acknowledges a preparation, as a region that stopped reading control messages.
    let unread = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.before_topology = Some(Box::new({
        let unread = std::sync::Arc::clone(&unread);
        move |boot: &mut BootTopology| {
            let slot = boot
                .frame
                .slots()
                .iter()
                .position(|slot| slot.name.as_str() == "metrics_enrich")
                .expect("the frame has an enrichment region");
            let (sender, mailbox) = control_mailbox::<RegionControl>();
            let original = boot.mailboxes[slot].replace(mailbox);
            // The unread mailbox stands for a region that runs, so the installer starts sending it preparations.
            if let Some(original) = &original {
                original.mark_running();
            }
            *unread.lock().unwrap() = Some((sender, original));
        }
    }));
    let (mut topology, stream) = start(options).await;
    let status = topology.status().expect("the plan-driven path has an installer");
    topology.feed("dsd_in.metrics", vec![counter("kept", 0)]);
    topology
        .wait_for("the first batch", |deliveries| {
            count(deliveries, "dd_metrics_encode", "kept") == 1
        })
        .await;

    // The preparation times out after 10 seconds and is aborted in every region, and generation 1 keeps running.
    block(&stream, &["blocked"]).await;
    let aborted = status_where(&status, |status| {
        record(status, 2).is_some_and(|(state, _)| state.is_final())
    })
    .await;
    assert_eq!(
        record(&aborted, 2),
        Some((
            GenerationState::Rejected,
            Some("the regions `metrics_enrich` did not acknowledge within 10s".to_string())
        ))
    );
    assert_eq!(active(&aborted), Some(1));
    topology.feed("dsd_in.metrics", vec![counter("kept", 1), counter("blocked", 1)]);
    let deliveries = topology
        .wait_for("the batch after the abort", |deliveries| {
            count(deliveries, "dd_metrics_encode", "blocked") == 1
        })
        .await;
    let after = deliveries
        .iter()
        .rev()
        .find(|delivery| delivery.output == "dd_metrics_encode")
        .expect("the main output received the batch");
    assert_eq!(after.generation, Some(1));

    // The candidate is tried again with backoff. A preparation waiting in the unread mailbox keeps its generation
    // alive, since the message holds it until the region takes it: what an unresponsive region accumulates is bounded
    // by the overlap budget, at which point the candidate waits instead of being prepared again.
    let budget = InstallerConfig::default().overlap_budget;
    let full = status_where(&status, |status| {
        status.installer.live.len() == budget && status.installer.preparing.is_none()
    })
    .await;
    assert_eq!(live(&full), [1, 2, 3, 4]);
    for generation in 2..=4 {
        assert_eq!(
            record(&full, generation).map(|(state, _)| state),
            Some(GenerationState::Rejected)
        );
    }
    assert!(full.installer.pending.is_some(), "{}", full);
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    let waiting = status.load_full();
    assert_eq!(live(&waiting), [1, 2, 3, 4], "{}", waiting);
    assert!(waiting.installer.preparing.is_none(), "{}", waiting);

    // Once the mailbox is gone, its preparations go with it, and their generations retire.
    drop(unread.lock().unwrap().take());
    let released = status_where(&status, |status| {
        live(status).iter().all(|generation| !(2..=4).contains(generation))
    })
    .await;
    assert_eq!(active(&released), Some(1));
    drop(topology.stop().await);
}

#[tokio::test(start_paused = true)]
async fn a_change_made_before_the_topology_runs_is_installed_once_it_runs_without_timing_out() {
    // The topology is built, and waits a minute for its environment before it starts its components, as `run.rs`
    // waits for the environment provider; the filterlist changes meanwhile.
    let (ready, waiting) = tokio::sync::oneshot::channel::<()>();
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.environment_ready = Some(Box::pin(async move {
        let _ = waiting.await;
    }));
    let (mut topology, stream) = start(options).await;
    let status = topology.status().expect("the plan-driven path has an installer");
    block(&stream, &["blocked"]).await;
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;

    // Nothing was prepared for regions that do not read their mailboxes yet: no preparation timed out, and nothing
    // waits in their mailboxes.
    let waited = status.load_full();
    assert_eq!(live(&waited), [1], "{}", waited);
    assert!(waited.installer.preparing.is_none(), "{}", waited);
    assert_eq!(waited.last_error, None);
    assert!(
        waited.installer.generations.iter().all(|record| record.id.get() == 1),
        "{}",
        waited
    );

    // Once the topology runs, the change is installed by its first preparation, and applies from the first batch.
    ready.send(()).expect("the topology waits for its environment");
    let installed = status_where(&status, |status| active(status) == Some(2)).await;
    assert_eq!(installed.config_changes, 1);
    assert!(
        installed
            .installer
            .generations
            .iter()
            .all(|record| !matches!(record.state, GenerationState::Rejected)),
        "{}",
        installed
    );
    topology.feed("dsd_in.metrics", vec![counter("kept", 0), counter("blocked", 0)]);
    let deliveries = topology
        .wait_for("the first batch", |deliveries| {
            count(deliveries, "dd_metrics_encode", "kept") == 1
        })
        .await;
    assert_eq!(count(deliveries, "dd_metrics_encode", "blocked"), 0);
    drop(topology.stop().await);
}

/// Runs the installer worker as a supervisor would, until `shutdown` triggers or the task is aborted.
fn run_installer(
    installer: &std::sync::Arc<InstallerWorker>, shutdown: ShutdownHandle,
) -> impl std::future::Future<Output = ()> {
    let installer = std::sync::Arc::clone(installer);
    async move {
        let run = match installer.initialize(shutdown).await {
            Ok(run) => run,
            Err(_) => panic!("the installer initializes"),
        };
        run.await.expect("the installer stops cleanly");
    }
}

#[tokio::test]
async fn a_restarted_installer_worker_keeps_its_state_and_installs_the_next_change() {
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.supervise_installer = false;
    let (mut topology, stream) = start(options).await;
    let status = topology.status().expect("the plan-driven path has an installer");
    let installer = std::sync::Arc::new(topology.take_installer());

    // The first run installs a change, then stops abruptly, as a panicking worker does.
    let first = tokio::spawn(run_installer(&installer, ShutdownHandle::noop()));
    block(&stream, &["blocked.a"]).await;
    status_where(&status, |status| active(status) == Some(2) && live(status) == [2]).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    // The restarted worker continues from the state the first one left: the next change becomes generation 3, the
    // configuration changes keep counting, and the regions install it.
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let second = tokio::spawn(run_installer(&installer, shutdown));
    block(&stream, &["blocked.b"]).await;
    let installed = status_where(&status, |status| active(status) == Some(3) && live(status) == [3]).await;
    assert_eq!(installed.config_changes, 2);
    assert_eq!(
        record(&installed, 2).map(|(state, _)| state),
        Some(GenerationState::Retired)
    );
    topology.feed(
        "dsd_in.metrics",
        vec![counter("blocked.a", 0), counter("blocked.b", 0), counter("kept", 0)],
    );
    let deliveries = topology
        .wait_for("the batch", |deliveries| {
            count(deliveries, "dd_metrics_encode", "kept") == 1
        })
        .await;
    assert_eq!(count(deliveries, "dd_metrics_encode", "blocked.a"), 1);
    assert_eq!(count(deliveries, "dd_metrics_encode", "blocked.b"), 0);

    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, second).await.unwrap().unwrap();
    drop(topology.stop().await);
}

/// Returns the components of a health registry snapshot, with whether each is live and ready.
pub(super) fn health_components(health: &HealthRegistry) -> BTreeSet<(String, bool, bool)> {
    let snapshot = serde_json::from_str::<Value>(&health.snapshot_json()).expect("the snapshot is JSON");
    snapshot
        .as_object()
        .expect("the snapshot is an object")
        .iter()
        .map(|(component, state)| (component.clone(), state["live"] == true, state["ready"] == true))
        .collect()
}

/// Returns the names of every resource group registered in the process.
pub(super) fn resource_groups() -> BTreeSet<String> {
    let mut groups = BTreeSet::new();
    ResourceGroupRegistry::global().visit_resource_groups(|name, _| {
        groups.insert(name.to_string());
    });
    groups
}

/// Returns the path of every component in a memory bounds tree, with its minimum and firm bytes.
pub(super) fn bounds_tree(bounds: &ComponentBounds) -> BTreeSet<(String, usize, usize)> {
    fn visit(prefix: &str, bounds: &ComponentBounds, into: &mut BTreeSet<(String, usize, usize)>) {
        into.insert((
            prefix.to_string(),
            bounds.total_minimum_required_bytes(),
            bounds.total_firm_limit_bytes(),
        ));
        for (name, child) in bounds.subcomponents() {
            visit(&format!("{}/{}", prefix, name), child, into);
        }
    }
    let mut tree = BTreeSet::new();
    visit("", bounds, &mut tree);
    tree
}

/// Returns every metric series registered with `recorder`.
pub(super) fn series(recorder: &TestRecorder) -> BTreeSet<String> {
    let counters = recorder
        .counters()
        .into_iter()
        .map(|(key, _)| format!("counter {:?}", key));
    let gauges = recorder.gauges().into_iter().map(|(key, _)| format!("gauge {:?}", key));
    let histograms = recorder
        .histogram_keys()
        .into_iter()
        .map(|key| format!("histogram {:?}", key));
    counters.chain(gauges).chain(histograms).collect()
}

/// Returns the status `GET /ready` answers.
pub(super) async fn ready(health: &HealthRegistry) -> StatusCode {
    let handler = health.api_handler();
    let request = Request::builder().uri("/ready").body(Empty::<Bytes>::new()).unwrap();
    handler
        .generate_routes()
        .with_state(handler.generate_initial_state())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

/// Waits until the components of the topology, the regions among them, are registered, `GET /ready` answers
/// `200 OK`, and every component is live.
pub(super) async fn wait_until_healthy(health: &HealthRegistry) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let components = health_components(health);
            let registered = ["dsd_pre_aggregation", "dsd_agg", "metrics_enrich"]
                .iter()
                .all(|region| {
                    components
                        .iter()
                        .any(|(component, _, _)| *component == format!("topology.primary.transforms.{}", region))
                });
            if registered
                && ready(health).await == StatusCode::OK
                && components.iter().all(|(_, live, ready)| *live && *ready)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the topology never became healthy: {}", health.snapshot_json()));
}

#[tokio::test]
async fn repeated_installs_leave_health_registrations_resource_groups_bounds_and_metric_series_as_they_were() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let options = Options::new(MetricsPath::Plan, Aggregation::Native);
    let (health, registry) = (options.health.clone(), options.registry.clone());
    let (mut topology, stream) = start(options).await;
    let status = topology.status().expect("the plan-driven path has an installer");
    wait_until_healthy(&health).await;
    topology.feed("dsd_in.metrics", vec![counter("kept", 0)]);
    topology
        .wait_for("the first batch", |deliveries| {
            count(deliveries, "dd_metrics_encode", "kept") == 1
        })
        .await;
    let registrations = |health: &HealthRegistry| {
        health_components(health)
            .into_iter()
            .map(|(component, _, ready)| (component, ready))
            .collect::<BTreeSet<_>>()
    };
    let before = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );

    // Twenty installs, each with a batch through the regions, each committed and its predecessor retired before the
    // next.
    const INSTALLS: usize = 20;
    for install in 1..=INSTALLS {
        let blocked = format!("blocked.{}", install);
        block(&stream, &[blocked.as_str()]).await;
        let generation = install as u64 + 1;
        status_where(&status, |status| {
            active(status) == Some(generation) && live(status) == [generation]
        })
        .await;
        topology.feed("dsd_in.metrics", vec![counter("kept", install)]);
        topology
            .wait_for("the batch", |deliveries| {
                count(deliveries, "dd_metrics_encode", "kept") == install + 1
            })
            .await;
    }
    wait_until_healthy(&health).await;
    let after = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );
    assert_eq!(after.0, before.0, "health registrations");
    assert_eq!(after.1, before.1, "resource groups");
    assert_eq!(after.2, before.2, "memory bounds");
    assert_eq!(
        (
            after.3.difference(&before.3).collect::<Vec<_>>(),
            before.3.difference(&after.3).collect::<Vec<_>>()
        ),
        (Vec::<&String>::new(), Vec::<&String>::new()),
        "metric series added and removed"
    );
    assert_eq!(ready(&health).await, StatusCode::OK);
    assert_eq!(
        recorder.gauge(metrics::Key::from_name(
            crate::programmable_metrics::telemetry::PLAN_LIVE_GENERATIONS
        )),
        Some(1.0)
    );
    drop(topology.stop().await);
}

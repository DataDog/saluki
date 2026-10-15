//! The experimental standalone configuration reload (`experimental.standalone_config_reload`) end to end: the
//! plan-driven topology in standalone mode, with real encoders and recipients sending to recording intakes
//! ([`test_intake`](super::test_intake)), while the test rewrites the configuration file the reload re-reads.
//!
//! These are the file-driven counterparts of the live endpoint changes in `delivery_tests`, which come through the
//! connected-mode configuration stream instead: a change the reload accepts reaches the installer the same way.

use std::{collections::BTreeMap, time::Duration};

use saluki_core::data_model::{
    event::metric::{context::Context as MetricContext, Metric},
    tags::{Tag, TagSet},
};

use super::{
    installer::PlanStatus,
    test_intake::{Context, Intake, PATIENCE, ZSTD},
    test_support::{reloading_configuration_system, ReloadedFile},
    test_topology::{Aggregation, Encoders, MetricsPath, Options, Recipients, TestTopology},
};
use crate::internal::ConfigUpdatesWorker;

/// What every configuration adds, besides [`ZSTD`]: the batcher sends what it holds at once.
const FLUSH: &str = "aggregate_passthrough_idle_flush_timeout: 0s";

/// The name of every metric the tests feed.
const NAME: &str = "kept.a";

/// Long enough for the reload, which re-reads the file every 100 milliseconds and takes a change once two reads agree,
/// to have read it several times.
const SEVERAL_REREADS: Duration = Duration::from_millis(800);

/// Returns batch `index`: a timestamped counter named [`NAME`], tagged with the batch.
fn batch(index: usize) -> Vec<Metric> {
    let tags = TagSet::from_iter([Tag::from(format!("batch:{}", index))]);
    vec![Metric::counter(
        MetricContext::from_parts(NAME.to_string(), tags),
        (1_700_000_000 + index as u64, 1.0),
    )]
}

/// Returns how many times `intake` received each batch, of the first `sent`.
fn batches_received(intake: &Intake, sent: usize) -> BTreeMap<usize, usize> {
    let points = intake.points();
    (0..sent)
        .filter_map(|index| {
            let context: Context = (NAME.to_string(), vec![format!("batch:{}", index)]);
            points.get(&context).map(|count| (index, *count))
        })
        .collect()
}

/// Returns the configuration layers that send to `primary` and to each of `additional`, with the API key `key-<n>` for
/// the `n`th.
fn endpoints(primary: &Intake, additional: &[&Intake]) -> String {
    let additional = additional
        .iter()
        .enumerate()
        .map(|(n, intake)| format!("\"{}\": [key-{}]", intake.url(), n))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{{}}}",
        primary.url(),
        additional
    )
}

/// Starts the plan-driven topology of `layers` in standalone mode with the configuration reload on, with real encoders
/// and sending recipients, and returns it with the file the reload re-reads.
async fn start_reloading(layers: &[&str]) -> (TestTopology, ReloadedFile) {
    let (system, updates, file) = reloading_configuration_system(layers).await;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.encoders = Encoders::Real;
    options.recipients = Recipients::Sending;
    let topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    (topology, file)
}

/// Feeds a batch every few milliseconds, from `next` on, until `done` holds, then `after` batches more; returns the
/// next batch's index.
async fn feed_until(
    topology: &TestTopology, mut next: usize, what: &str, after: usize, done: impl Fn(&PlanStatus) -> bool,
) -> usize {
    let status = topology.status().expect("the plan-driven path has an installer");
    let waited = tokio::time::timeout(PATIENCE, async {
        while !done(&status.load()) {
            topology.feed("dsd_in.metrics", batch(next));
            next += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{} never happened: {}", what, status.load());
    feed_for(topology, next, after).await
}

/// Feeds `count` batches, from `next` on, a few milliseconds apart; returns the next batch's index.
async fn feed_for(topology: &TestTopology, next: usize, count: usize) -> usize {
    for index in next..next + count {
        topology.feed("dsd_in.metrics", batch(index));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    next + count
}

/// Feeds a batch every few milliseconds, from `next` on, until `intake` received one; returns the next batch's index.
async fn feed_until_received(topology: &TestTopology, mut next: usize, intake: &Intake, what: &str) -> usize {
    let waited = tokio::time::timeout(PATIENCE, async {
        while intake.points().is_empty() {
            topology.feed("dsd_in.metrics", batch(next));
            next += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "{} never received a batch: {}",
        what,
        topology.status().unwrap().load()
    );
    next
}

/// Whether the recipient of the main output's delivery host that a removed endpoint had has retired.
fn retired(status: &PlanStatus) -> bool {
    status
        .recipients
        .iter()
        .any(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_added_to_the_reloaded_file_becomes_a_live_recipient() {
    let primary = Intake::start().await;
    let added = Intake::start().await;
    let (topology, file) = start_reloading(&[FLUSH, ZSTD, &endpoints(&primary, &[])]).await;

    // The boot generation sends to the primary endpoint alone.
    let next = feed_for(&topology, 0, 5).await;
    primary
        .wait_for("the boot batches", |primary| {
            batches_received(primary, next).len() == next
        })
        .await;
    assert!(added.points().is_empty());

    // The file gains an endpoint, which the reload applies: its recipient starts receiving, with its own API key.
    file.rewrite(&[FLUSH, ZSTD, &endpoints(&primary, &[&added])]);
    let next = feed_until_received(&topology, next, &added, "the added endpoint").await;
    let sent = feed_for(&topology, next, 10).await;
    added
        .wait_for("the batches after the edit", |added| {
            batches_received(added, sent).contains_key(&(sent - 1))
        })
        .await;
    primary
        .wait_for("every batch", |primary| batches_received(primary, sent).len() == sent)
        .await;
    let _ = topology.stop().await;

    assert!(added
        .requests()
        .iter()
        .all(|request| request.api_key.as_deref() == Some("key-0")));
    // The added endpoint receives a contiguous run of batches, each once, up to the last one sent; the primary
    // endpoint, every batch once.
    let received = batches_received(&added, sent);
    let first = *received.keys().next().unwrap();
    assert!(first >= 5, "the added endpoint received a boot batch: {:?}", received);
    assert_eq!(
        received,
        (first..sent).map(|index| (index, 1)).collect::<BTreeMap<_, _>>()
    );
    assert_eq!(
        batches_received(&primary, sent),
        (0..sent).map(|index| (index, 1)).collect::<BTreeMap<_, _>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_removed_from_the_reloaded_file_drains_and_retires() {
    let primary = Intake::start().await;
    let removed = Intake::start().await;
    let (topology, file) = start_reloading(&[FLUSH, ZSTD, &endpoints(&primary, &[&removed])]).await;

    let next = feed_until_received(&topology, 0, &removed, "the endpoint to remove").await;
    let next = feed_for(&topology, next, 5).await;

    // The file loses the endpoint, which the reload applies: its recipient drains what it holds, and retires.
    file.rewrite(&[FLUSH, ZSTD, &endpoints(&primary, &[])]);
    let sent = feed_until(&topology, next, "the removed endpoint's retirement", 5, retired).await;
    primary
        .wait_for("every batch", |primary| batches_received(primary, sent).len() == sent)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let status = topology.status().unwrap().load_full();
    let _ = topology.stop().await;

    // The removed endpoint received every batch from the first up to the edit that withdrew it, once each, and
    // nothing after; its drain delivered what it held.
    let received = batches_received(&removed, sent);
    let last = *received.keys().next_back().unwrap();
    assert_eq!(received, (0..=last).map(|index| (index, 1)).collect::<BTreeMap<_, _>>());
    assert!(last < sent - 5, "{} of {}", last, sent);
    let drain = status
        .recipients
        .iter()
        .find_map(|entry| (entry.host == "dd_out").then_some(entry.last_drain).flatten())
        .expect("the removed recipient retired");
    assert!(!drain.deadline_reached, "{:?}", drain);
    assert_eq!((drain.persisted, drain.dropped), (0, 0), "{:?}", drain);
    assert_eq!(
        batches_received(&primary, sent),
        (0..sent).map(|index| (index, 1)).collect::<BTreeMap<_, _>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_reloaded_file_is_rejected_and_the_running_configuration_stays() {
    let primary = Intake::start().await;
    let kept = Intake::start().await;
    let (topology, file) = start_reloading(&[FLUSH, ZSTD, &endpoints(&primary, &[&kept])]).await;
    let status = topology.status().unwrap();

    let next = feed_until_received(&topology, 0, &kept, "the endpoint").await;
    let changes = status.load().config_changes;
    let generation = status.load().active.as_ref().map(|active| active.generation);

    // Each invalid file, which would also remove the endpoint, is rejected as a whole: YAML that does not parse, a
    // value that fails translation, and a configuration that fails validation (no API key). The running configuration
    // stays: the installer observes no change, and the endpoint keeps receiving.
    let removing = endpoints(&primary, &[]);
    let mut next = next;
    for invalid in [
        format!(
            "{}dogstatsd_port: [unterminated\n",
            ReloadedFile::rendered(&[FLUSH, ZSTD, &removing])
        ),
        ReloadedFile::rendered(&[FLUSH, ZSTD, &removing, "dogstatsd_tag_cardinality: bogus"]),
        ReloadedFile::rendered(&[FLUSH, ZSTD, &removing, "api_key: ''"]),
    ] {
        file.write(&invalid);
        let before = batches_received(&kept, next).len();
        let deadline = tokio::time::Instant::now() + SEVERAL_REREADS;
        while tokio::time::Instant::now() < deadline {
            next = feed_for(&topology, next, 1).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        kept.wait_for("batches after the invalid file", |kept| {
            batches_received(kept, next).len() > before
        })
        .await;
        let after = status.load();
        assert_eq!(after.config_changes, changes, "{}", invalid);
        assert_eq!(
            after.active.as_ref().map(|active| active.generation),
            generation,
            "{}",
            invalid
        );
        assert!(!retired(&after), "{}", invalid);
    }

    // A later valid edit applies: the endpoint it removes retires.
    file.rewrite(&[FLUSH, ZSTD, &removing]);
    let sent = feed_until(&topology, next, "the removed endpoint's retirement", 5, retired).await;
    primary
        .wait_for("every batch", |primary| batches_received(primary, sent).len() == sent)
        .await;
    let _ = topology.stop().await;
    assert!(status.load().config_changes > changes);
    assert_eq!(
        batches_received(&primary, sent),
        (0..sent).map(|index| (index, 1)).collect::<BTreeMap<_, _>>()
    );
}

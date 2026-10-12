//! The encode hosts and delivery hosts of the plan-driven topology end to end: real encoders, and recipients sending
//! to recording intakes ([`test_intake`](super::test_intake)), against the native encoders and forwarders of the legacy
//! topology, and through live configuration changes.
//!
//! Every test's input is timestamped counters, which the no-aggregation pipeline sends past aggregation as they
//! arrive, so each batch reaches the intakes while the topology runs, tagged with the batch it belongs to; and every
//! configuration sets `aggregate_passthrough_idle_flush_timeout` to zero, so that the batcher of the legacy topology
//! sends them at once too.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use saluki_core::data_model::{
    event::{
        eventd::EventD,
        log::Log,
        metric::{context::Context as MetricContext, Metric},
        trace::{Span, Trace},
        Event,
    },
    tags::{Tag, TagSet},
};
use serde_json::json;

use super::{
    installer::PlanStatus,
    test_intake::{Context, Intake, PATIENCE, ZSTD},
    test_support::{connected_configuration_system, layered_configuration_system, update},
    test_topology::{Aggregation, Delivery, Encoders, MetricsPath, Options, Recipients, TestTopology},
};
use crate::internal::ConfigUpdatesWorker;

/// What every configuration adds, besides [`ZSTD`]: the native batcher sends what it holds at once.
const FLUSH: &str = "aggregate_passthrough_idle_flush_timeout: 0s";

/// The OTLP source beside DogStatsD, whose logs reach `dd_out` through the logs encoder.
const OTLP: &str = "data_plane: {otlp: {enabled: true}}";

/// The API key of the primary endpoint, from the tests' base configuration.
const PRIMARY_KEY: &str = "lowering-primary-key";

/// Returns batch `index`: a timestamped counter of every name in `names`, each tagged with the batch.
fn batch(index: usize, names: &[&str]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| {
            let tags = TagSet::from_iter([Tag::from(format!("batch:{}", index))]);
            Metric::counter(
                MetricContext::from_parts(name.to_string(), tags),
                (1_700_000_000 + index as u64, 1.0),
            )
        })
        .collect()
}

/// Returns the context a metric of [`batch`] `index` named `name` reaches an intake with.
fn context(name: &str, index: usize) -> Context {
    (name.to_string(), vec![format!("batch:{}", index)])
}

/// Returns the event of batch `index`, whose title names it.
fn event(index: usize) -> Event {
    Event::EventD(EventD::new(format!("event-{}-end", index), "a test event"))
}

/// Returns how many times event `index` reached `intake`.
fn events_received(intake: &Intake, index: usize) -> usize {
    intake.occurrences("/api/v1/events_batch", &format!("event-{}-end", index))
}

/// Returns the points `intake` received of each context, by the API key that carried them.
fn points_by_key(intake: &Intake) -> BTreeMap<(String, Context), usize> {
    let mut points = BTreeMap::new();
    for request in intake.requests() {
        for (context, count) in request.points {
            *points
                .entry((request.api_key.clone().unwrap_or_default(), context))
                .or_default() += count;
        }
    }
    points
}

/// Returns the configuration `template` with each intake's placeholder (`{P}`, `{B}`, and so on, by the intakes' order
/// in `names`) replaced by its URL.
fn configured(template: &str, names: &[&str], intakes: &[Intake]) -> String {
    names
        .iter()
        .zip(intakes)
        .fold(template.to_string(), |config, (name, intake)| {
            config.replace(&format!("{{{}}}", name), &intake.url())
        })
}

/// One configuration of the differential: its name, its template, the intakes it names, and, written by hand, the
/// metric names each intake receives, with the API key each copy carries.
struct Case {
    name: &'static str,
    template: &'static str,
    intakes: &'static [&'static str],
    expected: &'static [(&'static str, &'static str, &'static [&'static str])],
}

/// The metric names every batch of the differential carries.
const NAMES: [&str; 4] = ["orders.total", "catalog.items", "raw.requests", "other.metric"];

/// The configurations of the differential: additional endpoints, endpoint routing groups (two endpoints sharing one,
/// and one with empty lists, which has none and receives no metric), and Multi-Region Failover mirroring an allowlist.
const CASES: [Case; 3] = [
    Case {
        name: "an additional endpoint with two keys",
        template: r#"
dd_url: "{P}"
additional_endpoints: {"{B}": [key-b1, key-b2]}
"#,
        intakes: &["P", "B"],
        expected: &[
            ("P", PRIMARY_KEY, &NAMES),
            ("B", "key-b1", &NAMES),
            ("B", "key-b2", &NAMES),
        ],
    },
    Case {
        name: "two routing groups, a shared group, and an endpoint with empty lists",
        template: r#"
dd_url: "{P}"
additional_endpoints:
  "{B}": [key-b]
  "{C}": [key-c]
  "{D}": [key-d]
  "{E}": [key-e]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      "{B}": [orders.total]
      "{C}": [orders.total]
      "{D}": []
    metric_prefix_allowlist:
      "{E}": [catalog.]
"#,
        intakes: &["P", "B", "C", "D", "E"],
        expected: &[
            ("P", PRIMARY_KEY, &NAMES),
            ("B", "key-b", &["orders.total"]),
            ("C", "key-c", &["orders.total"]),
            ("D", "key-d", &[]),
            ("E", "key-e", &["catalog.items"]),
        ],
    },
    Case {
        name: "multi-region failover mirroring an allowlist",
        template: r#"
dd_url: "{P}"
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: "{M}"
  failover_metrics: true
  metric_allowlist: [orders.total]
"#,
        intakes: &["P", "M"],
        expected: &[("P", PRIMARY_KEY, &NAMES), ("M", "mrf-key", &["orders.total"])],
    },
];

/// How many batches the differential sends.
const BATCHES: usize = 20;

/// Runs one path of a differential case: feeds every batch, waits until every intake received what the case expects,
/// and returns what each intake received, by API key, once nothing more arrives.
async fn deliver(case: &Case, path: MetricsPath) -> Vec<BTreeMap<(String, Context), usize>> {
    let mut intakes = Vec::new();
    for _ in case.intakes {
        intakes.push(Intake::start().await);
    }
    let config = configured(case.template, case.intakes, &intakes);
    let system = layered_configuration_system(&[FLUSH, ZSTD, &config]).await;
    let mut options = Options::new(path, Aggregation::Native);
    options.encoders = Encoders::Real;
    options.recipients = Recipients::Sending;
    let topology = TestTopology::start(&system, options, |_| {}).await;
    for index in 0..BATCHES {
        topology.feed("dsd_in.metrics", batch(index, &NAMES));
    }
    let expected = case
        .intakes
        .iter()
        .map(|intake| {
            case.expected
                .iter()
                .filter(|(name, ..)| name == intake)
                .flat_map(|(_, key, names)| {
                    names.iter().flat_map(move |name| {
                        (0..BATCHES).map(move |index| ((key.to_string(), context(name, index)), 1))
                    })
                })
                .collect::<BTreeMap<_, _>>()
        })
        .collect::<Vec<_>>();
    for ((name, intake), expected) in case.intakes.iter().zip(&intakes).zip(&expected) {
        intake
            .wait_for(&format!("{}: {:?}: `{}`", case.name, path, name), |intake| {
                points_by_key(intake).len() >= expected.len()
            })
            .await;
    }
    // Anything sent twice arrives within the time a request takes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    topology.stop().await;
    intakes.iter().map(points_by_key).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_recipient_receives_what_the_legacy_topology_sends_it() {
    for case in &CASES {
        let legacy = deliver(case, MetricsPath::Legacy).await;
        let plan = deliver(case, MetricsPath::Plan).await;
        for ((intake, legacy), plan) in case.intakes.iter().zip(&legacy).zip(&plan) {
            let expected = case
                .expected
                .iter()
                .filter(|(name, ..)| name == intake)
                .flat_map(|(_, key, names)| {
                    names.iter().flat_map(move |name| {
                        (0..BATCHES).map(move |index| ((key.to_string(), context(name, index)), 1))
                    })
                })
                .collect::<BTreeMap<_, _>>();
            assert_eq!(*legacy, expected, "{}: what `{}` received natively", case.name, intake);
            assert_eq!(
                *plan, expected,
                "{}: what `{}` received through its host",
                case.name, intake
            );
        }
    }
}

/// Starts the plan-driven topology of `layers` on the connected configuration path, with real encoders and sending
/// recipients, and returns it with the configuration stream.
async fn start_connected(
    layers: &[&str],
) -> (
    TestTopology,
    tokio::sync::mpsc::Sender<saluki_config::dynamic::ConfigUpdate>,
) {
    let (system, updates, stream) = connected_configuration_system(&layers.join("\n")).await;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.encoders = Encoders::Real;
    options.recipients = Recipients::Sending;
    options.real_trace_sampler = true;
    let topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    (topology, stream)
}

/// Feeds batch `index` of `names`, its event, and, when the OTLP source runs, its log and its trace; returns the next
/// batch's index.
fn feed(topology: &TestTopology, index: usize, names: &[&str]) -> usize {
    topology.feed("dsd_in.metrics", batch(index, names));
    topology.feed_events("dsd_in.events", vec![event(index)]);
    if topology.has_feed("otlp_in.logs") {
        topology.feed_events("otlp_in.logs", vec![Event::Log(Log::new(format!("log-{}-end", index)))]);
        topology.feed_events("otlp_in.traces", vec![trace(index)]);
    }
    index + 1
}

/// Returns the trace of batch `index`: one span, whose name names the batch, which the sampler keeps (a user's keep).
fn trace(index: usize) -> Event {
    let span = Span::new(
        "checkout",
        format!("trace-{}-end", index),
        "GET /cart",
        "web",
        index as u64 + 1,
        0,
        1_700_000_000_000_000_000 + index as u64,
        1_000,
        0,
    );
    let mut trace = Trace::new(vec![span]);
    trace.trace_id_low = index as u64 + 1;
    trace.priority = Some(2);
    Event::Trace(trace)
}

/// Returns how many times the trace of batch `index` reached `intake`.
fn traces_received(intake: &Intake, index: usize) -> usize {
    intake.occurrences("/api/v0.2/traces", &format!("trace-{}-end", index))
}

/// Returns how many times the log of batch `index` reached `intake`.
fn logs_received(intake: &Intake, index: usize) -> usize {
    intake.occurrences("/api/v2/logs", &format!("log-{}-end", index))
}

/// Feeds a batch every few milliseconds, from `next` on, until `done` holds of the installer's status, then `after`
/// batches more; returns the next batch's index.
async fn feed_until(
    topology: &TestTopology, mut next: usize, names: &[&str], what: &str, after: usize,
    done: impl Fn(&PlanStatus) -> bool,
) -> usize {
    let status = topology.status().expect("the plan-driven path has an installer");
    let waited = tokio::time::timeout(PATIENCE, async {
        while !done(&status.load()) {
            next = feed(topology, next, names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "{} never happened: {}\n{}\n{:#?}",
        what,
        status.load(),
        serde_json::to_string_pretty(&status.load().resources.hosts).unwrap(),
        status.load().recipients
    );
    for _ in 0..after {
        next = feed(topology, next, names);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    next
}

/// Returns the generation that processed each batch the main output's encode host received, by batch, from what its
/// observer saw; metrics of no batch (the trace sampler's own) are left out.
fn generations(deliveries: &[Delivery]) -> BTreeMap<usize, BTreeSet<u64>> {
    let mut generations = BTreeMap::<usize, BTreeSet<u64>>::new();
    for delivery in deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode")
    {
        for metric in &delivery.metrics {
            let batch = metric
                .context()
                .tags()
                .into_iter()
                .find_map(|tag| tag.as_str().strip_prefix("batch:").and_then(|index| index.parse().ok()));
            if let Some(index) = batch {
                generations.entry(index).or_default().extend(delivery.generation);
            }
        }
    }
    generations
}

/// Returns the batches of which `intake` received the metric `name`, each with how many times, under any key.
fn batches_received(intake: &Intake, name: &str, sent: usize) -> BTreeMap<usize, usize> {
    let points = intake.points();
    (0..sent)
        .filter_map(|index| points.get(&context(name, index)).map(|count| (index, *count)))
        .collect()
}

/// Asserts that `received` holds each index of one contiguous range once, and returns the range.
#[track_caller]
fn contiguous_once(what: &str, received: &BTreeMap<usize, usize>) -> std::ops::Range<usize> {
    let (Some(first), Some(last)) = (received.keys().next(), received.keys().next_back()) else {
        panic!("{} received nothing", what);
    };
    let range = *first..*last + 1;
    for index in range.clone() {
        assert_eq!(
            received.get(&index),
            Some(&1),
            "{}: batch {} in {:?}",
            what,
            index,
            received
        );
    }
    range
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_added_and_removed_live_receives_the_metrics_events_logs_and_traces_routed_to_it_once_each() {
    let primary = Intake::start().await;
    let added = Intake::start().await;
    let primary_config = format!("dd_url: \"{}\"", primary.url());
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, OTLP, &primary_config]).await;
    let names = ["kept.a"];
    let added_endpoint = added.url();

    // The boot generation sends to the primary endpoint alone. The Core Agent adds an endpoint while batches flow;
    // once it has received something, more batches follow, then the Core Agent removes it again, and batches flow
    // until its recipient retired.
    let mut next = 0;
    for _ in 0..5 {
        next = feed(&topology, next, &names);
    }
    stream
        .send(update(
            "additional_endpoints",
            json!({ added_endpoint.clone(): ["key-added"] }),
        ))
        .await
        .unwrap();
    let added_ref = &added;
    let status = topology.status().unwrap();
    let waited = tokio::time::timeout(PATIENCE, async {
        while added_ref.points().is_empty() || added_ref.occurrences("/api/v1/events_batch", "event-") == 0 {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "the added endpoint never received a batch: {}",
        status.load()
    );
    for _ in 0..10 {
        next = feed(&topology, next, &names);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stream.send(update("additional_endpoints", json!({}))).await.unwrap();
    let next = feed_until(
        &topology,
        next,
        &names,
        "the removed endpoint's retirement",
        5,
        |status| {
            status
                .recipients
                .iter()
                .any(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
        },
    )
    .await;
    let sent = next;

    // Every batch reaches the primary endpoint once, its metrics, its event, its log, and its trace.
    primary
        .wait_for("every batch", |primary| {
            batches_received(primary, "kept.a", sent).len() == sent
                && (0..sent).all(|index| {
                    events_received(primary, index) > 0
                        && logs_received(primary, index) > 0
                        && traces_received(primary, index) > 0
                })
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let final_status = topology.status().unwrap().load_full();
    let deliveries = topology.stop().await;
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", sent)),
        0..sent
    );
    for index in 0..sent {
        assert_eq!(
            events_received(&primary, index),
            1,
            "the primary endpoint: event {}",
            index
        );
        assert_eq!(logs_received(&primary, index), 1, "the primary endpoint: log {}", index);
        assert_eq!(
            traces_received(&primary, index),
            1,
            "the primary endpoint: trace {}",
            index
        );
    }

    // The added endpoint received, once each, a contiguous range of batches after the first and before the last:
    // from the first batch of the generation that bound it to the main output, which commits after the resource
    // transaction that added its recipient published, to the resource transaction that withdrew it, which publishes
    // before the generation that no longer binds it commits (plan 4.10), so the batches between are dropped for it and
    // counted.
    let received = contiguous_once("the added endpoint", &batches_received(&added, "kept.a", sent));
    assert!(received.start > 0 && received.end < sent, "{:?} of {}", received, sent);
    let generations = generations(&deliveries);
    let inside = received
        .clone()
        .flat_map(|index| generations[&index].iter().copied())
        .collect::<BTreeSet<_>>();
    for index in 0..received.start {
        assert!(
            generations[&index].is_disjoint(&inside),
            "batch {} of generations {:?} did not reach the added endpoint, which generations {:?} sent to",
            index,
            generations[&index],
            inside
        );
    }
    assert!(added
        .requests()
        .iter()
        .all(|request| request.api_key.as_deref() == Some("key-added")));

    // Its events, routed over the native set, from the publication that added it to the one that withdrew it, in
    // whole payloads: one contiguous range, once each, which may start with events the encoder held when the endpoint
    // was added.
    let events = (0..sent)
        .map(|index| (index, events_received(&added, index)))
        .filter(|(_, count)| *count > 0)
        .collect::<BTreeMap<_, _>>();
    let events = contiguous_once("the added endpoint's events", &events);
    assert!(events.end < sent, "{:?} of {}", events, sent);
    let logs = (0..sent)
        .map(|index| (index, logs_received(&added, index)))
        .filter(|(_, count)| *count > 0)
        .collect::<BTreeMap<_, _>>();
    let logs = contiguous_once("the added endpoint's logs", &logs);
    assert!(logs.end < sent, "{:?} of {}", logs, sent);
    let traces = (0..sent)
        .map(|index| (index, traces_received(&added, index)))
        .filter(|(_, count)| *count > 0)
        .collect::<BTreeMap<_, _>>();
    let traces = contiguous_once("the added endpoint's traces", &traces);
    assert!(traces.end < sent, "{:?} of {}", traces, sent);

    // Its recipient retired after draining what it held, which the status route lists as native.
    let retired = final_status
        .recipients
        .iter()
        .filter(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
        .collect::<Vec<_>>();
    assert_eq!(retired.len(), 1, "{:?}", final_status.recipients);
    assert_eq!(
        serde_json::to_value(retired[0].provenance.as_ref().unwrap()).unwrap(),
        json!("native")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_primary_endpoint_respelled_live_keeps_receiving_every_batch_event_log_and_trace_once() {
    // A trailing slash names the same URL, so the same queue directory: the primary endpoint stays one recipient, whose
    // settings a publication changes, rather than a second recipient that waits forever for the directory the first
    // one owns while the first one is withdrawn, which cut the primary endpoint off from all data.
    let primary = Intake::start().await;
    let primary_config = format!("dd_url: \"{}\"", primary.url());
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, OTLP, &primary_config]).await;
    let names = ["kept.a"];
    let status = topology.status().unwrap();
    let mut next = 0;
    for _ in 0..5 {
        next = feed(&topology, next, &names);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    primary
        .wait_for("the first batches", |primary| {
            !batches_received(primary, "kept.a", 5).is_empty()
        })
        .await;
    let primary_entry = |status: &PlanStatus| {
        status
            .recipients
            .iter()
            .filter(|entry| entry.host == "dd_out" && entry.id.starts_with("dd_out/primary/"))
            .map(|entry| (entry.id.clone(), entry.incarnation))
            .collect::<Vec<_>>()
    };
    let before = primary_entry(&status.load());
    assert_eq!(before.len(), 1, "{:?}", status.load().recipients);
    let changes = status.load().config_changes;

    stream
        .send(update("dd_url", json!(format!("{}/", primary.url()))))
        .await
        .unwrap();
    let next = feed_until(&topology, next, &names, "the respelling's lowering", 20, |status| {
        status.config_changes > changes && status.installer.preparing.is_none() && status.installer.pending.is_none()
    })
    .await;
    let sent = next;
    primary
        .wait_for("every batch", |primary| {
            batches_received(primary, "kept.a", sent).len() == sent
                && (0..sent).all(|index| {
                    events_received(primary, index) > 0
                        && logs_received(primary, index) > 0
                        && traces_received(primary, index) > 0
                })
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = status.load_full();
    topology.stop().await;
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", sent)),
        0..sent
    );
    for index in 0..sent {
        assert_eq!(events_received(&primary, index), 1, "event {}", index);
        assert_eq!(logs_received(&primary, index), 1, "log {}", index);
        assert_eq!(traces_received(&primary, index), 1, "trace {}", index);
    }
    assert_eq!(
        primary_entry(&after),
        before,
        "the same recipient, under the same incarnation"
    );
    assert!(after.degraded.is_empty(), "{:?}", after.degraded);
}

/// Retries come quickly, so that a recipient whose intake comes back drains well within its deadline.
const QUICK_RETRIES: &str = "forwarder_backoff_base: 0.05\nforwarder_backoff_factor: 1.5\nforwarder_backoff_max: 0.5";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_removed_while_its_intake_is_down_delivers_everything_sent_to_it_once_its_intake_is_back() {
    let primary = Intake::start().await;
    let removed = Intake::start().await;
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-removed]}}",
        primary.url(),
        removed.url()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, QUICK_RETRIES, &config]).await;
    let names = ["kept.a"];

    // The endpoint's intake is down while batches flow, until the recipient's loop has tried to send them, and while
    // the Core Agent removes the endpoint: the loop keeps retrying what the main output's lease sent it, and its
    // drain waits for those payloads.
    removed.set_paused(true);
    let mut next = 0;
    let waited = tokio::time::timeout(PATIENCE, async {
        while removed.unavailable("/api/v2/series") == 0 || removed.unavailable("/api/v1/events_batch") == 0 {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the recipient never tried to send its payloads");
    stream.send(update("additional_endpoints", json!({}))).await.unwrap();
    next = feed_until(&topology, next, &names, "the removed recipient draining", 5, |status| {
        status
            .recipients
            .iter()
            .any(|entry| entry.host == "dd_out" && entry.draining_since_ms.is_some())
    })
    .await;
    assert!(removed.points().is_empty());

    // The intake comes back: the draining recipient delivers what it held, and retires.
    removed.set_paused(false);
    let sent = feed_until(&topology, next, &names, "the removed recipient retiring", 0, |status| {
        status
            .recipients
            .iter()
            .any(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
    })
    .await;
    primary
        .wait_for("every batch", |primary| {
            batches_received(primary, "kept.a", sent).len() == sent
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let final_status = topology.status().unwrap().load_full();
    let deliveries = topology.stop().await;

    // The primary endpoint received every batch once; the removed endpoint, once its intake came back, every batch
    // from the first up to the resource transaction that withdrew it, once each, and its events likewise.
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", sent)),
        0..sent
    );
    let received = contiguous_once("the removed endpoint", &batches_received(&removed, "kept.a", sent));
    assert_eq!(received.start, 0);
    assert!(received.end < sent, "{:?} of {}", received, sent);
    assert!(deliveries.iter().any(|delivery| delivery.output == "dd_metrics_encode"));
    let events = (0..sent)
        .map(|index| (index, events_received(&removed, index)))
        .filter(|(_, count)| *count > 0)
        .collect::<BTreeMap<_, _>>();
    assert_eq!(contiguous_once("the removed endpoint's events", &events).start, 0);
    let retired = final_status
        .recipients
        .iter()
        .find(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
        .unwrap();
    let drain = retired.last_drain.unwrap();
    assert!(!drain.deadline_reached, "{:?}", drain);
    assert!(drain.delivered > 0, "{:?}", drain);
    assert_eq!((drain.persisted, drain.dropped), (0, 0), "{:?}", drain);
}

/// Returns the first batch the generation of batch `index` processed: the batch the generations that followed
/// `index`'s predecessors started with.
fn first_batch_of_generation_after(generations: &BTreeMap<usize, BTreeSet<u64>>, before: &BTreeSet<u64>) -> usize {
    generations
        .iter()
        .find(|(_, of)| of.is_disjoint(before))
        .map(|(index, _)| *index)
        .expect("a later generation processed a batch")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_and_a_routing_group_enabled_live_receive_what_their_lists_select_from_the_generation_that_binds_them()
{
    let primary = Intake::start().await;
    let routed = Intake::start().await;
    let mirror = Intake::start().await;
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-b]}}\nmulti_region_failover: {{enabled: false, \
         api_key: mrf-key, dd_url: \"{}\", failover_metrics: true, metric_allowlist: [orders.total]}}",
        primary.url(),
        routed.url(),
        mirror.url()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, &config]).await;
    let names = ["orders.total", "kept.a"];

    // The boot generation sends everything to both endpoints. The Core Agent enables failover, then routes only
    // `orders.total` to the additional endpoint, while batches flow; each change installs a generation.
    let mut next = 0;
    for _ in 0..5 {
        next = feed(&topology, next, &names);
    }
    stream
        .send(update("multi_region_failover.enabled", json!(true)))
        .await
        .unwrap();
    next = feed_until(&topology, next, &names, "failover's generation", 5, |status| {
        status
            .installer
            .active
            .as_ref()
            .is_some_and(|active| active.id.get() == 2)
    })
    .await;
    let group_endpoint = routed.url();
    stream
        .send(update(
            "experimental.metrics_endpoint_routing.metric_allowlist",
            json!({ group_endpoint: ["orders.total"] }),
        ))
        .await
        .unwrap();
    let sent = feed_until(
        &topology,
        next,
        &names,
        "the routing group's generation",
        10,
        |status| {
            status
                .installer
                .active
                .as_ref()
                .is_some_and(|active| active.id.get() == 3)
        },
    )
    .await;
    primary
        .wait_for("every batch", |primary| {
            batches_received(primary, "kept.a", sent).len() == sent
        })
        .await;
    routed
        .wait_for("every `orders.total`", |routed| {
            batches_received(routed, "orders.total", sent).len() == sent
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let deliveries = topology.stop().await;
    let generations = generations(&deliveries);
    let boot = BTreeSet::from([1]);
    let failover = first_batch_of_generation_after(&generations, &boot);
    let routing = first_batch_of_generation_after(&generations, &BTreeSet::from([1, 2]));

    // The primary endpoint received everything once, across both changes.
    for name in names {
        assert_eq!(
            contiguous_once("the primary endpoint", &batches_received(&primary, name, sent)),
            0..sent,
            "{}",
            name
        );
    }
    // The failover endpoint mirrors `orders.total` from the generation that enabled failover on, once each.
    assert_eq!(
        contiguous_once(
            "the failover endpoint",
            &batches_received(&mirror, "orders.total", sent)
        ),
        failover..sent
    );
    assert!(batches_received(&mirror, "kept.a", sent).is_empty());
    assert!(mirror
        .requests()
        .iter()
        .all(|request| request.api_key.as_deref() == Some("mrf-key")));
    // The additional endpoint received `orders.total` once each, across the change that made it a routing group, whose
    // recipient stays; and every other metric only until the generation that routes it.
    assert_eq!(
        contiguous_once(
            "the routing group's endpoint",
            &batches_received(&routed, "orders.total", sent)
        ),
        0..sent
    );
    assert_eq!(
        contiguous_once(
            "the routing group's endpoint",
            &batches_received(&routed, "kept.a", sent)
        ),
        0..routing
    );
    assert!(
        failover > 0 && failover < routing && routing < sent,
        "{} {} {}",
        failover,
        routing,
        sent
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_flipped_to_v3_series_live_receives_every_batch_once_in_one_protocol_or_the_other() {
    let primary = Intake::start().await;
    let flipped = Intake::start().await;
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-b]}}",
        primary.url(),
        flipped.url()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, &config]).await;
    let names = ["kept.a"];
    let mut next = 0;
    for _ in 0..5 {
        next = feed(&topology, next, &names);
    }
    let endpoint = flipped.url();
    stream
        .send(update("use_v3_api.series.endpoints", json!({ endpoint: "true" })))
        .await
        .unwrap();
    let sent = feed_until(&topology, next, &names, "the flip's generation", 10, |status| {
        status
            .installer
            .active
            .as_ref()
            .is_some_and(|active| active.id.get() == 2)
    })
    .await;
    for intake in [&primary, &flipped] {
        intake
            .wait_for("every batch", |intake| {
                batches_received(intake, "kept.a", sent).len() == sent
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let deliveries = topology.stop().await;
    let flip = first_batch_of_generation_after(&generations(&deliveries), &BTreeSet::from([1]));

    // The primary endpoint keeps V2 series; the flipped one takes V2 series until the generation that flips it, and V3
    // series from it on: every batch once, in one protocol or the other.
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", sent)),
        0..sent
    );
    assert!(primary.points_at("/api/intake/metrics/v3/series").is_empty());
    assert_eq!(
        contiguous_once("the flipped endpoint", &batches_received(&flipped, "kept.a", sent)),
        0..sent
    );
    let v2 = flipped.points_at("/api/v2/series");
    let v3 = flipped.points_at("/api/intake/metrics/v3/series");
    for index in 0..sent {
        let (expected_v2, expected_v3) = if index < flip { (1, 0) } else { (0, 1) };
        assert_eq!(
            (
                v2.get(&context("kept.a", index)).copied().unwrap_or_default(),
                v3.get(&context("kept.a", index)).copied().unwrap_or_default()
            ),
            (expected_v2, expected_v3),
            "batch {} (the flip at {})",
            index,
            flip
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_settings_and_a_proxy_changed_live_reach_the_running_recipients_without_loss_or_duplication() {
    let primary = Intake::start().await;
    // A forward proxy for plain HTTP receives each request with the target in its absolute URI, so an intake stands
    // in for it: it records what reaches it through the proxy.
    let proxy = Intake::start().await;
    let config = format!("dd_url: \"{}\"", primary.url());
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, &config]).await;
    let status = topology.status().unwrap();
    let names = ["kept.a", "kept.b", "kept.c"];
    let mut next = 0;
    for _ in 0..5 {
        next = feed(&topology, next, &names);
    }

    // Settings the recipients run with: smaller series payloads, a shorter timeout, and fewer requests at once, each
    // applied while batches flow.
    for (key, value) in [
        ("serializer_max_series_points_per_payload", json!(2)),
        ("forwarder_timeout", json!(5)),
        ("forwarder_max_concurrent_requests", json!(1)),
    ] {
        let changes = status.load().config_changes;
        stream.send(update(key, value)).await.unwrap();
        next = feed_until(&topology, next, &names, key, 5, |status| {
            status.config_changes > changes
        })
        .await;
    }

    // A proxy: requests go through it from the publication that sets it on.
    stream.send(update("proxy.http", json!(proxy.url()))).await.unwrap();
    let proxy_ref = &proxy;
    let waited = tokio::time::timeout(PATIENCE, async {
        while proxy_ref.points().is_empty() {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "nothing went through the proxy: {}", status.load());
    for _ in 0..5 {
        next = feed(&topology, next, &names);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A proxy no client can be built with: the host keeps sending through the client it has, and reports itself
    // degraded, naming the host, until a later publication fits a client again.
    stream.send(update("proxy.http", json!("http://[::1"))).await.unwrap();
    next = feed_until(&topology, next, &names, "the degraded status", 5, |status| {
        status
            .apply_states
            .status
            .degraded
            .as_deref()
            .is_some_and(|degraded| degraded.contains("dd_out"))
    })
    .await;
    stream.send(update("proxy.http", json!(""))).await.unwrap();
    let sent = feed_until(&topology, next, &names, "the status clearing", 5, |status| {
        status.apply_states.status.degraded.is_none()
    })
    .await;
    let all = |primary: &Intake, proxy: &Intake| {
        names.iter().all(|name| {
            let mut received = batches_received(primary, name, sent);
            for (index, count) in batches_received(proxy, name, sent) {
                *received.entry(index).or_default() += count;
            }
            received.len() == sent
        })
    };
    let waited = tokio::time::timeout(PATIENCE, async {
        while !all(&primary, &proxy) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "not every batch arrived");
    tokio::time::sleep(Duration::from_millis(200)).await;
    topology.stop().await;

    // Every batch arrived once, directly or through the proxy; through the proxy, a contiguous range from the
    // publication that set it to the one that cleared it; and the smaller payloads carry two points at most.
    for name in names {
        let direct = batches_received(&primary, name, sent);
        let proxied = batches_received(&proxy, name, sent);
        let mut received = direct.clone();
        for (index, count) in &proxied {
            *received.entry(*index).or_default() += count;
        }
        assert_eq!(contiguous_once(name, &received), 0..sent);
        let proxied = contiguous_once("through the proxy", &proxied);
        assert!(proxied.start > 0 && proxied.end < sent, "{:?} of {}", proxied, sent);
    }
    let after = primary
        .requests()
        .into_iter()
        .rev()
        .find(|request| request.path == "/api/v2/series" && !request.points.is_empty())
        .unwrap();
    assert!(
        after.points.iter().map(|(_, points)| points).sum::<usize>() <= 2,
        "{:?}",
        after.points
    );
}

/// How many cycles the reclamation tests run before they take their baseline: enough for the caches and buffers the
/// cycles reuse to reach their size.
const WARM_UP_CYCLES: usize = 20;

/// How many more bytes a host's resource group may hold live after the cycles than at the baseline: what its reused
/// buffers grow by, far less than a cycle's leak would add over hundreds of cycles.
const LIVE_BYTES_SLACK: u64 = 64 * 1024;

/// A metrics recorder, installed for the whole process, that counts the handles it handed out that something besides it
/// still holds: the metric handles the process holds, which a handle registered per recipient, per endpoint, or per
/// domain and never dropped would grow. As the process's registry evicts the series no handle holds, it forgets them at
/// the next registration, so that it holds no more memory over cycles than the process would.
#[derive(Default)]
struct LiveHandles {
    counters: std::sync::Mutex<BTreeMap<metrics::Key, std::sync::Arc<std::sync::atomic::AtomicU64>>>,
    gauges: std::sync::Mutex<BTreeMap<metrics::Key, std::sync::Arc<std::sync::atomic::AtomicU64>>>,
    histograms: std::sync::Mutex<BTreeMap<metrics::Key, std::sync::Arc<Samples>>>,
}

/// A histogram's samples, which the reclamation test does not need.
struct Samples;

impl metrics::HistogramFn for Samples {
    fn record(&self, _: f64) {}
}

impl LiveHandles {
    /// Installs the recorder for the process, or returns `None` if another one is installed, as when several tests run
    /// in one process: nextest runs each test in its own.
    fn install() -> Option<&'static Self> {
        let handles: &'static Self = Box::leak(Box::default());
        metrics::set_global_recorder(handles).ok().map(|()| handles)
    }

    /// Returns how many handles something besides the recorder holds.
    fn live(&self) -> usize {
        fn held<T>(map: &std::sync::Mutex<BTreeMap<metrics::Key, std::sync::Arc<T>>>) -> usize {
            let map = map.lock().unwrap();
            map.values()
                .filter(|handle| std::sync::Arc::strong_count(handle) > 1)
                .count()
        }
        held(&self.counters) + held(&self.gauges) + held(&self.histograms)
    }
}

impl metrics::Recorder for LiveHandles {
    fn describe_counter(&self, _: metrics::KeyName, _: Option<metrics::Unit>, _: metrics::SharedString) {}

    fn describe_gauge(&self, _: metrics::KeyName, _: Option<metrics::Unit>, _: metrics::SharedString) {}

    fn describe_histogram(&self, _: metrics::KeyName, _: Option<metrics::Unit>, _: metrics::SharedString) {}

    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let mut counters = self.counters.lock().unwrap();
        counters.retain(|_, handle| std::sync::Arc::strong_count(handle) > 1);
        metrics::Counter::from_arc(std::sync::Arc::clone(counters.entry(key.clone()).or_default()))
    }

    fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        let mut gauges = self.gauges.lock().unwrap();
        gauges.retain(|_, handle| std::sync::Arc::strong_count(handle) > 1);
        metrics::Gauge::from_arc(std::sync::Arc::clone(gauges.entry(key.clone()).or_default()))
    }

    fn register_histogram(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Histogram {
        let mut histograms = self.histograms.lock().unwrap();
        histograms.retain(|_, handle| std::sync::Arc::strong_count(handle) > 1);
        let histogram = histograms
            .entry(key.clone())
            .or_insert_with(|| std::sync::Arc::new(Samples));
        metrics::Histogram::from_arc(std::sync::Arc::clone(histogram))
    }
}

/// What the process holds for its delivery: the tasks the runtime runs, the recipients and leases the installer's
/// resource table holds, the recipients each delivery host withdraws or drains, the resource groups the component
/// registry accounts (with their minimum and firm bounds and live bytes), the components the health registry tracks,
/// the resource budget's use, and the metric handles the process holds.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    tasks: usize,
    recipients: usize,
    leases: usize,
    host_recipients: usize,
    groups: BTreeMap<String, (u64, u64, u64)>,
    health: usize,
    budget_used: u64,
    handles: usize,
}

impl Held {
    /// Returns what `self` holds that `baseline` did not: more tasks or metric handles, other counts, groups added or
    /// removed, other bounds, or, for a host's resource groups (its delivery host's and its leases'), more live bytes
    /// than [`LIVE_BYTES_SLACK`] above the baseline. Live bytes of the other groups are left out: the test's own
    /// doubles (the collectors, and the encode hosts' observers, which run in their hosts' tasks) keep what they
    /// record.
    fn excess_over(&self, baseline: &Held) -> Vec<String> {
        let mut excess = Vec::new();
        if self.tasks > baseline.tasks {
            excess.push(format!("tasks: {} where {} ran", self.tasks, baseline.tasks));
        }
        if self.handles > baseline.handles {
            excess.push(format!(
                "metric handles: {} held where {} were",
                self.handles, baseline.handles
            ));
        }
        for (what, now, then) in [
            ("recipients", self.recipients, baseline.recipients),
            ("leases", self.leases, baseline.leases),
            (
                "withdrawn or draining recipients",
                self.host_recipients,
                baseline.host_recipients,
            ),
            ("health registrations", self.health, baseline.health),
            ("budget bytes", self.budget_used as usize, baseline.budget_used as usize),
        ] {
            if now != then {
                excess.push(format!("{}: {} where {} were", what, now, then));
            }
        }
        let host_group = |group: &str| {
            group.starts_with("topology.primary.forwarders.")
                || group.contains(".leases.")
                || group.ends_with(".resources")
        };
        for (group, &(minimum, firm, live)) in &self.groups {
            match baseline.groups.get(group) {
                None => excess.push(format!("group `{}` added", group)),
                Some(&(then_minimum, then_firm, then_live)) => {
                    if (minimum, firm) != (then_minimum, then_firm) {
                        excess.push(format!(
                            "group `{}`: bounds {:?} where they were {:?}",
                            group,
                            (minimum, firm),
                            (then_minimum, then_firm)
                        ));
                    }
                    if host_group(group) && live > then_live + LIVE_BYTES_SLACK {
                        excess.push(format!(
                            "group `{}`: {} live bytes where {} were",
                            group, live, then_live
                        ));
                    }
                }
            }
        }
        for group in baseline.groups.keys().filter(|group| !self.groups.contains_key(*group)) {
            excess.push(format!("group `{}` removed", group));
        }
        excess
    }
}

fn held(
    status: &PlanStatus, registry: &saluki_core::accounting::ComponentRegistry,
    health: &saluki_core::health::HealthRegistry, handles: &LiveHandles,
) -> Held {
    let snapshot = serde_json::from_str::<serde_json::Value>(&registry.root().memory_snapshot_json())
        .expect("the memory snapshot is JSON");
    let groups = snapshot
        .as_object()
        .expect("the memory snapshot is an object")
        .iter()
        .map(|(group, usage)| {
            let bytes = |field: &str| usage[field].as_u64().expect("usage fields are byte counts");
            (
                group.clone(),
                (
                    bytes("minimum_required_bytes"),
                    bytes("firm_limit_bytes"),
                    bytes("actual_live_bytes"),
                ),
            )
        })
        .collect();
    let health = serde_json::from_str::<serde_json::Value>(&health.snapshot_json())
        .ok()
        .and_then(|snapshot| snapshot.as_object().map(|components| components.len()))
        .unwrap_or_default();
    Held {
        tasks: tokio::runtime::Handle::current().metrics().num_alive_tasks(),
        recipients: status
            .recipients
            .iter()
            .filter(|entry| entry.last_drain.is_none())
            .count(),
        leases: status.resources.hosts.iter().map(|host| host.leases.len()).sum(),
        host_recipients: status
            .resources
            .hosts
            .iter()
            .map(|host| host.withdrawn.len())
            .sum::<usize>()
            + status
                .recipients
                .iter()
                .filter(|entry| entry.draining_since_ms.is_some())
                .count(),
        groups,
        health,
        budget_used: status.resources.budget.used,
        handles: handles.live(),
    }
}

/// A snapshot of the resources product that declares, as revision `revision`, the recipient `team_d_intake` sending to
/// `endpoint` with the credential `credentials/team_d`, and the output `team_d` delivering to it.
fn team_d_resources(revision: u64, endpoint: &str) -> saluki_metric_plan::channel::ArtifactSnapshot {
    use saluki_metric_plan::{
        canonical::PlanHash,
        channel::{ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot},
    };

    let document = json!({
        "resources_version": 1,
        "recipients": [{
            "name": "team_d_intake",
            "host": "dd_out",
            "endpoint": endpoint,
            "credential": "credentials/team_d",
        }],
        "outputs": [{"name": "team_d", "recipients": ["team_d_intake"]}],
    });
    let hash = PlanHash::of_value(&document).unwrap();
    let payload = json!({"revision": revision, "artifact_hash": hash, "artifact": document}).to_string();
    ArtifactSnapshot::new(
        ArtifactProduct::Resources,
        vec![ArtifactConfig {
            id: ArtifactConfigId::new("resources"),
            version: revision,
            sha256: ArtifactConfig::transport_hash(&payload),
            payload,
        }],
    )
}

/// Runs one cycle of the reclamation test: an endpoint of an intake of its own, so of a domain of its own, is added and
/// receives something (metrics, which its host keeps per-domain series for, in every fifth cycle, whose wait for the
/// encoder's flush is longer), while a resources artifact declares a recipient sending to the same intake and an output
/// delivering to it, whose recipient is prepared; then both are removed, and the process settles, with one generation
/// live, one lease, and the primary endpoint's recipient alone, the removed ones retired, and the intake is stopped.
/// Returns the next batch's index.
async fn cycle(
    round: usize, topology: &TestTopology, stream: &tokio::sync::mpsc::Sender<saluki_config::dynamic::ConfigUpdate>,
    resources: &saluki_metric_plan::channel::ArtifactPublisher, mut next: usize,
) -> usize {
    let status = topology.status().expect("the plan-driven path has an installer");
    let intake = &Intake::start().await;
    let endpoint = format!("{}/native-{}", intake.url(), round);
    stream
        .send(update("additional_endpoints", json!({ endpoint: ["key-added"] })))
        .await
        .unwrap();
    assert!(resources.publish(team_d_resources(
        round as u64 + 1,
        &format!("{}/declared-{}", intake.url(), round)
    )));
    let before = intake.requests().len();
    let received = || {
        if round.is_multiple_of(5) {
            !intake.points().is_empty()
        } else {
            intake.requests().len() > before
        }
    };
    let declared_live = |status: &PlanStatus| {
        status.recipients.iter().any(|entry| {
            entry.id.starts_with("dd_out/declared/")
                && entry.last_drain.is_none()
                && entry.draining_since_ms.is_none()
                && entry.state == super::installer::status::RecipientEntryState::Live
        })
    };
    let waited = tokio::time::timeout(PATIENCE, async {
        while !received() || !declared_live(&status.load()) {
            next = feed(topology, next, &["kept.a"]);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "cycle {}: the endpoint received nothing, or the declared recipient never ran: {}\n{:#?}",
        round,
        status.load(),
        status.load().recipients
    );
    stream.send(update("additional_endpoints", json!({}))).await.unwrap();
    assert!(resources.publish(saluki_metric_plan::channel::ArtifactSnapshot::new(
        saluki_metric_plan::channel::ArtifactProduct::Resources,
        Vec::new()
    )));
    let settled = |status: &PlanStatus| {
        status.installer.live.len() == 1
            && status
                .recipients
                .iter()
                .filter(|entry| entry.last_drain.is_none())
                .count()
                == 1
            && status
                .resources
                .hosts
                .iter()
                .map(|host| host.leases.len())
                .sum::<usize>()
                == 1
    };
    let waited = tokio::time::timeout(PATIENCE, async {
        while !settled(&status.load()) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "cycle {}: the process never settled: {}\n{}\n{:#?}",
        round,
        status.load(),
        serde_json::to_string_pretty(&status.load().resources.hosts).unwrap(),
        status.load().recipients
    );
    next
}

/// Adds and removes an endpoint, and declares and revokes a recipient and an output, `cycles` times, each of a domain
/// of its own, after [`WARM_UP_CYCLES`], and checks that the process then holds what it held after the warm-up (see
/// [`Held::excess_over`]), with the resource budget counted against a memory limit.
async fn reclamation(cycles: usize) {
    use saluki_app::accounting::{MemoryMode, VerifiedMemory};

    let handles = LiveHandles::install().expect("the test runs in a process of its own");
    let primary = Intake::start().await;
    let config = format!(
        "dd_url: \"{}\"\nexperimental: {{programmable_metrics: {{resources: {{credentials: {{team_d: {}}}}}}}}}",
        primary.url(),
        "key-team-d"
    );
    let (system, updates, stream) = connected_configuration_system(&[FLUSH, ZSTD, &config].join("\n")).await;
    let registry = saluki_core::accounting::ComponentRegistry::default();
    let health = saluki_core::health::HealthRegistry::new();
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.encoders = Encoders::Real;
    options.recipients = Recipients::Sending;
    options.registry = registry.clone();
    options.health = health.clone();
    options.keep = |_| false;
    let transport = std::sync::Arc::new(std::sync::Mutex::new(None));
    options.before_install = Some(Box::new({
        let transport = std::sync::Arc::clone(&transport);
        move |metrics: &mut super::installer::ProgrammableMetrics| {
            *transport.lock().unwrap() = Some(metrics.connect_artifact_channel());
            // What a preparation adds is counted, against a limit no cycle reaches.
            metrics.admit_against(Some(VerifiedMemory {
                grant_bytes: 1 << 33,
                firm_bytes: 0,
                mode: MemoryMode::Strict,
            }));
        }
    }));
    let topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    let status = topology.status().unwrap();
    let transport = transport
        .lock()
        .unwrap()
        .take()
        .expect("the artifact channel is connected");

    // The warm-up cycles register what every later one reuses (the hosts' per-domain series, the leases' resource
    // groups) and grow the buffers they reuse; the baseline is taken after them, once the process settled.
    let mut next = 0;
    for round in 0..WARM_UP_CYCLES {
        next = cycle(round, &topology, &stream, &transport.publisher, next).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let baseline = held(&status.load(), &registry, &health, handles);
    assert!(
        baseline.budget_used > 0,
        "the budget counts the boot generation's lease: {:?}",
        baseline
    );
    let started = std::time::Instant::now();
    for round in WARM_UP_CYCLES..WARM_UP_CYCLES + cycles {
        next = cycle(round, &topology, &stream, &transport.publisher, next).await;
    }
    let elapsed = started.elapsed();

    // Everything returns to the baseline: the tasks the cycles spawned (recipient loops, client connections, lease
    // builders) once they exit, and the rest at once.
    let waited = tokio::time::timeout(PATIENCE, async {
        while !held(&status.load(), &registry, &health, handles)
            .excess_over(&baseline)
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "after {} cycles in {:?}, the process holds more than it did: {:#?}",
        cycles,
        elapsed,
        held(&status.load(), &registry, &health, handles).excess_over(&baseline)
    );
    topology.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cycles_of_adding_and_removing_endpoints_and_declared_resources_leave_the_process_holding_what_it_held() {
    reclamation(50).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "runs 1,000 cycles, about twelve minutes; the stage 7 gate runs it with `--run-ignored only`"]
async fn a_thousand_cycles_of_adding_and_removing_endpoints_and_declared_resources_reclaim_what_they_held() {
    reclamation(1_000).await;
}

/// Returns every file below `directory`, at any depth.
fn files_below(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_whose_intake_stays_down_retires_at_its_deadline_and_persists_what_it_holds() {
    let primary = Intake::start().await;
    let removed = Intake::start().await;
    let storage = tempfile::tempdir().unwrap();
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-removed]}}\nforwarder_storage_max_size_in_bytes: \
         10485760\nforwarder_storage_path: \"{}\"",
        primary.url(),
        removed.url(),
        storage.path().display()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, QUICK_RETRIES, &config]).await;
    let names = ["kept.a"];

    // The endpoint's intake goes down for good once the recipient's loop has tried to send what it has; the Core Agent
    // removes the endpoint, and the recipient drains until its deadline (30 seconds), since nothing it holds can be
    // delivered.
    removed.set_paused(true);
    let mut next = 0;
    let waited = tokio::time::timeout(PATIENCE, async {
        while removed.unavailable("/api/v2/series") == 0 {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the recipient never tried to send its payloads");
    stream.send(update("additional_endpoints", json!({}))).await.unwrap();
    let status = topology.status().unwrap();
    let retired = tokio::time::timeout(saluki_metric_plan::limits::values::DRAIN_DEADLINE * 2, async {
        loop {
            next = feed(&topology, next, &names);
            if let Some(entry) = status
                .load()
                .recipients
                .iter()
                .find(|entry| entry.host == "dd_out" && entry.last_drain.is_some())
            {
                return entry.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the recipient retires at its deadline");
    let drain = retired.last_drain.unwrap();
    topology.stop().await;

    // It reached its deadline, delivered nothing, and persisted what it held into its directory under the storage
    // path, from which a recipient of the same endpoint would send it again; nothing was dropped.
    assert!(drain.deadline_reached, "{:?}", drain);
    assert_eq!(drain.delivered, 0, "{:?}", drain);
    assert!(drain.persisted > 0, "{:?}", drain);
    assert_eq!(drain.dropped, 0, "{:?}", drain);
    assert!(removed.points().is_empty());
    assert!(!files_below(storage.path()).is_empty());
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", next)).start,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_process_stops_while_a_recipient_drains() {
    let primary = Intake::start().await;
    let removed = Intake::start().await;
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-removed]}}",
        primary.url(),
        removed.url()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, QUICK_RETRIES, &config]).await;
    let names = ["kept.a"];
    removed.set_paused(true);
    let mut next = 0;
    let waited = tokio::time::timeout(PATIENCE, async {
        while removed.unavailable("/api/v2/series") == 0 {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the recipient never tried to send its payloads");
    stream.send(update("additional_endpoints", json!({}))).await.unwrap();
    feed_until(&topology, next, &names, "the removed recipient draining", 0, |status| {
        status
            .recipients
            .iter()
            .any(|entry| entry.host == "dd_out" && entry.draining_since_ms.is_some())
    })
    .await;

    // Shutdown closes the draining recipient as at process shutdown, well before its deadline: the topology stops
    // cleanly, in less time than the drain would have taken.
    let started = std::time::Instant::now();
    topology.stop().await;
    assert!(
        started.elapsed() < saluki_metric_plan::limits::values::DRAIN_DEADLINE,
        "{:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_that_cannot_be_prepared_is_retried_while_the_endpoint_its_edit_removes_stops_at_the_transaction() {
    use std::os::unix::fs::PermissionsExt as _;

    let primary = Intake::start().await;
    let removed = Intake::start().await;
    let added = Intake::start().await;
    let storage = tempfile::tempdir().unwrap();
    let config = format!(
        "dd_url: \"{}\"\nadditional_endpoints: {{\"{}\": [key-removed]}}\nforwarder_storage_max_size_in_bytes: \
         10485760\nforwarder_storage_path: \"{}\"",
        primary.url(),
        removed.url(),
        storage.path().display()
    );
    let (topology, stream) = start_connected(&[FLUSH, ZSTD, OTLP, &config]).await;
    let status = topology.status().unwrap();
    let names = ["kept.a"];
    let mut next = 0;
    let removed_ref = &removed;
    let waited = tokio::time::timeout(PATIENCE, async {
        while removed_ref.points().is_empty() {
            next = feed(&topology, next, &names);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the endpoint to remove never received a batch");

    // Every recipient of the intakes' host has its queue directory below one directory per host; once the boot
    // recipients opened theirs, nothing more can be created there, so a new recipient's preparation fails.
    let hosts = storage.path().join("dd_out").join("127.0.0.1");
    primary
        .wait_for("the boot recipients' directories", |_| hosts.is_dir())
        .await;
    std::fs::set_permissions(&hosts, std::fs::Permissions::from_mode(0o500)).unwrap();

    // One edit removes an endpoint and adds one: the transaction withdraws the removed one, and the added one, which
    // cannot be prepared, is retried, while the generation running keeps running and is reported degraded.
    let added_endpoint = added.url();
    stream
        .send(update("additional_endpoints", json!({ added_endpoint: ["key-added"] })))
        .await
        .unwrap();
    next = feed_until(&topology, next, &names, "the degraded status", 10, |status| {
        status.apply_states.status.degraded.is_some()
            && status.resources.hosts.iter().any(|host| !host.withdrawn.is_empty())
    })
    .await;
    assert_eq!(
        status.load().installer.active.as_ref().map(|active| active.id.get()),
        Some(1)
    );
    assert!(added.points().is_empty());
    // The configuration change is not in effect, and the status says why while its generation is tried again.
    next = feed_until(
        &topology,
        next,
        &names,
        "the configuration change's failure",
        0,
        |status| {
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("is tried again"))
        },
    )
    .await;

    // Once a directory can be created again, a retry prepares the added endpoint, and the generation that binds it
    // commits.
    std::fs::set_permissions(&hosts, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sent = feed_until(
        &topology,
        next,
        &names,
        "the added endpoint's generation",
        10,
        |status| {
            status
                .installer
                .active
                .as_ref()
                .is_some_and(|active| active.id.get() > 1)
                && status.apply_states.status.degraded.is_none()
        },
    )
    .await;
    for intake in [&primary, &added] {
        intake
            .wait_for("the last batch", |intake| {
                batches_received(intake, "kept.a", sent).contains_key(&(sent - 1))
                    && events_received(intake, sent - 1) > 0
                    && logs_received(intake, sent - 1) > 0
                    && traces_received(intake, sent - 1) > 0
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    topology.stop().await;

    // The primary endpoint received every batch once; the removed one a first range, up to the transaction that
    // withdrew it; the added one a last range, from the generation that bound it.
    assert_eq!(
        contiguous_once("the primary endpoint", &batches_received(&primary, "kept.a", sent)),
        0..sent
    );
    let before = contiguous_once("the removed endpoint", &batches_received(&removed, "kept.a", sent));
    let after = contiguous_once("the added endpoint", &batches_received(&added, "kept.a", sent));
    assert_eq!(before.start, 0);
    assert_eq!(after.end, sent);
    assert!(before.end < after.start, "{:?} {:?}", before, after);

    // Events, logs, and traces follow the native set, which the failed preparation does not hold back: the primary
    // endpoint received each once, the removed one a first range up to the transaction that withdrew it, the added one
    // a last range from the transaction that published it once it could be prepared.
    let received = |intake: &Intake, what: fn(&Intake, usize) -> usize| {
        (0..sent)
            .map(|index| (index, what(intake, index)))
            .filter(|(_, count)| *count > 0)
            .collect::<BTreeMap<_, _>>()
    };
    for (what, count) in [
        ("events", events_received as fn(&Intake, usize) -> usize),
        ("logs", logs_received),
        ("traces", traces_received),
    ] {
        assert_eq!(
            contiguous_once(&format!("the primary endpoint's {what}"), &received(&primary, count)),
            0..sent
        );
        let before = contiguous_once(&format!("the removed endpoint's {what}"), &received(&removed, count));
        let after = contiguous_once(&format!("the added endpoint's {what}"), &received(&added, count));
        assert_eq!(before.start, 0, "{what}");
        assert_eq!(after.end, sent, "{what}");
        assert!(before.end <= after.start, "{what}: {:?} {:?}", before, after);
    }
}

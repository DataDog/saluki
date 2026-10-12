//! The telemetry audit: every telemetry write the default program's kernels and their caches make, by how often they
//! make it.
//!
//! A slot that runs in several tasks at once must not write a telemetry handle that the tasks share on every event:
//! handles are shared by everything registered under one name and label set, so such writes contend across the tasks
//! on every event. Kernels and caches instead count in integers of their own and publish what they counted once a
//! second, when their region publishes. This audit runs the default program through a frame of region drivers with a
//! recorder that counts every write to every handle, over workloads that differ in one dimension at a time, and checks
//! that what it measures is the checked-in table below: no write is made per event.
//!
//! A second test checks the other half of the rule: what the pre-aggregation slot's kernels count reaches their series
//! each time their region publishes, without waiting for the kernels to be dropped.
//!
//! The audit covers what the regions run: the kernels of every slot, their caches and memos, and the mapper's context
//! resolver. It does not cover the interconnect between components, which the frame harness replaces by direct calls.
//! Those writes are made per buffer, which the telemetry rule allows: a dispatcher's `component_events_sent_total` and
//! its trace-level `component_send_latency_seconds`, with two reads of the monotonic clock, and a consumer's
//! `component_events_received_total` and trace-level `component_events_received_size`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{Tag, TagSet},
    },
    execution::AdmissionTime,
};
use saluki_metrics::test::TestRecorder;

use super::harness::FrameHarness;
use crate::{
    ir::Plan,
    operators::OperatorRegistry,
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    validate::Validator,
};

/// How often a handle is written, as the audit measures it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Frequency {
    /// More events with the same contexts write it more: once or more per event.
    PerEvent,

    /// More contexts in the same events write it more: once or more per context the workload creates.
    PerNewContext,

    /// More buffers of the same events write it more: once or more per buffer.
    PerBuffer,

    /// The same in every workload: written when a kernel is built, at each publication, or at a flush tick.
    Fixed,
}

/// Every handle the default program's kernels write more than a fixed number of times, normalized by [`normalize`],
/// with how often they write it. Every other handle they register is written a fixed number of times, whatever the
/// workload: when a kernel is built, at each publication, by a background task once a second, or at a flush tick.
///
/// No entry is [`Frequency::PerEvent`]: every per-event count accumulates in the kernel or cache that counts it, and
/// reaches its handle once per publication.
const AUDIT: &[(&str, Frequency)] = &[
    // The timestamp split counts each buffer of timestamped metrics it passes through, and the metrics in it.
    ("aggregate_passthrough_flushes_total", Frequency::PerBuffer),
    ("aggregate_passthrough_metrics_total", Frequency::PerBuffer),
    // The mapper's context resolver updates its gauge of active contexts when it creates a context, and every context
    // it created updates it again when it is dropped: twice per new mapped context, never per event.
    (
        "context_resolver_active_contexts{resolver_id=dsd_mapper/primary}",
        Frequency::PerNewContext,
    ),
];

/// Labels whose values name the component a kernel runs in, which the slot layout decides, and whose values the audit
/// leaves out.
const COMPONENT_LABELS: [&str; 2] = ["component_id", "component_type"];

/// Labels whose values start with the component a kernel runs in, followed by a `/`, which the audit leaves out.
const SCOPED_LABELS: [&str; 2] = ["cache_id", "resolver_id"];

/// Returns a handle's key as the audit names it: its name and labels, without the parts the slot layout decides, and
/// without the source location of a background task's name.
fn normalize(key: &Key) -> String {
    let mut labels = key
        .labels()
        .filter(|label| !COMPONENT_LABELS.contains(&label.key()))
        .map(|label| {
            let value = label.value();
            let value = match label.key() {
                key if SCOPED_LABELS.contains(&key) => value.split_once('/').map_or(value, |(_, rest)| rest),
                "task_name" => value.split_once('@').map_or(value, |(file, _)| file),
                _ => value,
            };
            format!("{}={}", label.key(), value)
        })
        .collect::<Vec<_>>();
    labels.sort();
    if labels.is_empty() {
        key.name().to_string()
    } else {
        format!("{}{{{}}}", key.name(), labels.join(","))
    }
}

/// A handle that counts the writes made to it, whatever they write.
struct CountingHandle {
    key: String,
    writes: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl CountingHandle {
    fn write(&self) {
        *self.writes.lock().unwrap().entry(self.key.clone()).or_default() += 1;
    }
}

impl CounterFn for CountingHandle {
    fn increment(&self, _value: u64) {
        self.write();
    }

    fn absolute(&self, _value: u64) {
        self.write();
    }
}

impl GaugeFn for CountingHandle {
    fn increment(&self, _value: f64) {
        self.write();
    }

    fn decrement(&self, _value: f64) {
        self.write();
    }

    fn set(&self, _value: f64) {
        self.write();
    }
}

impl HistogramFn for CountingHandle {
    fn record(&self, _value: f64) {
        self.write();
    }
}

/// A recorder whose handles count their writes, by normalized key.
#[derive(Default)]
struct CountingRecorder {
    writes: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl CountingRecorder {
    fn handle(&self, key: &Key) -> Arc<CountingHandle> {
        let key = normalize(key);
        // Registering a handle is not a write, but it names every handle, so that one never written is listed too.
        self.writes.lock().unwrap().entry(key.clone()).or_default();
        Arc::new(CountingHandle {
            key,
            writes: Arc::clone(&self.writes),
        })
    }

    fn writes(&self) -> BTreeMap<String, u64> {
        self.writes.lock().unwrap().clone()
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(self.handle(key))
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(self.handle(key))
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(self.handle(key))
    }
}

/// A workload: DogStatsD batches through the pre-aggregation, aggregation, and post-aggregation slots, and OTLP
/// batches through enrichment, each of `events` metrics over `contexts` contexts, then a flush tick.
#[derive(Clone, Copy, Debug)]
struct Workload {
    batches: u64,
    events: usize,
    contexts: usize,
}

/// The base time of the default program's fixtures.
const B: u64 = 1_700_000_000;

/// Returns the admission time `relative` seconds after [`B`].
fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

/// Returns a current-thread Tokio runtime with timers: kernels with caches are built in one, as the interpreter's are.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Returns a frame of region drivers running the default program, which must be built inside a Tokio runtime.
fn default_program_harness() -> FrameHarness {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    FrameHarness::new(&plan, &report, registry)
}

/// The DogStatsD metrics of one batch: by quarters, counters the mapper's profile maps, distributions the tag
/// projection rules project, counters aggregation holds as they arrive, and timestamped counters, which pass
/// aggregation by, all on `contexts` contexts in turn.
fn dogstatsd_batch(workload: Workload) -> Vec<Metric> {
    (0..workload.events)
        .map(|event| {
            let context = event % workload.contexts;
            let tag = format!("user_id:{}", context);
            match event % 4 {
                3 => Metric::counter(
                    Context::from_parts(format!("timestamped.t{}", context), TagSet::default()),
                    (B + 1, 1.0),
                ),
                0 => Metric::counter(
                    Context::from_parts(format!("raw.requests.r{}", context), TagSet::default()),
                    1.0,
                ),
                1 => Metric::distribution(
                    Context::from_parts(
                        "checkout.latency",
                        [Tag::from(tag), Tag::from("env:dev")].into_iter().collect::<TagSet>(),
                    ),
                    2.0,
                ),
                _ => Metric::counter(
                    Context::from_parts(format!("orders.o{}", context), TagSet::default()),
                    1.0,
                ),
            }
        })
        .collect()
}

/// Returns the writes every handle took over `workload`, which ends with a publication and the frame's drop.
fn writes_over(workload: Workload) -> BTreeMap<String, u64> {
    let recorder = CountingRecorder::default();
    {
        let _local = metrics::set_default_local_recorder(&recorder);
        let runtime = runtime();
        let _guard = runtime.enter();
        let mut harness = default_program_harness();
        for batch in 0..workload.batches {
            harness.admit("dsd_in", t(1 + batch % 4), dogstatsd_batch(workload));
            let otlp = (0..workload.events)
                .map(|event| {
                    let name = format!("otlp.g{}", event % workload.contexts);
                    Metric::gauge(Context::from_parts(name, TagSet::default()), (B + 5, 1.0))
                })
                .collect();
            harness.admit("otlp_in", t(5), otlp);
        }
        // The tick at 15 waits for the watermark, which a batch at 16 raises: the tick then flushes every context.
        harness.advance(t(15));
        harness.admit("dsd_in", t(16), vec![Metric::gauge("late", 1.0)]);
        harness.publish();
        drop(harness);
    }
    recorder.writes()
}

/// Classifies every handle by comparing workloads that differ in one dimension at a time.
fn measure() -> BTreeMap<String, Frequency> {
    let base = Workload {
        batches: 4,
        events: 12,
        contexts: 12,
    };
    let more_events = Workload { events: 48, ..base };
    let more_contexts = Workload {
        events: 48,
        contexts: 48,
        ..base
    };
    let more_buffers = Workload { batches: 8, ..base };
    let [base, more_events, more_contexts, more_buffers] =
        [base, more_events, more_contexts, more_buffers].map(writes_over);
    base.keys()
        .chain(more_contexts.keys())
        .map(|key| {
            let writes = |run: &BTreeMap<String, u64>| run.get(key).copied().unwrap_or(0);
            let frequency = if writes(&more_events) > writes(&base) {
                Frequency::PerEvent
            } else if writes(&more_contexts) > writes(&more_events) {
                Frequency::PerNewContext
            } else if writes(&more_buffers) > writes(&base) {
                Frequency::PerBuffer
            } else {
                Frequency::Fixed
            };
            (key.clone(), frequency)
        })
        .collect()
}

#[test]
fn the_default_programs_kernels_write_no_telemetry_handle_per_event() {
    let measured = measure();
    let table = measured
        .iter()
        .map(|(key, frequency)| format!("    (\"{}\", Frequency::{:?}),", key, frequency))
        .collect::<Vec<_>>()
        .join("\n");
    let unfixed = measured
        .into_iter()
        .filter(|(_, frequency)| *frequency != Frequency::Fixed)
        .collect::<BTreeMap<_, _>>();
    let audit = AUDIT
        .iter()
        .map(|(key, frequency)| (key.to_string(), *frequency))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        unfixed, audit,
        "every handle the kernels write, as measured:\n{}",
        table
    );
    let per_event = audit
        .iter()
        .filter(|(_, frequency)| **frequency == Frequency::PerEvent)
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    assert!(per_event.is_empty(), "handles written per event: {:?}", per_event);
}

/// The series the pre-aggregation slot's kernels count on every metric, each by its name and, for a cache or a
/// resolver, the end of its identifier: the mapper's result cache and context resolver, and the tag projection's
/// context memo and outcomes.
const PRE_AGGREGATION_SERIES: [(&str, &str); 9] = [
    ("cache_hits_total", "dsd_mapper/result_cache"),
    ("cache_misses_total", "dsd_mapper/result_cache"),
    ("cache_hits_total", "tag_filterlist/context_cache"),
    ("cache_misses_total", "tag_filterlist/context_cache"),
    ("context_resolver_resolved_existing_context_total", "dsd_mapper/primary"),
    ("context_resolver_resolved_new_context_total", "dsd_mapper/primary"),
    ("tag_filterlist_metrics_modified_total", ""),
    ("tag_filterlist_rule_hits_total", ""),
    ("tag_filterlist_rule_misses_total", ""),
];

/// Returns the values of [`PRE_AGGREGATION_SERIES`] in `recorder`, each summed over the components that report it.
fn pre_aggregation_series(recorder: &TestRecorder) -> BTreeMap<(&'static str, &'static str), u64> {
    let mut values = PRE_AGGREGATION_SERIES
        .iter()
        .map(|series| (*series, 0))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in recorder.counters() {
        let identifier = key
            .labels()
            .find(|label| SCOPED_LABELS.contains(&label.key()))
            .map_or("", |label| label.value());
        for ((name, scope), total) in values.iter_mut() {
            if key.name() == *name && identifier.ends_with(scope) {
                *total += value;
            }
        }
    }
    values
}

#[test]
fn the_pre_aggregation_kernels_publish_what_they_counted_each_time_their_region_publishes() {
    // Three counters of one context, which the mapper maps, and two distributions of another, whose tags the tag
    // projection rules project. The mapper looks every name up in its result cache, which misses the first time it sees each name, and
    // resolves each mapped context, the first time as a new context. The tag projection finds no rule for the mapped
    // counters, whose outcomes it never memoizes, and projects the distributions, the second from its memo.
    let batch = || {
        let distribution = || {
            Metric::distribution(
                Context::from_parts(
                    "checkout.latency",
                    [Tag::from("user_id:1"), Tag::from("env:dev")]
                        .into_iter()
                        .collect::<TagSet>(),
                ),
                2.0,
            )
        };
        let mut metrics = (0..3)
            .map(|_| Metric::counter("raw.requests.checkout", 1.0))
            .collect::<Vec<_>>();
        metrics.extend([distribution(), distribution()]);
        metrics
    };
    // The values of the series, in the order of `PRE_AGGREGATION_SERIES`.
    let counted = |values: [u64; 9]| {
        PRE_AGGREGATION_SERIES
            .into_iter()
            .zip(values)
            .collect::<BTreeMap<_, _>>()
    };

    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let runtime = runtime();
    let _guard = runtime.enter();
    let mut harness = default_program_harness();

    harness.admit("dsd_in", t(1), batch());
    assert_eq!(
        pre_aggregation_series(&recorder),
        counted([0; 9]),
        "nothing is written per metric"
    );
    harness.publish();
    let first = counted([3, 2, 1, 4, 2, 1, 2, 2, 3]);
    assert_eq!(pre_aggregation_series(&recorder), first);

    // A second batch of the same metrics, every name of which the mapper's cache now holds, reaches the series at the
    // next publication, and not before.
    harness.admit("dsd_in", t(2), batch());
    assert_eq!(pre_aggregation_series(&recorder), first);
    harness.publish();
    let second = counted([8, 2, 3, 7, 5, 1, 4, 4, 6]);
    assert_eq!(pre_aggregation_series(&recorder), second);

    // The publications published everything: the kernels have nothing left to publish when they are dropped.
    drop(harness);
    assert_eq!(pre_aggregation_series(&recorder), second);
}

//! Helpers shared by this crate's kernel tests: describing metrics completely, comparing outputs as multisets, driving
//! a kernel through one turn, and counting telemetry writes.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{BatchHeader, Budget, Kernel, PortIndex, PortOutputs, Progress, Turn, TurnCounters},
    topology::EventsBuffer,
};
use saluki_metrics::test::TestRecorder;

/// Describes everything about a metric that a delivery carries: its name, host, tags and origin tags in their order,
/// values, and metadata.
///
/// Two metrics with equal descriptions are indistinguishable downstream, including at an encoder, which writes tags
/// in their order.
pub(crate) fn describe(metric: &Metric) -> String {
    let context = metric.context();
    let tags = context.tags().into_iter().map(|tag| tag.as_str()).collect::<Vec<_>>();
    let origin_tags = context
        .origin_tags()
        .into_iter()
        .map(|tag| tag.as_str())
        .collect::<Vec<_>>();
    format!(
        "name={:?} host={:?} tags={:?} origin_tags={:?} values={:?} metadata={:?}",
        context.name().as_ref(),
        context.host(),
        tags,
        origin_tags,
        metric.values(),
        metric.metadata()
    )
}

/// Returns the descriptions of `metrics`, sorted, so that outputs compare as multisets.
pub(crate) fn multiset<'a, I>(metrics: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a Metric>,
{
    let mut described = metrics.into_iter().map(describe).collect::<Vec<_>>();
    described.sort();
    described
}

/// Returns a buffer holding `metrics`.
pub(crate) fn buffer_of<I>(metrics: I) -> EventsBuffer
where
    I: IntoIterator<Item = Metric>,
{
    let mut buffer = EventsBuffer::default();
    for metric in metrics {
        assert!(
            buffer.try_push(Event::Metric(metric)).is_none(),
            "test buffers hold at most one buffer's worth of metrics"
        );
    }
    buffer
}

/// Returns the metrics of a buffer, in order, ignoring other events.
pub(crate) fn metrics_of(buffer: EventsBuffer) -> Vec<Metric> {
    buffer.into_iter().filter_map(Event::try_into_metric).collect()
}

/// What one call to [`Kernel::consume`] produced.
pub(crate) struct Consumed {
    /// The metrics delivered on each port, in order.
    pub ports: Vec<Vec<Metric>>,

    /// The number of buffers delivered on each port.
    pub buffers: Vec<usize>,

    /// The loss the kernel counted.
    pub counters: TurnCounters,
}

/// Runs one batch through a kernel with `port_count` ports, as a host does: one turn with the batch's header and an
/// unlimited budget, then the input's leftover moved to its port.
pub(crate) fn consume<K>(kernel: &mut K, header: &BatchHeader, port_count: usize, mut input: EventsBuffer) -> Consumed
where
    K: Kernel + ?Sized,
{
    let mut budget = Budget::unlimited();
    let mut outputs = PortOutputs::new(port_count);
    let mut counters = TurnCounters::default();
    let progress = {
        let mut turn = Turn::new(header, Instant::now(), &mut budget, &mut outputs, &mut counters);
        kernel.consume(&mut turn, &mut input)
    };
    assert!(
        matches!(progress, Progress::Done),
        "stateless kernels finish every batch in one call"
    );
    outputs.complete(input);

    let mut ports = Vec::with_capacity(port_count);
    let mut buffers = Vec::with_capacity(port_count);
    for port in 0..port_count {
        let index = PortIndex::new(u16::try_from(port).expect("tests use few ports"));
        let drained = outputs.drain(index).collect::<Vec<_>>();
        buffers.push(drained.len());
        ports.push(drained.into_iter().flat_map(metrics_of).collect());
    }
    Consumed {
        ports,
        buffers,
        counters,
    }
}

/// A recorder that counts the writes made to each handle, whatever they write, and passes every write on to a
/// [`TestRecorder`], which keeps the values.
#[derive(Default)]
pub(crate) struct CountingRecorder {
    values: TestRecorder,
    writes: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl CountingRecorder {
    /// Returns the recorder that keeps the values written.
    pub(crate) fn values(&self) -> &TestRecorder {
        &self.values
    }

    /// Returns the number of writes made to each handle since the last call, by [key name][key_name], leaving out
    /// handles not written.
    pub(crate) fn take_writes(&self) -> BTreeMap<String, u64> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }

    fn counting<H>(&self, key: &Key, inner: H) -> Arc<CountingHandle<H>> {
        Arc::new(CountingHandle {
            key: key_name(key),
            writes: Arc::clone(&self.writes),
            inner,
        })
    }
}

/// Names a key as `name{label=value,...}`, its labels sorted, or as its name alone without labels.
pub(crate) fn key_name(key: &Key) -> String {
    let mut labels = key
        .labels()
        .map(|label| format!("{}={}", label.key(), label.value()))
        .collect::<Vec<_>>();
    labels.sort();
    if labels.is_empty() {
        key.name().to_string()
    } else {
        format!("{}{{{}}}", key.name(), labels.join(","))
    }
}

/// A handle that counts its writes, and passes them on to the handle it wraps.
struct CountingHandle<H> {
    key: String,
    writes: Arc<Mutex<BTreeMap<String, u64>>>,
    inner: H,
}

impl<H> CountingHandle<H> {
    fn count(&self) {
        *self.writes.lock().unwrap().entry(self.key.clone()).or_default() += 1;
    }
}

impl CounterFn for CountingHandle<Counter> {
    fn increment(&self, value: u64) {
        self.count();
        self.inner.increment(value);
    }

    fn absolute(&self, value: u64) {
        self.count();
        self.inner.absolute(value);
    }
}

impl GaugeFn for CountingHandle<Gauge> {
    fn increment(&self, value: f64) {
        self.count();
        self.inner.increment(value);
    }

    fn decrement(&self, value: f64) {
        self.count();
        self.inner.decrement(value);
    }

    fn set(&self, value: f64) {
        self.count();
        self.inner.set(value);
    }
}

impl HistogramFn for CountingHandle<Histogram> {
    fn record(&self, value: f64) {
        self.count();
        self.inner.record(value);
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.values.describe_counter(key, unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.values.describe_gauge(key, unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.values.describe_histogram(key, unit, description);
    }

    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        Counter::from_arc(self.counting(key, self.values.register_counter(key, metadata)))
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(self.counting(key, self.values.register_gauge(key, metadata)))
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(self.counting(key, self.values.register_histogram(key, metadata)))
    }
}

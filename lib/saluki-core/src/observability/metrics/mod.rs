//! Internal metrics support.
//!
//! Includes the [`MetricsStream`] broadcast of internally emitted metrics events and the
//! [`Reflector`]-based [`AggregatedMetricsState`] view that downstream callers query for
//! Prometheus exposition.

use std::{
    num::NonZeroUsize,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, LazyLock, Mutex, OnceLock,
    },
    task::{self, ready, Poll},
    time::Duration,
};

use async_trait::async_trait;
use futures::Stream;
use metrics::{
    atomics::AtomicU64, Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Level, Metadata,
    Recorder, SetRecorderError, SharedString, Unit,
};
use metrics_util::storage::AtomicBucket;
use saluki_common::{collections::FastHashMap, sync::shutdown::ShutdownHandle};
use saluki_context::{
    origin::RawOrigin,
    tags::{Tag, TagSet},
    Context, ContextResolver, ContextResolverBuilder,
};
use saluki_error::GenericError;
use tokio::{
    select,
    sync::broadcast::{self, error::RecvError, Receiver},
};
use tokio_util::sync::ReusableBoxFuture;
use tracing::debug;

use crate::{
    data_model::event::{metric::*, Event},
    runtime::{InitializationError, Supervisable, SupervisorFuture},
};

mod aggregated;
pub use self::aggregated::{
    get_shared_metrics_state, AggregatedMetricValue, AggregatedMetricsProcessor, AggregatedMetricsState,
};

mod histogram;
pub use self::histogram::AggregatedHistogram;

mod processor;
pub use self::processor::TelemetryProcessor;

mod reflector;
pub use self::reflector::{Processor, Reflector};

mod remapper;
pub use self::remapper::{RemappedMetric, RemapperRule};

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const INTERNAL_METRICS_INTERNER_SIZE: NonZeroUsize = NonZeroUsize::new(16_384).unwrap();

/// Number of consecutive flush intervals that a metric must go without a live handle and without being re-registered
/// before it is evicted from the registry.
///
/// This grace period prevents metrics that are emitted through `metrics` macros without caching the returned handle --
/// and that therefore re-register on every call -- from being churned in and out of the registry while they are still
/// being emitted. Naturally, this only applies to metrics which re-register within `IDLE_EVICT_INTERVALS *
/// FLUSH_INTERVAL`, which is 3 seconds by default... but that's a tradeoff we're making here to avoid unbounded growth.
const IDLE_EVICT_INTERVALS: u32 = 3;

static RECEIVER_STATE: OnceLock<Arc<State>> = OnceLock::new();

/// A batch of metric updates produced by a single flush.
#[derive(Clone, Default)]
pub struct MetricsSnapshot {
    /// Updates to active metrics.
    pub upserts: Vec<Event>,

    /// Metrics which are no longer active and should be removed.
    pub evictions: Vec<Context>,
}

/// A [`MetricsSnapshot`] that can be cheaply cloned and shared between multiple consumers.
pub type SharedMetricsSnapshot = Arc<MetricsSnapshot>;

struct Handle<T> {
    inner: T,
    level: Level,
    idle: AtomicU32,
}

impl<T> Handle<T> {
    const fn new(inner: T, level: Level) -> Self {
        Self {
            inner,
            level,
            idle: AtomicU32::new(0),
        }
    }

    fn level(&self) -> Level {
        self.level
    }

    fn reset_idle(&self) {
        self.idle.store(0, Ordering::Relaxed);
    }

    fn bump_idle(&self) -> u32 {
        self.idle.fetch_add(1, Ordering::Relaxed).saturating_add(1)
    }
}

struct CounterInner {
    value: AtomicU64,
}

impl Handle<CounterInner> {
    const fn new_counter(level: Level) -> Self {
        Self::new(
            CounterInner {
                value: AtomicU64::new(0),
            },
            level,
        )
    }

    /// Consumes the accumulated delta since the last flush, resetting the counter to zero.
    fn consume(&self) -> u64 {
        self.inner.value.swap(0, Ordering::Relaxed)
    }
}

impl CounterFn for Handle<CounterInner> {
    fn increment(&self, value: u64) {
        CounterFn::increment(&self.inner.value, value);
    }

    fn absolute(&self, value: u64) {
        CounterFn::absolute(&self.inner.value, value);
    }
}

struct GaugeInner {
    value: AtomicU64,
    // Tracks whether or not the gauge has been written since the last flush, since the gauge _could_
    // be modified in a way where the value seen by two consecutive flushes is the same, even though
    // it _was_ modified between the two and should be considered non-idle.
    dirty: AtomicBool,
}

impl Handle<GaugeInner> {
    const fn new_gauge(level: Level) -> Self {
        Self::new(
            GaugeInner {
                value: AtomicU64::new(0),
                dirty: AtomicBool::new(false),
            },
            level,
        )
    }

    /// Reads the current gauge value.
    fn load(&self) -> f64 {
        f64::from_bits(self.inner.value.load(Ordering::Relaxed))
    }

    /// Returns whether the gauge was written since the last flush, clearing the flag.
    fn take_dirty(&self) -> bool {
        self.inner.dirty.swap(false, Ordering::Relaxed)
    }
}

impl GaugeFn for Handle<GaugeInner> {
    fn increment(&self, value: f64) {
        GaugeFn::increment(&self.inner.value, value);
        self.inner.dirty.store(true, Ordering::Relaxed);
    }

    fn decrement(&self, value: f64) {
        GaugeFn::decrement(&self.inner.value, value);
        self.inner.dirty.store(true, Ordering::Relaxed);
    }

    fn set(&self, value: f64) {
        GaugeFn::set(&self.inner.value, value);
        self.inner.dirty.store(true, Ordering::Relaxed);
    }
}

/// Storage for a single histogram, shared between the registry and every caller-held [`Histogram`].
struct HistogramInner {
    value: AtomicBucket<f64>,
}

impl Handle<HistogramInner> {
    fn new_histogram(level: Level) -> Self {
        Self::new(
            HistogramInner {
                value: AtomicBucket::new(),
            },
            level,
        )
    }

    /// Drains all recorded samples since the last flush into `out`, clearing the histogram.
    fn drain_into(&self, out: &mut Vec<f64>) {
        self.inner.value.clear_with(|samples| out.extend(samples));
    }
}

impl HistogramFn for Handle<HistogramInner> {
    fn record(&self, value: f64) {
        self.inner.value.push(value);
    }
}

/// A registry for all internal metrics.
///
/// Optimized for simplicity and the ability to efficiently track metric state and evict idle metrics. Not optimized for
/// high concurrency with regards to registration, so metric handles should always be held for as long as possible, when
/// possible.
#[derive(Default)]
struct MetricsRegistry {
    maps: Mutex<RegistryMaps>,
}

#[derive(Default)]
struct RegistryMaps {
    counters: FastHashMap<Key, Arc<Handle<CounterInner>>>,
    gauges: FastHashMap<Key, Arc<Handle<GaugeInner>>>,
    histograms: FastHashMap<Key, Arc<Handle<HistogramInner>>>,
}

impl MetricsRegistry {
    /// Returns a handle to the counter for `key`, creating it at `level` if it doesn't yet exist.
    ///
    /// The level is fixed when the metric is first created; re-registration only refreshes the idle
    /// counter (the activity signal that keeps actively emitted metrics from being evicted).
    fn get_or_create_counter(&self, key: &Key, level: Level) -> Counter {
        let mut maps = self.maps.lock().unwrap();
        let handle = if let Some(handle) = maps.counters.get(key) {
            handle.reset_idle();
            Arc::clone(handle)
        } else {
            let handle = Arc::new(Handle::new_counter(level));
            maps.counters.insert(key.clone(), Arc::clone(&handle));
            handle
        };

        Counter::from_arc(handle)
    }

    /// Returns a handle to the gauge for `key`, creating it at `level` if it doesn't yet exist.
    fn get_or_create_gauge(&self, key: &Key, level: Level) -> Gauge {
        let mut maps = self.maps.lock().unwrap();
        let handle = if let Some(handle) = maps.gauges.get(key) {
            handle.reset_idle();
            Arc::clone(handle)
        } else {
            let handle = Arc::new(Handle::new_gauge(level));
            maps.gauges.insert(key.clone(), Arc::clone(&handle));
            handle
        };

        Gauge::from_arc(handle)
    }

    /// Returns a handle to the histogram for `key`, creating it at `level` if it doesn't yet exist.
    fn get_or_create_histogram(&self, key: &Key, level: Level) -> Histogram {
        let mut maps = self.maps.lock().unwrap();
        let handle = if let Some(handle) = maps.histograms.get(key) {
            handle.reset_idle();
            Arc::clone(handle)
        } else {
            let handle = Arc::new(Handle::new_histogram(level));
            maps.histograms.insert(key.clone(), Arc::clone(&handle));
            handle
        };

        Histogram::from_arc(handle)
    }
}

/// Handle to the metrics filter.
///
/// Allows for overriding the current metrics filter level, which influences which metrics are emitted to downstream
/// receivers.
pub struct FilterHandle {
    state: Arc<State>,
}

impl FilterHandle {
    /// Overrides the current metrics filter level.
    pub fn override_filter(&self, level: Level) {
        *self.state.current_level.lock().unwrap() = level;
    }

    /// Resets the metrics filter level to the default that was configured when the metrics subsystem was initialized.
    pub fn reset_filter(&self) {
        *self.state.current_level.lock().unwrap() = self.state.default_level;
    }
}

struct State {
    registry: MetricsRegistry,
    flush_tx: broadcast::Sender<SharedMetricsSnapshot>,
    metrics_prefix: String,
    default_level: Level,
    current_level: Mutex<Level>,
    idle_evict_intervals: u32,
    flush_interval: Duration,
}

struct MetricsRecorder {
    state: Arc<State>,
}

impl MetricsRecorder {
    fn new(metrics_prefix: String, default_level: Level) -> Self {
        let (flush_tx, _) = broadcast::channel(2);
        Self {
            state: Arc::new(State {
                registry: MetricsRegistry::default(),
                flush_tx,
                metrics_prefix,
                default_level,
                current_level: Mutex::new(default_level),
                idle_evict_intervals: IDLE_EVICT_INTERVALS,
                flush_interval: FLUSH_INTERVAL,
            }),
        }
    }

    fn filter_handle(&self) -> FilterHandle {
        FilterHandle {
            state: Arc::clone(&self.state),
        }
    }

    fn install(self) -> Result<(), SetRecorderError<Self>> {
        let state = Arc::clone(&self.state);
        metrics::set_global_recorder(self)?;

        if RECEIVER_STATE.set(state).is_err() {
            panic!("metrics receiver should never be set prior to global recorder being installed");
        }

        Ok(())
    }

    fn prefix_key(&self, key: &Key) -> Key {
        Key::from_parts(format!("{}.{}", self.state.metrics_prefix, key.name()), key.labels())
    }
}

impl Recorder for MetricsRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_counter(&prefixed_key, *metadata.level())
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_gauge(&prefixed_key, *metadata.level())
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_histogram(&prefixed_key, *metadata.level())
    }
}

/// Internal metrics stream
///
/// Used to receive periodic snapshots of the internal metrics registry, which contains all metrics that are currently
/// active within the process.
pub struct MetricsStream {
    inner: ReusableBoxFuture<
        'static,
        (
            Result<SharedMetricsSnapshot, RecvError>,
            Receiver<SharedMetricsSnapshot>,
        ),
    >,
}

impl MetricsStream {
    /// Creates a new `MetricsStream` that receives updates from the internal metrics registry.
    pub fn register() -> Self {
        let state = RECEIVER_STATE.get().expect("metrics receiver should be set");
        Self {
            inner: ReusableBoxFuture::new(make_rx_future(state.flush_tx.subscribe())),
        }
    }
}

impl Stream for MetricsStream {
    type Item = SharedMetricsSnapshot;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Poll the receiver, and rearm the future once we actually resolve it.
            let (result, rx) = ready!(self.inner.poll(cx));
            self.inner.set(make_rx_future(rx));

            match result {
                Ok(item) => return Poll::Ready(Some(item)),
                Err(RecvError::Closed) => return Poll::Ready(None),
                Err(RecvError::Lagged(n)) => {
                    debug!(
                        missed_payloads = n,
                        "Stream lagging behind internal metrics producer. Internal metrics may have been lost."
                    );
                    continue;
                }
            }
        }
    }
}

async fn make_rx_future(
    mut rx: Receiver<SharedMetricsSnapshot>,
) -> (
    Result<SharedMetricsSnapshot, RecvError>,
    Receiver<SharedMetricsSnapshot>,
) {
    let result = rx.recv().await;
    (result, rx)
}

#[derive(Default)]
struct FlushState {
    counter_emits: Vec<(Key, f64)>,
    gauge_emits: Vec<(Key, f64)>,
    histogram_emits: Vec<(Key, Vec<f64>)>,
    evicted_keys: Vec<Key>,
}

impl FlushState {
    fn clear(&mut self) {
        self.counter_emits.clear();
        self.gauge_emits.clear();
        self.histogram_emits.clear();
        self.evicted_keys.clear();
    }
}

async fn flush_metrics() {
    let mut context_resolver = MetricsContextResolver::new(INTERNAL_METRICS_INTERNER_SIZE);

    let state = RECEIVER_STATE.get().expect("metrics receiver should be set");

    let mut flush_interval = tokio::time::interval(state.flush_interval);
    flush_interval.tick().await;

    let mut flush_state = FlushState::default();

    loop {
        flush_interval.tick().await;
        flush_once(state, &mut context_resolver, &mut flush_state);
    }
}

/// Performs a single flush pass over the registry: consumes each metric's value, evicts metrics that
/// are no longer referenced and have gone idle, and broadcasts the resulting metric values and
/// evictions to downstream consumers.
fn flush_once(state: &State, context_resolver: &mut MetricsContextResolver, flush_state: &mut FlushState) {
    let has_listeners = state.flush_tx.receiver_count() > 0;
    let current_level = *state.current_level.lock().unwrap();
    let idle_threshold = state.idle_evict_intervals;
    let mut snapshot = MetricsSnapshot::default();

    flush_state.clear();
    let FlushState {
        counter_emits,
        gauge_emits,
        histogram_emits,
        evicted_keys,
    } = flush_state;

    {
        let mut maps = state.registry.maps.lock().unwrap();

        // Counters: a counter is never idle so long as it has a non-zero delta during a flush _or_ has
        // an active reference (strong count > 1).
        //
        // The strong-count check precedes `consume()`, with an Acquire fence on the orphaned path:
        // observing `strong_count == 1` synchronizes-with the dropping thread's release, which
        // guarantees the following `consume()` observes that thread's final increment.
        maps.counters.retain(|key, handle| {
            let idle = handle.bump_idle();
            let orphaned = Arc::strong_count(handle) == 1;
            if orphaned {
                std::sync::atomic::fence(Ordering::Acquire);
            }
            let delta = handle.consume();

            if orphaned && delta == 0 && idle >= idle_threshold {
                evicted_keys.push(key.clone());
                return false;
            }

            if has_listeners && handle.level() >= current_level {
                counter_emits.push((key.clone(), delta as f64));
            }
            true
        });

        // Gauges: a gauge is never idle so long as it was touched ("dirty") prior to a flush _or_ has
        // an active reference (strong count > 1).
        maps.gauges.retain(|key, handle| {
            let idle = handle.bump_idle();
            let orphaned = Arc::strong_count(handle) == 1;
            if orphaned {
                std::sync::atomic::fence(Ordering::Acquire);
            }
            let value = handle.load();
            let written = handle.take_dirty();

            if orphaned && !written && idle >= idle_threshold {
                evicted_keys.push(key.clone());
                return false;
            }

            if has_listeners && handle.level() >= current_level {
                gauge_emits.push((key.clone(), value));
            }
            true
        });

        // Histograms: a histogram is never idle so long as it has recorded samples prior to a flush _or_
        // has an active reference (strong count > 1).
        //
        // The strong-count check precedes the drain, with an Acquire fence on the orphaned path, so the
        // drain observes a dropping thread's final `record()`.
        maps.histograms.retain(|key, handle| {
            let idle = handle.bump_idle();
            let orphaned = Arc::strong_count(handle) == 1;
            if orphaned {
                std::sync::atomic::fence(Ordering::Acquire);
            }
            let mut histogram_samples = Vec::new();
            handle.drain_into(&mut histogram_samples);
            let had_samples = !histogram_samples.is_empty();

            if orphaned && !had_samples && idle >= idle_threshold {
                evicted_keys.push(key.clone());
                return false;
            }

            if has_listeners && had_samples && handle.level() >= current_level {
                histogram_emits.push((key.clone(), histogram_samples));
            }
            true
        });
    }

    // For every evicted key we collected, take its cached context (if it exists) and add it to the
    // snapshot's evictions list.
    for key in evicted_keys {
        if let Some(context) = context_resolver.take(key) {
            if has_listeners {
                snapshot.evictions.push(context);
            }
        }
    }

    if !has_listeners {
        return;
    }

    for (key, value) in counter_emits.drain(..) {
        let context = context_resolver.resolve_from_key(key);
        snapshot.upserts.push(Event::Metric(Metric::counter(context, value)));
    }
    for (key, value) in gauge_emits.drain(..) {
        let context = context_resolver.resolve_from_key(key);
        snapshot.upserts.push(Event::Metric(Metric::gauge(context, value)));
    }
    for (key, samples) in histogram_emits.drain(..) {
        let context = context_resolver.resolve_from_key(key);
        snapshot
            .upserts
            .push(Event::Metric(Metric::histogram(context, &samples[..])));
    }

    if !snapshot.upserts.is_empty() || !snapshot.evictions.is_empty() {
        let _ = state.flush_tx.send(Arc::new(snapshot));
    }
}

struct MetricsContextResolver {
    context_resolver: ContextResolver,
    key_context_cache: FastHashMap<Key, Context>,
}

impl MetricsContextResolver {
    fn new(resolver_interner_size_bytes: NonZeroUsize) -> Self {
        Self {
            // Set up our context resolver without caching, since we will be caching the contexts ourselves.
            context_resolver: ContextResolverBuilder::from_name("core/internal_metrics")
                .expect("resolver name is not empty")
                .with_interner_capacity_bytes(resolver_interner_size_bytes)
                .without_caching()
                .build(),
            key_context_cache: FastHashMap::default(),
        }
    }

    fn resolve_from_key(&mut self, key: Key) -> Context {
        static SELF_ORIGIN_INFO: LazyLock<RawOrigin<'static>> = LazyLock::new(|| {
            let mut origin_info = RawOrigin::default();
            origin_info.set_process_id(std::process::id());
            origin_info
        });

        // Check the cache first.
        if let Some(context) = self.key_context_cache.get(&key) {
            return context.clone();
        }

        // We don't have the context cached, so we need to resolve it.
        let tags = key
            .labels()
            .map(|l| Tag::from(format!("{}:{}", l.key(), l.value())))
            .collect::<TagSet>();

        let context = self
            .context_resolver
            .resolve(key.name(), &tags, Some(SELF_ORIGIN_INFO.clone()))
            .expect("resolver should always allow falling back");

        self.key_context_cache.insert(key, context.clone());
        context
    }

    /// Removes a key's cached context, returning it if present.
    ///
    /// Returns `Some` only if the key had been resolved before (that is, the metric was emitted at
    /// least once), which is what tells the flush loop whether a downstream eviction needs to be sent.
    fn take(&mut self, key: &Key) -> Option<Context> {
        self.key_context_cache.remove(key)
    }
}

/// Initializes the metrics subsystem with the given metrics prefix and default filter level.
///
/// `default_level` sets the initial filter level for emitted metrics, and is also what the filter is restored to when
/// [`FilterHandle::reset_filter`] is invoked. Metrics whose level is more verbose than this default are filtered out
/// until a runtime override is applied via [`FilterHandle::override_filter`].
///
/// Returns a [`FilterHandle`] for adjusting the runtime metrics filter, plus a [`MetricsFlusherWorker`]
/// that must be added to a [`Supervisor`][crate::runtime::Supervisor] in order to drive the periodic
/// flush loop. Internal metrics aren't propagated to subscribers until the worker is running.
///
/// # Errors
///
/// If a global recorder was already installed, an error will be returned.
pub async fn initialize_metrics(
    metrics_prefix: String, default_level: Level,
) -> Result<(FilterHandle, MetricsFlusherWorker), GenericError> {
    let recorder = MetricsRecorder::new(metrics_prefix, default_level);
    let filter_handle = recorder.filter_handle();
    recorder.install()?;

    Ok((filter_handle, MetricsFlusherWorker))
}

/// A worker that periodically flushes the internal metrics registry to broadcast subscribers.
///
/// Wraps the internal flush loop and runs it under a [`Supervisor`][crate::runtime::Supervisor]. Must
/// only be added to a supervisor after [`initialize_metrics`] has been called -- the flush loop
/// reads from the global recorder state set by that call.
pub struct MetricsFlusherWorker;

#[async_trait]
impl Supervisable for MetricsFlusherWorker {
    fn name(&self) -> &str {
        "internal-telemetry-metrics-flusher"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => {},
                _ = flush_metrics() => {},
            }

            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // We keep a live receiver in each test so `flush_once` observes a listener and emits updates.
    fn make_state(idle_evict_intervals: u32) -> (Arc<State>, Receiver<SharedMetricsSnapshot>) {
        let (flush_tx, rx) = broadcast::channel(64);
        let state = Arc::new(State {
            registry: MetricsRegistry::default(),
            flush_tx,
            metrics_prefix: "test".to_string(),
            default_level: Level::TRACE,
            current_level: Mutex::new(Level::TRACE),
            idle_evict_intervals,
            flush_interval: FLUSH_INTERVAL,
        });
        (state, rx)
    }

    fn resolver() -> MetricsContextResolver {
        MetricsContextResolver::new(INTERNAL_METRICS_INTERNER_SIZE)
    }

    // Mimics `MetricsRecorder::register_counter` against a raw key (no prefixing needed in tests).
    fn register_counter(state: &State, name: &'static str) -> Counter {
        state
            .registry
            .get_or_create_counter(&Key::from_name(name), Level::TRACE)
    }

    fn register_gauge(state: &State, name: &'static str) -> Gauge {
        state.registry.get_or_create_gauge(&Key::from_name(name), Level::TRACE)
    }

    fn counter_present(state: &State, name: &'static str) -> bool {
        state
            .registry
            .maps
            .lock()
            .unwrap()
            .counters
            .contains_key(&Key::from_name(name))
    }

    fn gauge_present(state: &State, name: &'static str) -> bool {
        state
            .registry
            .maps
            .lock()
            .unwrap()
            .gauges
            .contains_key(&Key::from_name(name))
    }

    fn drain(rx: &mut Receiver<SharedMetricsSnapshot>) -> Vec<MetricsSnapshot> {
        let mut out = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            out.push((*batch).clone());
        }
        out
    }

    fn evicted_names(snapshots: &[MetricsSnapshot]) -> Vec<String> {
        snapshots
            .iter()
            .flat_map(|snapshot| snapshot.evictions.iter().map(|ctx| ctx.name().to_string()))
            .collect()
    }

    #[tokio::test]
    async fn evicts_counter_within_bounded_time_after_handle_dropped() {
        let (state, mut rx) = make_state(3);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        let counter = register_counter(&state, "dropped_counter");
        counter.increment(5);

        // While the handle is held, the metric is retained and its delta is emitted.
        flush_once(&state, &mut resolver, &mut flush_state);
        assert!(counter_present(&state, "dropped_counter"));

        // Drop the last handle; the metric must be reclaimed within `idle_evict_intervals` flushes.
        drop(counter);
        let _ = drain(&mut rx);
        for _ in 0..3 {
            flush_once(&state, &mut resolver, &mut flush_state);
        }
        assert!(!counter_present(&state, "dropped_counter"));

        // The eviction is propagated downstream so the aggregated view drops it too.
        let updates = drain(&mut rx);
        assert!(evicted_names(&updates).iter().any(|n| n == "dropped_counter"));
    }

    #[tokio::test]
    async fn does_not_evict_while_handle_is_held() {
        let (state, _rx) = make_state(3);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        let counter = register_counter(&state, "held_counter");

        // Idle well past the eviction threshold without dropping the handle.
        for _ in 0..8 {
            flush_once(&state, &mut resolver, &mut flush_state);
        }

        assert!(
            counter_present(&state, "held_counter"),
            "a held metric must never be evicted"
        );
        drop(counter);
    }

    #[tokio::test]
    async fn reregistration_keeps_uncached_metric_alive() {
        // Models the uncached-macro pattern: a fresh handle is registered (and dropped) every interval.
        // Re-registration resets the idle counter, so the metric is never churned out while in use.
        let (state, _rx) = make_state(3);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        for _ in 0..8 {
            let counter = register_counter(&state, "uncached_counter");
            counter.increment(1);
            drop(counter);
            flush_once(&state, &mut resolver, &mut flush_state);
        }
        assert!(counter_present(&state, "uncached_counter"));

        // Once it stops being re-registered, it is reclaimed within the idle threshold.
        for _ in 0..3 {
            flush_once(&state, &mut resolver, &mut flush_state);
        }
        assert!(!counter_present(&state, "uncached_counter"));
    }

    #[tokio::test]
    async fn gauge_reset_to_same_value_is_not_evicted() {
        // A gauge re-set to the same value every interval (e.g. a backoff gauge that sits at 0.0) must
        // stay alive. Activity is driven by re-registration, not value comparison.
        let (state, _rx) = make_state(3);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        for _ in 0..8 {
            let gauge = register_gauge(&state, "steady_gauge");
            gauge.set(0.0);
            drop(gauge);
            flush_once(&state, &mut resolver, &mut flush_state);
        }
        assert!(gauge_present(&state, "steady_gauge"));
    }

    #[tokio::test]
    async fn final_counter_delta_reaches_downstream_before_eviction() {
        let (state, mut rx) = make_state(1);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        let counter = register_counter(&state, "final_counter");

        // Arm eviction by letting the idle counter reach the threshold while the handle is held.
        flush_once(&state, &mut resolver, &mut flush_state);

        // Increment and drop the handle in the same inter-flush window. The next flush sees a non-zero
        // delta, so it must emit it and defer eviction by one interval (so the delta isn't lost).
        counter.increment(7);
        drop(counter);
        flush_once(&state, &mut resolver, &mut flush_state);
        assert!(
            counter_present(&state, "final_counter"),
            "must not evict in an interval that produced a delta"
        );

        // The emitted delta must reach the downstream aggregated state.
        let agg_processor = AggregatedMetricsProcessor;
        let agg_state = agg_processor.build_initial_state();
        for update in drain(&mut rx) {
            agg_processor.process(update, &agg_state);
        }
        assert_eq!(agg_state.get_aggregated_with_tags("final_counter", &[]), 7.0);

        // The following interval (no delta) finally evicts it.
        flush_once(&state, &mut resolver, &mut flush_state);
        assert!(!counter_present(&state, "final_counter"));
    }

    #[tokio::test]
    async fn gauge_final_value_reaches_downstream_before_eviction() {
        let (state, mut rx) = make_state(1);
        let mut resolver = resolver();
        let mut flush_state = FlushState::default();

        let gauge = register_gauge(&state, "final_gauge");
        gauge.set(1.0);

        // Arm eviction by letting the idle counter reach the threshold while the handle is held.
        flush_once(&state, &mut resolver, &mut flush_state);

        // Update to a NEW value and drop the handle in the same inter-flush window. Even though the
        // gauge is now orphaned and idle, the write must not be lost: the next flush sees the dirty flag,
        // emits the value, and defers eviction by one interval.
        gauge.set(42.0);
        drop(gauge);
        flush_once(&state, &mut resolver, &mut flush_state);
        assert!(
            gauge_present(&state, "final_gauge"),
            "must not evict in an interval that wrote a new value"
        );

        // The final value must reach the downstream aggregated state.
        let agg_processor = AggregatedMetricsProcessor;
        let agg_state = agg_processor.build_initial_state();
        for update in drain(&mut rx) {
            agg_processor.process(update, &agg_state);
        }
        assert_eq!(agg_state.find_single_with_tags("final_gauge", &[]), Some(42.0));

        // The following interval (no write) finally evicts it.
        flush_once(&state, &mut resolver, &mut flush_state);
        assert!(!gauge_present(&state, "final_gauge"));
    }
}

// Loom model of the counter eviction path in `flush_once`.
//
// In `flush_once`, the registry `maps` lock is held during the drain, but the increment/drop path
// does not take that lock: `Counter::increment` writes straight through the `Arc<Handle>`, and
// dropping a caller's `Counter` only decrements the Arc strong count. A component holding a cached
// handle can therefore land a final increment and drop the handle while a flush is mid-pass over it.
//
// The model transcribes the counter branch with loom primitives instead of exercising the real types,
// which loom cannot instrument here:
//
// - The strong count is an explicit `AtomicUsize`. loom's `Arc::strong_count` does not reflect a
//   concurrent decrement from another thread (it models Arc's drop synchronization, not the observable
//   count value), so a flush reading it never sees the orphaned (`== 1`) state mid-race. The explicit
//   atomic matches `std`'s `Arc`: clone increments (Relaxed), drop decrements with `Release`,
//   observation is a `Relaxed` load -- the same shape as how `flush_once` reads `Arc::strong_count`.
// - The real `Handle` is wrapped by the `metrics` crate's `Counter`/`CounterFn`, which construct
//   `std::sync::Arc` internally. loom only instruments atomics and `Arc`s swapped to `loom::sync::*`
//   behind the `loom` cfg, not those inside a third-party crate.
//
// `flush_decision_consume_first` and `flush_decision_strong_count_first` mirror the two possible
// orderings of `flush_once`'s counter branch and must stay in lockstep with it.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
    use loom::sync::Arc;

    /// The shared state behind a counter handle: the accumulator the flush loop drains, and the Arc
    /// strong count it consults to decide whether the handle is orphaned.
    struct Shared {
        value: AtomicU64,
        strong: AtomicUsize,
    }

    /// A component holding a cached handle lands one final increment, then drops the handle.
    fn caller_increment_then_drop(shared: &Arc<Shared>, amount: u64) {
        shared.value.fetch_add(amount, Ordering::Relaxed);
        shared.strong.fetch_sub(1, Ordering::Release); // Arc clone drop
    }

    /// Consumes the value, then checks the strong count (value read before the liveness check).
    fn flush_decision_consume_first(shared: &Arc<Shared>) -> (u64, bool) {
        let delta = shared.value.swap(0, Ordering::Relaxed);
        let orphaned = shared.strong.load(Ordering::Relaxed) == 1;
        (delta, orphaned && delta == 0)
    }

    /// Checks the strong count first; if orphaned, Acquire-fences to synchronize with the dropping
    /// thread's `Release`, then consumes the value.
    fn flush_decision_strong_count_first(shared: &Arc<Shared>) -> (u64, bool) {
        let orphaned = shared.strong.load(Ordering::Relaxed) == 1;
        if orphaned {
            fence(Ordering::Acquire);
        }
        let delta = shared.value.swap(0, Ordering::Relaxed);
        (delta, orphaned && delta == 0)
    }

    fn model(decision: fn(&Arc<Shared>) -> (u64, bool)) {
        loom::model(move || {
            const FINAL: u64 = 7;

            // strong count starts at 2: one ref in the registry map, one held by the caller.
            let shared = Arc::new(Shared {
                value: AtomicU64::new(0),
                strong: AtomicUsize::new(2),
            });
            let caller_shared = Arc::clone(&shared);

            let caller = loom::thread::spawn(move || {
                caller_increment_then_drop(&caller_shared, FINAL);
            });

            // The flush task evaluates this counter while the component races.
            let (delta, evicted) = decision(&shared);
            caller.join().unwrap();

            // Invariant: a counter may only be evicted once everything it accumulated has been
            // emitted. If the flush evicts it, the delta it emitted downstream must already include
            // the final increment -- otherwise those counts are silently discarded with the handle.
            if evicted {
                assert_eq!(
                    delta,
                    FINAL,
                    "counter evicted while {} counts were never emitted -> permanently lost",
                    FINAL - delta
                );
            }
        });
    }

    // Consuming the value before checking the strong count is lossy: loom finds an interleaving where
    // the handle is evicted while a final increment goes unemitted. `#[should_panic]` asserts loom
    // reaches that interleaving.
    #[test]
    #[should_panic(expected = "permanently lost")]
    fn consume_first_ordering_loses_a_final_increment() {
        model(flush_decision_consume_first);
    }

    // Checking the strong count first (Acquire-fencing when orphaned) before consuming preserves every
    // increment across all interleavings loom explores.
    #[test]
    fn strong_count_first_ordering_preserves_a_final_increment() {
        model(flush_decision_strong_count_first);
    }
}

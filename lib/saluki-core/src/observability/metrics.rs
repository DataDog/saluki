//! Internal metrics support.
use std::{
    num::NonZeroUsize,
    pin::Pin,
    sync::{atomic::Ordering, Arc, LazyLock, Mutex, OnceLock},
    task::{self, ready, Poll},
    time::Duration,
};

use futures::Stream;
use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Level, Metadata, Recorder, SetRecorderError, SharedString, Unit,
};
use metrics_util::registry::{AtomicStorage, Registry};
use saluki_common::{
    collections::{FastConcurrentHashMap, FastHashMap},
    task::spawn_traced_named,
};
use saluki_context::{
    origin::RawOrigin,
    tags::{Tag, TagSet},
    Context, ContextResolver, ContextResolverBuilder,
};
use tokio::sync::broadcast::{self, error::RecvError, Receiver};
use tokio_util::sync::ReusableBoxFuture;
use tracing::debug;

use crate::data_model::event::{metric::*, Event};

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const INTERNAL_METRICS_INTERNER_SIZE: NonZeroUsize = NonZeroUsize::new(16_384).unwrap();

static RECEIVER_STATE: OnceLock<Arc<State>> = OnceLock::new();

/// A collection of events that can be cheaply cloned and shared between multiple consumers.
pub type SharedEvents = Arc<Vec<Event>>;

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
        *self.state.current_level.lock().unwrap() = Some(level);
    }

    /// Resets the metrics filter level to the default (INFO).
    pub fn reset_filter(&self) {
        *self.state.current_level.lock().unwrap() = None;
    }
}

struct State {
    registry: Registry<Key, AtomicStorage>,
    level_map: FastConcurrentHashMap<Key, Level>,
    flush_tx: broadcast::Sender<SharedEvents>,
    metrics_prefix: String,
    current_level: Mutex<Option<Level>>,
}

impl State {
    fn is_metric_filtered(&self, key: &Key, filter_level: &Level) -> bool {
        match self.level_map.pin().get(key) {
            // We have to know about a metric to be sure it's allowed.
            None => true,

            // Higher verbosity levels have lower values, so if the metric's level is lower than our filter level, that
            // means we're actively filtering it out.
            Some(level) => level < filter_level,
        }
    }
}

struct MetricsRecorder {
    state: Arc<State>,
}

impl MetricsRecorder {
    fn new(metrics_prefix: String) -> Self {
        let (flush_tx, _) = broadcast::channel(2);
        Self {
            state: Arc::new(State {
                registry: Registry::new(AtomicStorage),
                level_map: FastConcurrentHashMap::default(),
                flush_tx,
                metrics_prefix,
                current_level: Mutex::new(None),
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
        let handle = self
            .state
            .registry
            .get_or_create_counter(&prefixed_key, |c| c.clone().into());
        self.state.level_map.pin().insert(prefixed_key, *metadata.level());

        handle
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        let prefixed_key = self.prefix_key(key);
        let handle = self
            .state
            .registry
            .get_or_create_gauge(&prefixed_key, |g| g.clone().into());
        self.state.level_map.pin().insert(prefixed_key, *metadata.level());

        handle
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        let prefixed_key = self.prefix_key(key);
        let handle = self
            .state
            .registry
            .get_or_create_histogram(&prefixed_key, |h| h.clone().into());
        self.state.level_map.pin().insert(prefixed_key, *metadata.level());

        handle
    }
}

/// Internal metrics stream
///
/// Used to receive periodic snapshots of the internal metrics registry, which contains all metrics that are currently
/// active within the process.
pub struct MetricsStream {
    inner: ReusableBoxFuture<'static, (Result<SharedEvents, RecvError>, Receiver<SharedEvents>)>,
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
    type Item = SharedEvents;

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

async fn make_rx_future(mut rx: Receiver<SharedEvents>) -> (Result<SharedEvents, RecvError>, Receiver<SharedEvents>) {
    let result = rx.recv().await;
    (result, rx)
}

async fn flush_metrics(flush_interval: Duration) {
    let mut context_resolver = MetricsContextResolver::new(INTERNAL_METRICS_INTERNER_SIZE);

    let mut flush_interval = tokio::time::interval(flush_interval);
    flush_interval.tick().await;

    let state = RECEIVER_STATE.get().expect("metrics receiver should be set");

    let mut histogram_samples = Vec::<f64>::new();

    loop {
        flush_interval.tick().await;

        // If we have no downstream listeners, just clear our histograms so they don't accumulate memory forever.
        if state.flush_tx.receiver_count() == 0 {
            let histograms = state.registry.get_histogram_handles();
            for (_, histogram) in histograms {
                histogram.clear();
            }
            continue;
        }

        let mut metrics = Vec::new();
        let current_level = {
            let current_level = state.current_level.lock().unwrap();
            current_level.as_ref().copied().unwrap_or(Level::TRACE)
        };

        let counters = state.registry.get_counter_handles();
        let gauges = state.registry.get_gauge_handles();
        let histograms = state.registry.get_histogram_handles();

        for (key, counter) in counters {
            if state.is_metric_filtered(&key, &current_level) {
                continue;
            }

            let context = context_resolver.resolve_from_key(key);
            let value = counter.swap(0, Ordering::Relaxed) as f64;

            let metric = Metric::counter(context, value);
            metrics.push(Event::Metric(metric));
        }

        for (key, gauge) in gauges {
            if state.is_metric_filtered(&key, &current_level) {
                continue;
            }

            let context = context_resolver.resolve_from_key(key);
            let value = f64::from_bits(gauge.load(Ordering::Relaxed));

            let metric = Metric::gauge(context, value);
            metrics.push(Event::Metric(metric));
        }

        for (key, histogram) in histograms {
            if state.is_metric_filtered(&key, &current_level) {
                continue;
            }

            let context = context_resolver.resolve_from_key(key);

            // Collect all of the samples from the histogram.
            //
            // If the histogram was empty, skip emitting a metric for this histogram entirely. Empty sketches don't make
            // sense to send.
            histogram_samples.clear();
            histogram.clear_with(|samples| histogram_samples.extend(samples));

            if histogram_samples.is_empty() {
                continue;
            }

            let metric = Metric::histogram(context, &histogram_samples[..]);
            metrics.push(Event::Metric(metric));
        }

        let shared = Arc::new(metrics);
        let _ = state.flush_tx.send(shared);
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
}

/// Initializes the metrics subsystem with the given metrics prefix.
///
/// ## Errors
///
/// If a global recorder was already installed, an error will be returned.
pub async fn initialize_metrics(
    metrics_prefix: String,
) -> Result<FilterHandle, Box<dyn std::error::Error + Send + Sync>> {
    let recorder = MetricsRecorder::new(metrics_prefix);
    let filter_handle = recorder.filter_handle();
    recorder.install()?;

    spawn_traced_named("internal-telemetry-metrics-flusher", flush_metrics(FLUSH_INTERVAL));

    Ok(filter_handle)
}

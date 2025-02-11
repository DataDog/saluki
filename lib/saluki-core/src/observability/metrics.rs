//! Internal metrics support.
use std::{
    num::NonZeroUsize,
    sync::{atomic::Ordering, Arc, LazyLock, OnceLock},
    time::Duration,
};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SetRecorderError, SharedString, Unit};
use metrics_util::registry::{AtomicStorage, Registry};
use saluki_context::{
    origin::RawOrigin,
    tags::{Tag, TagSet},
    Context, ContextResolver, ContextResolverBuilder,
};
use saluki_event::{metric::*, Event};
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::debug;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const INTERNAL_METRICS_INTERNER_SIZE: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(8192) };

static RECEIVER_STATE: OnceLock<Arc<State>> = OnceLock::new();

struct State {
    registry: Registry<Key, AtomicStorage>,
    flush_tx: broadcast::Sender<Arc<Vec<Event>>>,
    metrics_prefix: String,
}

struct MetricsRecorder {
    state: Arc<State>,
}

impl MetricsRecorder {
    fn new(metrics_prefix: String) -> Self {
        let (flush_tx, _) = broadcast::channel(2);
        Self {
            state: Arc::new(State {
                registry: Registry::new(AtomicStorage {}),
                flush_tx,
                metrics_prefix,
            }),
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

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_counter(&prefixed_key, |c| c.clone().into())
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_gauge(&prefixed_key, |g| g.clone().into())
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let prefixed_key = self.prefix_key(key);
        self.state
            .registry
            .get_or_create_histogram(&prefixed_key, |h| h.clone().into())
    }
}

/// Internal metrics receiver.
///
/// Used to receive periodic snapshots of the internal metrics registry, which contains all metrics that are currently
/// active within the process.
pub struct MetricsReceiver {
    flush_rx: broadcast::Receiver<Arc<Vec<Event>>>,
}

impl MetricsReceiver {
    /// Creates a new `MetricsReceiver` and registers it for updates.
    pub fn register() -> Self {
        let state = RECEIVER_STATE.get().expect("metrics receiver should be set");
        Self {
            flush_rx: state.flush_tx.subscribe(),
        }
    }

    /// Waits for the next metrics snapshot.
    pub async fn next(&mut self) -> Arc<Vec<Event>> {
        loop {
            match self.flush_rx.recv().await {
                Ok(metrics) => return metrics,
                Err(RecvError::Closed) => {
                    panic!("metrics receiver should never be closed");
                }
                Err(RecvError::Lagged(missed_payloads)) => {
                    debug!(
                        missed_payloads,
                        "Receiver lagging behind internal metrics producer. Internal metrics may have been lost."
                    );
                }
            }
        }
    }
}

async fn flush_metrics(flush_interval: Duration) {
    // TODO: This is only worth 8KB, but it would be good to find a proper spot to tie this into the memory
    // bounds/accounting stuff since we currently just initialize metrics (and logging) with all-inclusive free
    // functions that come way before we even construct a topology.
    let mut context_resolver = ContextResolverBuilder::from_name("internal_metrics")
        .expect("resolver name is not empty")
        .with_interner_capacity_bytes(INTERNAL_METRICS_INTERNER_SIZE)
        .build();

    let mut flush_interval = tokio::time::interval(flush_interval);
    flush_interval.tick().await;

    let state = RECEIVER_STATE.get().expect("metrics receiver should be set");

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

        let counters = state.registry.get_counter_handles();
        let gauges = state.registry.get_gauge_handles();
        let histograms = state.registry.get_histogram_handles();

        for (key, counter) in counters {
            let context = context_from_key(&mut context_resolver, key);
            let value = counter.swap(0, Ordering::Relaxed) as f64;

            let metric = Metric::counter(context, value);
            metrics.push(Event::Metric(metric));
        }

        for (key, gauge) in gauges {
            let context = context_from_key(&mut context_resolver, key);
            let value = f64::from_bits(gauge.load(Ordering::Relaxed));

            let metric = Metric::gauge(context, value);
            metrics.push(Event::Metric(metric));
        }

        for (key, histogram) in histograms {
            let context = context_from_key(&mut context_resolver, key);

            // Collect all of the samples from the histogram.
            //
            // If the histogram was empty, skip emitting a metric for this histogram entirely. Empty sketches don't make
            // sense to send.
            let mut distribution_samples = Vec::<f64>::new();
            histogram.clear_with(|samples| distribution_samples.extend(samples));

            if distribution_samples.is_empty() {
                continue;
            }

            let metric = Metric::distribution(context, &distribution_samples[..]);
            metrics.push(Event::Metric(metric));
        }

        let shared = Arc::new(metrics);
        let _ = state.flush_tx.send(shared);
    }
}

fn context_from_key(context_resolver: &mut ContextResolver, key: Key) -> Context {
    let tags = key
        .labels()
        .map(|l| Tag::from(format!("{}:{}", l.key(), l.value())))
        .collect::<TagSet>();

    context_resolver
        .resolve(key.name(), &tags, Some(internal_telemetry_origin()))
        .expect("resolver should always allow falling back")
}

fn internal_telemetry_origin() -> RawOrigin<'static> {
    static SELF_ORIGIN_INFO: LazyLock<RawOrigin<'static>> = LazyLock::new(|| {
        let mut origin_info = RawOrigin::default();
        origin_info.set_process_id(std::process::id());
        origin_info
    });

    (*SELF_ORIGIN_INFO).clone()
}

/// Initializes the metrics subsystem with the given metrics prefix.
///
/// ## Errors
///
/// If a global recorder was already installed, an error will be returned.
pub async fn initialize_metrics(metrics_prefix: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let recorder = MetricsRecorder::new(metrics_prefix);
    recorder.install()?;

    tokio::spawn(flush_metrics(FLUSH_INTERVAL));

    Ok(())
}

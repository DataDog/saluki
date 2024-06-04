use std::{
    num::NonZeroUsize,
    sync::{atomic::Ordering, Arc, OnceLock},
    time::Duration,
};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SetRecorderError, SharedString, Unit};
use metrics_util::registry::{AtomicStorage, Registry};
use saluki_context::{Context, ContextRef, ContextResolver};
use stringtheory::interning::FixedSizeInterner;
use tokio::sync::broadcast;

use saluki_env::time::get_unix_timestamp;
use saluki_event::{metric::*, Event};

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

pub struct MetricsReceiver {
    flush_rx: broadcast::Receiver<Arc<Vec<Event>>>,
}

impl MetricsReceiver {
    pub fn register() -> Self {
        let state = RECEIVER_STATE.get().expect("metrics receiver should be set");
        Self {
            flush_rx: state.flush_tx.subscribe(),
        }
    }

    pub async fn next(&mut self) -> Option<Arc<Vec<Event>>> {
        // We convert receive errors into `Option<T>` here because we shouldn't ever actually get to a place where the
        // sender somehow goes away: it's tied up in an `Arc<T>` held in a static, so once it's set, it should live
        // forever.
        //
        // However, we're just being safe here because who knows. ¯\_(ツ)_/¯
        self.flush_rx.recv().await.ok()
    }
}

async fn flush_metrics(flush_interval: Duration) {
    // TODO: This is only worth 8KB, but it would be good to find a proper spot to tie this into the memory
    // bounds/accounting stuff since we currently just initialize metrics (and logging) with all-inclusive free
    // functions that come way before we even construct a topology.
    let context_resolver = ContextResolver::from_interner(
        "internal_metrics",
        FixedSizeInterner::new(INTERNAL_METRICS_INTERNER_SIZE),
    );

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

        let ts = get_unix_timestamp();

        for (key, counter) in counters {
            let delta = counter.swap(0, Ordering::Relaxed);
            metrics.push(Event::Metric(Metric {
                context: context_from_key(&context_resolver, key),
                value: MetricValue::Counter { value: delta as f64 },
                metadata: MetricMetadata::from_timestamp(ts),
            }));
        }

        for (key, gauge) in gauges {
            let value = gauge.load(Ordering::Relaxed);
            metrics.push(Event::Metric(Metric {
                context: context_from_key(&context_resolver, key),
                value: MetricValue::Gauge {
                    value: f64::from_bits(value),
                },
                metadata: MetricMetadata::from_timestamp(ts),
            }));
        }

        for (key, histogram) in histograms {
            // TODO: We should submit a PR to `metrics-util` to allow returning a value from the closure passed to
            // `AtomicBucket::clear_with` to avoid this silly mem::replace call.
            let mut metric_value = MetricValue::Counter { value: 0.0 };
            histogram.clear_with(|samples| {
                drop(std::mem::replace(
                    &mut metric_value,
                    MetricValue::distribution_from_values(samples),
                ))
            });

            metrics.push(Event::Metric(Metric {
                context: context_from_key(&context_resolver, key),
                value: metric_value,
                metadata: MetricMetadata::from_timestamp(ts),
            }));
        }

        let shared = Arc::new(metrics);
        let _ = state.flush_tx.send(shared);
    }
}

fn context_from_key(context_resolver: &ContextResolver, key: Key) -> Context {
    let (name, labels) = key.into_parts();
    let labels = labels
        .into_iter()
        .map(|l| format!("{}:{}", l.key(), l.value()))
        .collect::<Vec<_>>();

    let context_ref = ContextRef::from_name_and_tags(name.as_str(), &labels);
    context_resolver.resolve(context_ref)
}

pub async fn initialize_metrics(metrics_prefix: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let recorder = MetricsRecorder::new(metrics_prefix);
    recorder.install()?;

    tokio::spawn(flush_metrics(FLUSH_INTERVAL));

    Ok(())
}

use std::thread;

use saluki_metrics::{static_metrics, Counter, DerivedHistogram, Histogram};

#[static_metrics(prefix = derived, labels(id))]
#[derive(Clone)]
struct Metrics {
    #[metric(mapped(reason))]
    dropped_total: Counter,
    #[metric(derived(max))]
    payload_size_bytes: Histogram,
    #[metric(level = debug, derived(min, max))]
    latency_seconds: Histogram,
}

#[static_metrics(prefix = unlabeled)]
struct Unlabeled {
    #[metric(derived(min))]
    queue_depth: Histogram,
}

fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    assert_send_sync::<Metrics>();
    assert_send_sync::<DerivedHistogram<'static>>();

    let metrics = Metrics::new("test");
    metrics.dropped_total("full").increment(1);
    metrics.payload_size_bytes().record(1024.0);
    metrics.latency_seconds().record(3u32);
    metrics.latency_seconds().record_many(0.5, 2);

    // A clone shares the tracked values and can move to another thread.
    let clone = metrics.clone();
    thread::spawn(move || clone.latency_seconds().record(0.25))
        .join()
        .unwrap();

    assert_eq!(Metrics::payload_size_bytes_name(), "derived_payload_size_bytes");
    assert_eq!(Metrics::payload_size_bytes_max_name(), "derived_payload_size_bytes_max");
    assert_eq!(Metrics::latency_seconds_min_name(), "derived_latency_seconds_min");
    assert_eq!(Metrics::latency_seconds_max_name(), "derived_latency_seconds_max");

    let unlabeled = Unlabeled::new();
    unlabeled.queue_depth().record(7.0);
    assert_eq!(Unlabeled::queue_depth_min_name(), "unlabeled_queue_depth_min");
}

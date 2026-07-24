use saluki_metrics::{static_metrics, Counter, Gauge, Histogram};

#[static_metrics(prefix = "levels", labels(id))]
#[derive(Clone)]
struct Metrics {
    // No attribute: defaults to the INFO level.
    info_counter: Counter,
    #[metric(level = info)]
    explicit_info: Counter,
    #[metric(level = debug)]
    debug_gauge: Gauge,
    #[metric(level = trace)]
    trace_histogram: Histogram,
}

fn main() {
    let metrics = Metrics::new("x".to_string());
    metrics.info_counter().increment(1);
    metrics.explicit_info().increment(1);
    metrics.debug_gauge().set(1.0);
    metrics.trace_histogram().record(1.0);
}

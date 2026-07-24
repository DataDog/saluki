use saluki_metrics::static_metrics;

#[static_metrics(labels(id))]
#[derive(Clone)]
struct Metrics {
    requests_handled: saluki_metrics::Counter,
}

fn main() {}

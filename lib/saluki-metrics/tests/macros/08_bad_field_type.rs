use saluki_metrics::static_metrics;

#[static_metrics(prefix = "bad")]
#[derive(Clone)]
struct Metrics {
    not_a_metric: String,
}

fn main() {}

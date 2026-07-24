use saluki_metrics::static_metrics;

#[static_metrics(prefix = "nope")]
enum Metrics {
    A,
}

fn main() {}

use saluki_metrics::static_metrics;

#[static_metrics(prefix = "empty")]
#[derive(Clone)]
struct Metrics {}

fn main() {}

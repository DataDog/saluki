use saluki_metrics::static_metrics;

#[static_metrics(prefix = "levels")]
#[derive(Clone)]
struct Metrics {
    #[metric(level = warn)]
    bar: saluki_metrics::Counter,
}

fn main() {}

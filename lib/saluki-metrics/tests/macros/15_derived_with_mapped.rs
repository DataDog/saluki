use saluki_metrics::static_metrics;

#[static_metrics(prefix = bad)]
struct Metrics {
    #[metric(mapped(reason), derived(max))]
    size_bytes: saluki_metrics::Histogram,
}

fn main() {}

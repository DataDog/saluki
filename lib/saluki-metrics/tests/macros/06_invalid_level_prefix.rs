use saluki_metrics::static_metrics;

static_metrics! {
    name => Metrics,
    prefix => basic_usage,
    metrics => [
        warn_counter(bar),
    ]
}

fn main() {}

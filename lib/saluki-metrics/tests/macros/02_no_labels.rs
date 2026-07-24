use saluki_metrics::{static_metrics, Counter};

#[static_metrics(prefix = "no_labels")]
#[derive(Clone)]
struct Metrics {
    basic_counter: Counter,
}

fn main() {
    let metrics = Metrics::new();
    metrics.basic_counter().increment(1);

    assert_eq!(Metrics::basic_counter_name(), "no_labels_basic_counter");
}

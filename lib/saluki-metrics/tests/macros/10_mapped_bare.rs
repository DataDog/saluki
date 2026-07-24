use saluki_metrics::{static_metrics, Counter};

#[static_metrics(prefix = "mapped", labels(component_id))]
#[derive(Clone)]
struct Metrics {
    events_sent_total: Counter,
    // A bare mapped label accepts any `Stringable` value, supplied at emission time.
    #[metric(mapped(reason))]
    events_discarded_total: Counter,
}

fn main() {
    let metrics = Metrics::new("component-1".to_string());

    // Non-mapped accessor is unchanged.
    metrics.events_sent_total().increment(1);

    // Mapped accessor takes the label value by reference and returns an owned handle.
    metrics.events_discarded_total(&"queue_full").increment(1);
    let owned = String::from("invalid");
    metrics.events_discarded_total(&owned).increment(1);
    // Looking up the same value again resolves the cached handle.
    metrics.events_discarded_total(&"queue_full").increment(1);

    assert_eq!(Metrics::events_discarded_total_name(), "mapped_events_discarded_total");
}

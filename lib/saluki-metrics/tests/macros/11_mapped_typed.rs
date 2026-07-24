use saluki_metrics::{static_metrics, Counter};

enum DiscardReason {
    QueueFull,
    Invalid,
}

impl std::fmt::Display for DiscardReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DiscardReason::QueueFull => "queue_full",
            DiscardReason::Invalid => "invalid",
        })
    }
}

#[static_metrics(prefix = "mapped", labels(component_id))]
#[derive(Clone)]
struct Metrics {
    // A typed mapped label pins the accessor parameter to a concrete type for misuse resistance.
    #[metric(level = debug, mapped(reason: DiscardReason))]
    events_discarded_total: Counter,
}

fn main() {
    let metrics = Metrics::new("component-1".to_string());
    metrics.events_discarded_total(&DiscardReason::QueueFull).increment(1);
    metrics.events_discarded_total(&DiscardReason::Invalid).increment(1);
}

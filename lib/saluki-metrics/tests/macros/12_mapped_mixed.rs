use saluki_metrics::{static_metrics, Histogram};

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

#[static_metrics(prefix = "component", labels(component_id))]
#[derive(Clone)]
struct Metrics {
    // Many mapped labels, mixing a bare (generic) label with a typed one.
    #[metric(level = trace, mapped(origin, reason: DiscardReason))]
    send_latency_seconds: Histogram,
}

fn main() {
    let metrics = Metrics::new("component-1".to_string());
    metrics
        .send_latency_seconds(&"upstream", &DiscardReason::QueueFull)
        .record(0.5);
    metrics
        .send_latency_seconds(&"downstream", &DiscardReason::Invalid)
        .record(1.5);
}

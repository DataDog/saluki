use std::sync::Arc;

use saluki_metrics::{static_metrics, Counter};

#[static_metrics(prefix = "multi", labels(owned, borrowed, shared))]
#[derive(Clone)]
struct Metrics {
    events_total: Counter,
}

fn main() {
    // The generic `new()` accepts any `Stringable` label value type, so the three labels can be a `String`, a
    // `&'static str`, and an `Arc<str>` respectively.
    let metrics = Metrics::new("owned".to_string(), "borrowed", Arc::<str>::from("shared"));
    metrics.events_total().increment(1);
}

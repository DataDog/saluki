use saluki_metrics::{static_metrics, Counter, Gauge};

#[static_metrics(prefix = "basic_usage", labels(id))]
#[derive(Clone)]
struct Metrics {
    requests_handled: Counter,
    in_flight: Gauge,
}

fn main() {
    let metrics = Metrics::new("test".to_string());
    metrics.requests_handled().increment(1);
    metrics.in_flight().set(3.0);

    // The `<field>_name()` helper returns the fully qualified metric name.
    assert_eq!(Metrics::requests_handled_name(), "basic_usage_requests_handled");
    assert_eq!(Metrics::in_flight_name(), "basic_usage_in_flight");

    // `Debug` prints only the struct name, and `Clone` is available.
    assert_eq!(format!("{:?}", metrics), "Metrics");
    let _cloned = metrics.clone();
}

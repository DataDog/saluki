//! Metric fixtures shared by the tests of the code that reads a dump.

use serde_json::{json, Value};
use stele::Metric;

pub(crate) fn count(value: f64) -> Value {
    json!({"mtype": "Count", "value": value})
}

pub(crate) fn rate(value: f64) -> Value {
    json!({"mtype": "Rate", "interval": 10, "value": value})
}

pub(crate) fn gauge(value: f64) -> Value {
    json!({"mtype": "Gauge", "value": value})
}

/// Builds a dump entry as JSON, its values stamped in order starting at zero.
pub(crate) fn metric_json(name: &str, tags: &[&str], values: &[Value]) -> Value {
    json!({
        "context": {
            "name": name,
            "tags": tags,
        },
        "values": values
            .iter()
            .enumerate()
            .map(|(idx, value)| json!([idx as u64, value]))
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn metric(name: &str, tags: &[&str], values: &[Value]) -> Metric {
    serde_json::from_value(metric_json(name, tags, values)).expect("metric should deserialize")
}

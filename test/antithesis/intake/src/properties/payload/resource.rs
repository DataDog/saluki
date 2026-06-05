//! Resource-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::{MetricSeries, Resource};
use serde_json::json;

/// Pyld18 -- maximum resources per series.
const MAX_RESOURCES_PER_SERIES: usize = 500;
/// Pyld19 -- maximum host name length in bytes.
const MAX_HOST_NAME_BYTES: usize = 255;

/// The first resource with type "host", or None.
fn host_resource(series: &MetricSeries) -> Option<&Resource> {
    series.resources.iter().find(|r| r.type_() == "host")
}

/// Pyld17 -- the resolved host resource is named the Agent hostname.
pub(crate) fn host_resolved(ms: &MetricSeries, expected: &str) {
    let observed = match host_resource(ms) {
        None => Some(json!({ "resolution": "missing", "resolved": serde_json::Value::Null })),
        Some(host) if host.name() != expected => Some(json!({ "resolution": "mismatch", "resolved": host.name() })),
        Some(_) => None,
    };
    assert_always!(
        observed.is_none(),
        "Pyld17.host_resource_resolved",
        &json!({ "metric": ms.metric(), "expected": expected, "observed": observed })
    );
}

/// Pyld18 -- resource count at most MaxResources(orgID).
pub(crate) fn resource_count(ms: &MetricSeries) {
    let over = (ms.resources.len() > MAX_RESOURCES_PER_SERIES).then_some(ms.resources.len());
    assert_always!(
        over.is_none(),
        "Pyld18.resource_count",
        &json!({ "metric": ms.metric(), "max_resources": MAX_RESOURCES_PER_SERIES, "observed": over })
    );
}

/// Pyld19 -- host name at most 255 bytes.
pub(crate) fn host_name_length(ms: &MetricSeries) {
    let over = host_resource(ms)
        .map(Resource::name)
        .filter(|name| name.len() > MAX_HOST_NAME_BYTES);
    assert_always!(
        over.is_none(),
        "Pyld19.host_name_length",
        &json!({ "metric": ms.metric(), "observed": over })
    );
}

//! Resource-level checks

<<<<<<< HEAD
use std::sync::OnceLock;

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::{MetricSeries, Resource};
use datadog_protos::metrics::MetricPayload;
=======
use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::{MetricSeries, Resource};
>>>>>>> 9c1abdeb85 (enhancement(antithesis): Introduce rig intake API (#1826))
use serde_json::json;

/// Pyld18 -- maximum resources per series.
const MAX_RESOURCES_PER_SERIES: usize = 500;
/// Pyld19 -- maximum host name length in bytes.
const MAX_HOST_NAME_BYTES: usize = 255;

/// The first resource with type "host", or None.
fn host_resource(series: &MetricSeries) -> Option<&Resource> {
    series.resources.iter().find(|r| r.type_() == "host")
}

<<<<<<< HEAD
/// Pyld17 -- every metric context across all inbound traffic on a lane resolves a non-empty host,
/// and they all share one host. `established` is set once from the first host seen so the check spans
/// every request. Cross-lane host equality is checked by the differential oracle.
pub(crate) fn host_consistent(payload: &MetricPayload, established: &OnceLock<String>) {
    let mut observed = None;
    for ms in &payload.series {
        let name = host_resource(ms).map_or("", Resource::name);
        if name.is_empty() {
            observed = Some(json!({ "metric": ms.metric(), "issue": "empty_or_missing" }));
            break;
        }
        let prev = established.get_or_init(|| name.to_owned());
        if prev != name {
            observed =
                Some(json!({ "metric": ms.metric(), "issue": "mismatch", "established": prev, "resolved": name }));
            break;
        }
    }
    assert_always!(
        observed.is_none(),
        "Pyld17.host_resource_resolved",
        &json!({ "series": payload.series.len(), "host": established.get(), "observed": observed })
=======
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
>>>>>>> 9c1abdeb85 (enhancement(antithesis): Introduce rig intake API (#1826))
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

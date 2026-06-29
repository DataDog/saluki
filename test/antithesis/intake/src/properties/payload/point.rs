//! `MetricPoint`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::MetricSeries;
use serde_json::json;

/// Pyld21 -- max seconds a point timestamp may exceed intake wall clock.
const MAX_SECONDS_IN_FUTURE: i64 = 600;

/// Pyld20 -- no point value is NaN.
pub(crate) fn value_not_nan(ms: &MetricSeries) {
    let mut count = 0usize;
    let mut first = None;
    for (i, p) in ms.points.iter().enumerate() {
        if p.value.is_nan() {
            count += 1;
            if first.is_none() {
                first = Some(i);
            }
        }
    }
    let violation = first.map(|idx| (idx, count));
    assert_always!(
        violation.is_none(),
        "Pyld20.value_not_nan",
        &json!({ "metric": ms.metric(), "observed": violation })
    );
}

/// Pyld21 -- no point timestamp exceeds `intake_now` + 600s.
pub(crate) fn future_bound(ms: &MetricSeries, intake_now_secs: i64) {
    let bound = intake_now_secs.saturating_add(MAX_SECONDS_IN_FUTURE);
    let mut count = 0usize;
    let mut first = None;
    for (i, p) in ms.points.iter().enumerate() {
        if p.timestamp > bound {
            count += 1;
            if first.is_none() {
                first = Some(i);
            }
        }
    }
    let violation = first.map(|idx| (idx, count));
    assert_always!(
        violation.is_none(),
        "Pyld21.timestamp_future_bound",
        &json!({ "metric": ms.metric(), "observed": violation })
    );
}

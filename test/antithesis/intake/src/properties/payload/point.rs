//! `MetricPoint`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::MetricSeries;
use serde_json::json;

use crate::capture::{BucketValue, Target};

/// Pyld21 -- max seconds a point timestamp may exceed intake wall clock.
const MAX_SECONDS_IN_FUTURE: i64 = 600;

/// Pyld20 -- no point value is NaN.
pub(crate) fn value_not_nan(target: Target, ms: &MetricSeries) {
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
        &json!({ "lane": target, "metric": ms.metric(), "observed": violation })
    );
}

/// Pyld21 -- no point timestamp exceeds `intake_now` + 600s.
pub(crate) fn future_bound(target: Target, ms: &MetricSeries, intake_now_secs: i64) {
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
        &json!({ "lane": target, "metric": ms.metric(), "observed": violation })
    );
}

/// Pyld21 for v3 -- no v3 point bucket-start exceeds `intake_now` + 600s. Same property as
/// `future_bound`, applied to the natively decoded v3 points, which carry an absolute `u64` bucket-start.
pub(crate) fn future_bound_v3(target: Target, metric: &str, points: &[(u64, BucketValue)], now_secs: i64) {
    let bound = now_secs.saturating_add(MAX_SECONDS_IN_FUTURE);
    let first_over = points
        .iter()
        .position(|&(bucket_start, _)| i128::from(bucket_start) > i128::from(bound));
    assert_always!(
        first_over.is_none(),
        "Pyld21.timestamp_future_bound",
        &json!({ "lane": target, "metric": metric, "first_over": first_over })
    );
}

//! `MetricPayload`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::MetricPayload;
use serde_json::json;

use super::constants::MAX_POINTS_PER_PAYLOAD;
use crate::capture::Target;

/// Pyld07 -- the intake's decode decision matches production. Production accepts a payload,
/// or rejects it for a non-UTF-8 non-tag string field. Either is production-faithful. Only
/// genuinely malformed protobuf wire, which no real producer emits, fails this.
pub(crate) fn decode_production_faithful(
    target: Target, production_faithful: bool, outcome: &str, body_len: usize, decompression_applied: bool,
) {
    assert_always!(
        production_faithful,
        "Pyld07.decode_success",
        &json!({ "lane": target, "outcome": outcome, "body_len": body_len, "decompression_applied": decompression_applied })
    );
}

/// Pyld08 -- total points across the payload at or below the cap.
pub(crate) fn point_count(target: Target, payload: &MetricPayload) {
    let total: usize = payload.series.iter().map(|s| s.points.len()).sum();
    let over = (total > MAX_POINTS_PER_PAYLOAD).then_some(total);
    assert_always!(
        over.is_none(),
        "Pyld08.payload_point_count",
        &json!({ "lane": target, "max_points": MAX_POINTS_PER_PAYLOAD, "observed": over })
    );
}

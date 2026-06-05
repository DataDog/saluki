//! `MetricPayload`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::MetricPayload;
use serde_json::json;

use super::constants::MAX_POINTS_PER_PAYLOAD;

/// Pyld07 -- the body decodes as a v2 `MetricPayload`.
pub(crate) fn decode_success(decoded_ok: bool, body_len: usize, decompression_applied: bool) {
    assert_always!(
        decoded_ok,
        "Pyld07.decode_success",
        &json!({ "body_len": body_len, "decompression_applied": decompression_applied })
    );
}

/// Pyld08 -- total points across the payload at or below the cap.
pub(crate) fn point_count(payload: &MetricPayload) {
    let total: usize = payload.series.iter().map(|s| s.points.len()).sum();
    let over = (total > MAX_POINTS_PER_PAYLOAD).then_some(total);
    assert_always!(
        over.is_none(),
        "Pyld08.payload_point_count",
        &json!({ "max_points": MAX_POINTS_PER_PAYLOAD, "observed": over })
    );
}

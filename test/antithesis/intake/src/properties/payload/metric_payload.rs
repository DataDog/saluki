//! `MetricPayload`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::MetricPayload;
use serde_json::json;

use super::constants::MAX_POINTS_PER_PAYLOAD;
use crate::capture::Target;

/// Pyld07-v2 -- the body cleanly decodes. A correct Agent produces a payload the intake accepts, so any
/// reject fails: malformed wire, a non-UTF-8 non-tag field, or a non-UTF-8 tag from a non-`datadog-agent`
/// source. A feral tag from the `datadog-agent` source is coerced and decodes to `Ok`, so it never
/// reaches this as a rejection.
pub(crate) fn decode_production_faithful(
    target: Target, production_faithful: bool, outcome: &str, body_len: usize, decompression_applied: bool,
) {
    assert_always!(
        production_faithful,
        "Pyld07.decode_success",
        &json!({ "lane": target, "outcome": outcome, "body_len": body_len, "decompression_applied": decompression_applied })
    );
}

/// Pyld07-v3 -- the v3 body cleanly decodes. Same doctrine as Pyld07-v2: any reject fails, since a
/// correct Agent produces a payload the intake accepts. v3 tags are universally coerced, so a feral tag
/// decodes to `Ok` and only a non-UTF-8 non-tag field or malformed wire reaches this as a rejection.
pub(crate) fn decode_v3(target: Target, cleanly_decoded: bool, outcome: &str, body_len: usize) {
    assert_always!(
        cleanly_decoded,
        "Pyld07-v3.decode_success",
        &json!({ "lane": target, "outcome": outcome, "body_len": body_len })
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

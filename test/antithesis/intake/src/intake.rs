//! `/api/v2/series` intake handler.
//!
//! Decodes the Agent's `MetricPayload` and fires an Antithesis SDK
//! `assert_always!` for every W property the handler can evaluate from the
//! request envelope and decoded payload. Assertion firing is independent of the
//! response code: the handler always returns `202 Accepted` on a decodable
//! body, mirroring real intake, which returns `202` even when individual series
//! carry validation errors.
//!
//! Ported from the `invariant-jig` checker. The property numbering and
//! semantics live in that project's `README.md` §Properties.Payloads.

use std::time::{SystemTime, UNIX_EPOCH};

use antithesis_sdk::prelude::*;
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
};
use datadog_protos::metrics::{metric_payload::MetricSeries, MetricPayload};
use protobuf::Message as _;
use serde_json::json;
use tracing::{debug, error};

use crate::{
    http::{CompressedLen, DeclaredContentLength, DecompressionApplied},
    predicates::{metric_point, payload, series},
    state::AppState,
};

/// `serializer_max_series_points_per_payload` default (W8/W15). The Agent caps
/// both the per-payload total and the per-series count at this value.
const MAX_POINTS_PER_PAYLOAD: usize = 10_000;

/// `MaxTags(orgID)` default (W13).
const MAX_TAGS_PER_SERIES: usize = 100;

/// `MaxResources(orgID)` default (W18).
const MAX_RESOURCES_PER_SERIES: usize = 500;

/// Future-bound window used by W21, in seconds.
const MAX_SECONDS_IN_FUTURE: i64 = 600;

/// W5 compressed-body cap. The Agent caps at `<= 512000` bytes and intake
/// rejects at `>= 512000`, so the effective cross-cut bound is strict-below.
const W5_COMPRESSED_CAP_BYTES: u64 = 512_000;

/// W6 uncompressed-body cap. Intake admits up to 5 MiB inclusive.
const W6_UNCOMPRESSED_CAP_BYTES: u64 = 5 * 1024 * 1024;

/// Handler for `POST /api/v2/series`. Fires one SDK assertion per W property
/// per request (envelope-level) or per series/point (series/point-level).
/// Antithesis aggregates per assertion name.
pub async fn handle_series(State(state): State<AppState>, request: Request) -> StatusCode {
    // W21 bounds a point's timestamp against the intake wall clock at request
    // receipt. Capture it before draining and decoding the body: those take
    // noticeable time and would otherwise move the bound later than the request
    // actually arrived.
    let now_secs = unix_now_secs();
    let (parts, body) = request.into_parts();
    let compressed_len = parts.extensions.get::<CompressedLen>().map_or(0, |c| c.0);
    let decompression_applied = parts.extensions.get::<DecompressionApplied>().is_some_and(|d| d.0);
    let declared_content_length = parts.extensions.get::<DeclaredContentLength>().and_then(|d| d.0);

    let body_bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = %e, "Failed to drain /api/v2/series body.");
            return StatusCode::BAD_REQUEST;
        }
    };
    let headers = parts.headers;

    // The Agent sends a `{}` body to probe endpoint connectivity. That is not a
    // metric payload, so skip the whole W pipeline: W7 would otherwise read the
    // probe as a malformed `MetricPayload` and fire a spurious assertion on
    // every probe. Real intake likewise short-circuits the diagnostic body.
    if body_bytes.as_ref() == b"{}" {
        debug!("Received diagnostic probe for /api/v2/series, ignoring.");
        return StatusCode::ACCEPTED;
    }

    evaluate_envelope(&headers);
    evaluate_byte_sizes(
        declared_content_length,
        compressed_len,
        decompression_applied,
        body_bytes.len() as u64,
    );

    let decode_result = MetricPayload::parse_from_bytes(&body_bytes);
    assert_always!(
        decode_result.is_ok(),
        "W7.decode_success",
        &json!({
            "body_len": body_bytes.len(),
            "decompression_applied": decompression_applied,
        })
    );
    let metric_payload = match decode_result {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse /api/v2/series MetricPayload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    if !metric_payload.series.is_empty() {
        // Lets triage distinguish an Agent that came up and flushed from one
        // that never did. A run where this never fires stalled before any
        // /api/v2/series reached the intake.
        assert_reachable!(
            "intake.first_series_observed",
            &json!({ "series": metric_payload.series.len() })
        );
    }

    evaluate_payload(&metric_payload);
    for ms in &metric_payload.series {
        evaluate_series(ms, now_secs, &state.expected_hostname);
    }

    debug!(
        bytes = body_bytes.len(),
        series = metric_payload.series.len(),
        "received /api/v2/series"
    );

    // Feed the dump store so /metrics/dump reflects delivery. Best-effort: a
    // merge failure (e.g. an UNSPECIFIED type, which W12 already flagged) must
    // not change the response the Agent observes.
    if let Err(e) = state.metrics.merge_series_v2_payload(metric_payload) {
        debug!(error = %e, "Could not merge series v2 payload into dump store.");
    }

    StatusCode::ACCEPTED
}

fn evaluate_byte_sizes(
    declared_content_length: Option<u64>, compressed_len: u64, decompression_applied: bool, uncompressed_len: u64,
) {
    assert_always!(
        payload::w5_compressed_size(compressed_len, W5_COMPRESSED_CAP_BYTES),
        "W5.compressed_size",
        &json!({ "compressed_bytes": compressed_len, "cap_bytes": W5_COMPRESSED_CAP_BYTES })
    );
    if decompression_applied {
        assert_always!(
            payload::w6_uncompressed_size(uncompressed_len, W6_UNCOMPRESSED_CAP_BYTES),
            "W6.uncompressed_size",
            &json!({ "uncompressed_bytes": uncompressed_len, "cap_bytes": W6_UNCOMPRESSED_CAP_BYTES })
        );
    }
    assert_always!(
        payload::w22_content_length(declared_content_length, compressed_len),
        "W22.content_length",
        &json!({ "compressed_bytes": compressed_len, "declared_content_length": declared_content_length })
    );
}

fn evaluate_envelope(headers: &HeaderMap) {
    assert_always!(
        payload::w1_content_type(headers),
        "W1.content_type",
        &json!({ "header": "Content-Type" })
    );
    assert_always!(
        payload::w2_content_encoding(headers),
        "W2.content_encoding",
        &json!({ "header": "Content-Encoding" })
    );
    assert_always!(
        payload::w3_api_key(headers),
        "W3.api_key_present",
        &json!({ "header": "DD-Api-Key" })
    );
    // W4 (User-Agent prefix) is intentionally omitted. ADP sends
    // `agent-data-plane/...`, not the Go Agent's `datadog-agent/`, and intake
    // does not validate User-Agent. See predicates::payload for the rationale.
}

fn evaluate_payload(metric_payload: &MetricPayload) {
    let w8_violation = payload::w8_point_count(metric_payload, MAX_POINTS_PER_PAYLOAD);
    assert_always!(
        w8_violation.is_none(),
        "W8.payload_point_count",
        &json!({ "max_points": MAX_POINTS_PER_PAYLOAD, "observed": w8_violation })
    );
}

fn evaluate_series(ms: &MetricSeries, now_secs: i64, expected_hostname: &str) {
    assert_always!(
        series::w9_metric_non_empty(ms),
        "W9.metric_non_empty",
        &json!({ "metric": ms.metric() })
    );
    let w10 = series::w10_metric_name_too_long(ms);
    assert_always!(
        w10.is_none(),
        "W10.metric_name_length",
        &json!({ "metric": ms.metric(), "observed_len": w10 })
    );
    assert_always!(
        !series::w11_metric_name_no_alpha(ms),
        "W11.metric_name_alphabetic",
        &json!({ "metric": ms.metric() })
    );
    let w12 = series::w12_type_out_of_domain(ms);
    assert_always!(
        w12.is_none(),
        "W12.type_in_domain",
        &json!({ "metric": ms.metric(), "out_of_domain_type": w12 })
    );
    let w13 = series::w13_tag_count(ms, MAX_TAGS_PER_SERIES);
    assert_always!(
        w13.is_none(),
        "W13.tag_count",
        &json!({ "metric": ms.metric(), "max_tags": MAX_TAGS_PER_SERIES, "observed": w13 })
    );
    let w14 = series::w14_reserved_tag_prefix(ms);
    assert_always!(
        w14.is_none(),
        "W14.reserved_tag_prefix",
        &json!({ "metric": ms.metric(), "offending": w14 })
    );
    let w15 = series::w15_point_count(ms, MAX_POINTS_PER_PAYLOAD);
    assert_always!(
        w15.is_none(),
        "W15.series_point_count",
        &json!({ "metric": ms.metric(), "observed": w15 })
    );
    let w17 = series::w17_host_unresolved(ms, expected_hostname);
    let w17_observed = w17.map(|r| {
        let (resolution, resolved) = r.as_detail();
        json!({ "resolution": resolution, "resolved": resolved })
    });
    assert_always!(
        w17.is_none(),
        "W17.host_resource_resolved",
        &json!({ "metric": ms.metric(), "expected": expected_hostname, "observed": w17_observed })
    );
    let w18 = series::w18_resource_count(ms, MAX_RESOURCES_PER_SERIES);
    assert_always!(
        w18.is_none(),
        "W18.resource_count",
        &json!({ "metric": ms.metric(), "max_resources": MAX_RESOURCES_PER_SERIES, "observed": w18 })
    );
    let w19 = series::w19_host_name_too_long(ms);
    assert_always!(
        w19.is_none(),
        "W19.host_name_length",
        &json!({ "metric": ms.metric(), "observed": w19 })
    );
    let w16 = series::w16_origin_out_of_domain(ms);
    assert_always!(
        w16.is_none(),
        "W16.origin_in_domain",
        &json!({ "metric": ms.metric(), "out_of_domain": w16.map(|(field, value)| (field.as_str(), value)) })
    );
    let w20 = metric_point::w20_nan_violations(ms);
    assert_always!(
        w20.is_none(),
        "W20.value_not_nan",
        &json!({ "metric": ms.metric(), "observed": w20.map(|(idx, count)| (idx, count.get())) })
    );
    let w21 = metric_point::w21_future_violations(ms, now_secs, MAX_SECONDS_IN_FUTURE);
    assert_always!(
        w21.is_none(),
        "W21.timestamp_future_bound",
        &json!({ "metric": ms.metric(), "observed": w21.map(|(idx, count)| (idx, count.get())) })
    );
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

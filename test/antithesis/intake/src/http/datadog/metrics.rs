//! Metric and sketch intake handlers.
//!
//! `handle_series` fires every payload property's assertion. It walks the
//! envelope, byte-size, and decode checks in order. It returns the first failure
//! status, or `202 Accepted` when every check holds. `handle_sketches` parses and
//! records sketch payloads.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::HeaderMap,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{debug, error, info};

use crate::capture::EpochSeconds;
use crate::capture::Target;
use crate::http::middleware::Measurements;
use crate::http::state::AppState;
use crate::http::{body_over_cap, MAX_COMPRESSED_BODY_BYTES, MAX_DECOMPRESSED_BODY_BYTES};
use crate::lenient_decode::{decode_series_v3, decode_sketch_payload, Rejection, Source};
use crate::properties::payload::{bytes, envelope, metric_payload, sketch};
use crate::series_observation::SeriesObservation;

/// Reasons `handle_series` cannot evaluate a request.
#[derive(Debug)]
pub(crate) enum SeriesError {
    /// The measurement middleware did not record `Measurements` on the route.
    MissingMeasurements,
    /// The system clock predates the Unix epoch or overflows i64 seconds.
    Clock,
}

impl IntoResponse for SeriesError {
    fn into_response(self) -> Response {
        match self {
            Self::MissingMeasurements => {
                error!("Missing Measurements extension on /api/v2/series, measurement middleware is misconfigured.");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Clock => {
                error!("System clock is not readable as seconds since the Unix epoch.");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
        .into_response()
    }
}

/// Fire every property the request parts alone decide, for a request the intake cannot finish reading.
/// Each of these comes from a header or a pre-decompression measurement, so a body the intake never
/// buffered still gets its envelope checked instead of the whole request going silent.
fn assert_parts_only(
    target: Target, headers: &HeaderMap, compressed_len: Option<u64>, declared: Option<u64>, encoding: Option<&[u8]>,
) {
    envelope::api_key(target, headers);
    envelope::content_type(target, headers);
    envelope::content_encoding(target, encoding);
    // Pyld22 compares the declared length against the wire body, so it is skipped when the body
    // overran the cap and was never read: comparing against a length we never measured would fail the
    // property on a request whose only fault is its size.
    if let Some(compressed_len) = compressed_len {
        bytes::content_length(target, declared, compressed_len);
    }
}

/// Handler for `POST /api/v2/series`.
pub(crate) async fn handle_series(State(state): State<AppState>, request: Request) -> Result<StatusCode, SeriesError> {
    // Pyld21 bounds points' timestamps against the intake wall clock at request receipt
    let now_secs = now_epoch_secs()?;
    let (parts, body) = request.into_parts();
    let measurements = parts
        .extensions
        .get::<Measurements>()
        .ok_or(SeriesError::MissingMeasurements)?;
    let compressed_len = measurements.compressed_len;
    let decompression_applied = measurements.decompression_applied;
    let declared_content_length = measurements.declared_content_length;
    let content_encoding = measurements.content_encoding.clone();
    let over_compressed_cap = measurements.over_compressed_cap;
    let headers = parts.headers.clone();

    if over_compressed_cap {
        assert_parts_only(
            state.target,
            &headers,
            None,
            declared_content_length,
            content_encoding.as_deref(),
        );
        bytes::compressed_size_over_cap(state.target, MAX_COMPRESSED_BODY_BYTES as u64);
        return Ok(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let body_bytes = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body_bytes) => body_bytes,
        Err(e) => {
            assert_parts_only(
                state.target,
                &headers,
                Some(compressed_len),
                declared_content_length,
                content_encoding.as_deref(),
            );
            bytes::compressed_size(state.target, compressed_len);
            if body_over_cap(e) {
                bytes::uncompressed_size_over_cap(state.target, MAX_DECOMPRESSED_BODY_BYTES as u64);
            }
            return Ok(StatusCode::PAYLOAD_TOO_LARGE);
        }
    };

    // Datadog Agent sends `{}` to probe connectivity, not a metric payload. The real
    // intake accepts the probe with 202. Match it rather than 200.
    if body_bytes.as_ref() == b"{}" {
        debug!("Received connectivity probe for /api/v2/series, returning 202 Accepted.");
        return Ok(StatusCode::ACCEPTED);
    }

    // Classify the submitter source from the User-Agent, gating v2 tag handling exactly as the backend
    // does: `datadog-agent` sanitizes a feral tag, every other source whole-payload-rejects it. A
    // header that is not visible ASCII cannot be the ASCII `datadog-agent` prefix, so it is `Other`.
    let source = Source::from_user_agent(
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    );
    let uncompressed_len = body_bytes.len() as u64;

    if let Some(config) = state.sut_config() {
        envelope::series_api_as_configured(state.target, config, false);
    }

    // Envelope and byte-size properties.
    let api_key_ok = envelope::api_key(state.target, &headers);
    let content_type_ok = envelope::content_type(state.target, &headers);
    envelope::content_encoding(state.target, content_encoding.as_deref());
    let compressed_ok = bytes::compressed_size(state.target, compressed_len);
    let uncompressed_ok = bytes::uncompressed_size(state.target, uncompressed_len);
    bytes::content_length(state.target, declared_content_length, compressed_len);

    let (observation, decode_ok) = SeriesObservation::decode(state.target, &body_bytes, decompression_applied, source);

    if let Some(observation) = observation.as_ref() {
        observation.assert_payload_properties(state.target, now_secs, &state.established_host);
        debug!(
            bytes = body_bytes.len(),
            series = observation.series_len(),
            "received /api/v2/series"
        );
    }

    if let Some(observation) = observation {
        let count = state.recorder.record_series_v2(
            state.target,
            observation.into_payload(),
            EpochSeconds::from_epoch_secs(now_secs),
        );
        if count > 0 {
            info!(target = state.target.as_str(), count, "captured metrics");
        }
    }

    // Return the first failure status in pipeline order, or 202 Accepted.
    let failure = first_status_failure(&[
        (api_key_ok, StatusCode::FORBIDDEN),
        (content_type_ok, StatusCode::BAD_REQUEST),
        (compressed_ok, StatusCode::PAYLOAD_TOO_LARGE),
        (uncompressed_ok, StatusCode::PAYLOAD_TOO_LARGE),
        (decode_ok, StatusCode::BAD_REQUEST),
    ]);
    Ok(failure.unwrap_or(StatusCode::ACCEPTED))
}

/// Handler for `POST /api/beta/sketches`.
///
/// The sketch endpoint sits on the measured route stack, so it asserts the same envelope and byte
/// properties as `/api/v2/series` (Pyld01/03/05/06/22) from the request parts and `Measurements`, then
/// runs the lenient sketch decoder. Decode failures still answer 400; the envelope/byte assertions are
/// observational.
pub(crate) async fn handle_sketches(State(state): State<AppState>, request: Request) -> StatusCode {
    let Ok(now_secs) = now_epoch_secs() else {
        error!("System clock is not readable as seconds since the Unix epoch.");
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let (parts, body) = request.into_parts();
    let Some(measurements) = parts.extensions.get::<Measurements>() else {
        error!("Missing Measurements extension on /api/beta/sketches, measurement middleware is misconfigured.");
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let compressed_len = measurements.compressed_len;
    let declared_content_length = measurements.declared_content_length;
    let content_encoding = measurements.content_encoding.clone();
    let over_compressed_cap = measurements.over_compressed_cap;
    let headers = parts.headers.clone();
    if over_compressed_cap {
        assert_parts_only(
            state.target,
            &headers,
            None,
            declared_content_length,
            content_encoding.as_deref(),
        );
        bytes::compressed_size_over_cap(state.target, MAX_COMPRESSED_BODY_BYTES as u64);
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(
                target = state.target.as_str(),
                cap = MAX_DECOMPRESSED_BODY_BYTES,
                "Rejected sketches body at the decompressed cap."
            );
            assert_parts_only(
                state.target,
                &headers,
                Some(compressed_len),
                declared_content_length,
                content_encoding.as_deref(),
            );
            bytes::compressed_size(state.target, compressed_len);
            if body_over_cap(e) {
                bytes::uncompressed_size_over_cap(state.target, MAX_DECOMPRESSED_BODY_BYTES as u64);
            }
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    // The Datadog Agent sends `{}` to probe connectivity, not a sketch payload. The real intake accepts
    // it with 202, so skip the assertions and decode for the probe.
    if body.as_ref() == b"{}" {
        debug!("Received connectivity probe for /api/beta/sketches, returning 202 Accepted.");
        return StatusCode::ACCEPTED;
    }
    let uncompressed_len = body.len() as u64;
    envelope::api_key(state.target, &headers);
    envelope::content_type(state.target, &headers);
    envelope::content_encoding(state.target, content_encoding.as_deref());
    bytes::compressed_size(state.target, compressed_len);
    bytes::uncompressed_size(state.target, uncompressed_len);
    bytes::content_length(state.target, declared_content_length, compressed_len);
    // Lenient decode: the Datadog Agent forwards feral non-UTF-8 tags on sketches the same way it
    // does on series, and the real intake keeps them. Strict parsing dropped whole agent-lane sketch
    // payloads and starved the differential of distribution contexts (#2039 fixed this for series only).
    let outcome = decode_sketch_payload(&body);
    let (cleanly_decoded, label) = match &outcome {
        Ok(_) => (true, "accepted"),
        Err(Rejection::NonUtf8Tag) => (true, "rejected_non_utf8_tag"),
        Err(Rejection::NonUtf8StrictField) => (false, "rejected_non_utf8_field"),
        Err(Rejection::MalformedWire) => (false, "malformed_wire"),
    };
    sketch::decode_faithful(state.target, cleanly_decoded, label, body.len());
    let payload = match outcome {
        Ok(payload) => payload,
        Err(rejection) => {
            error!(target = state.target.as_str(), ?rejection, "rejected sketch payload");
            return StatusCode::BAD_REQUEST;
        }
    };
    for sk in &payload.sketches {
        sketch::shape(state.target, sk);
    }
    let count = state
        .recorder
        .record_sketches(state.target, payload, EpochSeconds::from_epoch_secs(now_secs));
    info!(target = state.target.as_str(), count, "captured sketch metrics");
    StatusCode::ACCEPTED
}

/// Handler for `POST /api/intake/metrics/v3/series`.
///
/// The v3 native series API is dictionary + delta encoded columnar protobuf. `measure_compressed_size`
/// records the wire body before `RequestDecompressionLayer` decompresses it, so this handler asserts the
/// envelope and byte properties (Pyld01/03/05/06/22) from the request parts and `Measurements`, then runs
/// `decode_series_v3`, an independent reimplementation of the production v3 decoder that applies the
/// two-tier failure model and fires the v3 structural assertions internally. Decode failures answer 400;
/// the envelope/byte assertions here are observational. v3 series carry no `{}` connectivity probe.
pub(crate) async fn handle_series_v3(State(state): State<AppState>, request: Request) -> StatusCode {
    let Ok(now_secs) = now_epoch_secs() else {
        error!("System clock is not readable as seconds since the Unix epoch.");
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let (parts, body) = request.into_parts();
    let Some(measurements) = parts.extensions.get::<Measurements>() else {
        error!(
            "Missing Measurements extension on /api/intake/metrics/v3/series, measurement middleware is misconfigured."
        );
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let compressed_len = measurements.compressed_len;
    let declared_content_length = measurements.declared_content_length;
    let content_encoding = measurements.content_encoding.clone();
    let over_compressed_cap = measurements.over_compressed_cap;
    let headers = parts.headers.clone();
    if over_compressed_cap {
        assert_parts_only(
            state.target,
            &headers,
            None,
            declared_content_length,
            content_encoding.as_deref(),
        );
        bytes::compressed_size_over_cap(state.target, MAX_COMPRESSED_BODY_BYTES as u64);
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(
                target = state.target.as_str(),
                cap = MAX_DECOMPRESSED_BODY_BYTES,
                "Rejected v3 series body at the decompressed cap."
            );
            assert_parts_only(
                state.target,
                &headers,
                Some(compressed_len),
                declared_content_length,
                content_encoding.as_deref(),
            );
            bytes::compressed_size(state.target, compressed_len);
            if body_over_cap(e) {
                bytes::uncompressed_size_over_cap(state.target, MAX_DECOMPRESSED_BODY_BYTES as u64);
            }
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    let uncompressed_len = body.len() as u64;
    if let Some(config) = state.sut_config() {
        envelope::series_api_as_configured(state.target, config, true);
    }
    envelope::api_key(state.target, &headers);
    envelope::content_type(state.target, &headers);
    envelope::content_encoding(state.target, content_encoding.as_deref());
    bytes::compressed_size(state.target, compressed_len);
    bytes::uncompressed_size(state.target, uncompressed_len);
    bytes::content_length(state.target, declared_content_length, compressed_len);
    let outcome = decode_series_v3(state.target, &state.established_host, now_secs, &body);
    let (cleanly_decoded, label) = match &outcome {
        Ok(_) => (true, "accepted"),
        Err(Rejection::NonUtf8StrictField) => (false, "rejected_non_utf8_field"),
        Err(Rejection::NonUtf8Tag) => (false, "rejected_non_utf8_tag"),
        Err(Rejection::MalformedWire) => (false, "malformed_wire"),
    };
    metric_payload::decode_v3(state.target, cleanly_decoded, label, body.len());
    let series = match outcome {
        Ok(series) => series,
        Err(rejection) => {
            error!(target = state.target.as_str(), ?rejection, "rejected v3 series payload");
            return StatusCode::BAD_REQUEST;
        }
    };
    let count = state
        .recorder
        .record_series_v3(state.target, series, EpochSeconds::from_epoch_secs(now_secs));
    info!(target = state.target.as_str(), count, "captured v3 series metrics");
    StatusCode::ACCEPTED
}

/// Return the first failed status check, in the given pipeline order, or `None`
/// when every check holds.
fn first_status_failure(checks: &[(bool, StatusCode)]) -> Option<StatusCode> {
    checks.iter().find(|(ok, _)| !ok).map(|&(_, status)| status)
}

/// Return the current time as whole seconds since the Unix epoch.
///
/// Returns `SeriesError::Clock` when the system clock predates the epoch or the second count
/// overflows `i64`.
fn now_epoch_secs() -> Result<i64, SeriesError> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SeriesError::Clock)?
        .as_secs();
    i64::try_from(secs).map_err(|_| SeriesError::Clock)
}

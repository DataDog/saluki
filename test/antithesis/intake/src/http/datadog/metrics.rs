//! `/api/v2/series` handler and validation pipeline.
//!
//! `handle_series` fires every payload property's assertion. It walks the
//! envelope, byte-size, and decode checks in order. It returns the first failure
//! status, or `202 Accepted` when every check holds.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{debug, error};

use crate::http::middleware::Measurements;
use crate::http::state::AppState;
use crate::properties::payload::{bytes, envelope};
use crate::series_observation::SeriesObservation;

/// Memory backstop on the decompressed body buffered in the handler, above the Pyld06 5 MiB spec limit
const MAX_DECOMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Reasons `handle_series` cannot evaluate a request.
#[derive(Debug)]
pub(crate) enum SeriesError {
    /// The measurement middleware did not record `Measurements` on the route.
    MissingMeasurements,
    /// The system clock predates the Unix epoch or overflows i64 seconds.
    Clock,
    /// Reading the request body failed, or the body overran the decompressed cap.
    Body(axum::Error),
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
            Self::Body(e) => {
                // `to_bytes` errors on the size cap and on a read failure. Treat both
                // as oversized, matching the wire-side measurement middleware.
                error!(error = %e, cap = MAX_DECOMPRESSED_BODY_BYTES, "Rejected /api/v2/series body at the decompressed cap.");
                StatusCode::PAYLOAD_TOO_LARGE
            }
        }
        .into_response()
    }
}

/// Handler for `POST /api/v2/series`.
pub(crate) async fn handle_series(State(state): State<AppState>, request: Request) -> Result<StatusCode, SeriesError> {
    // Pyld21 bounds points' timestamps against the intake wall clock at request receipt
    let now_secs = now_epoch_secs()?;
    let (parts, body) = request.into_parts();
    let &Measurements {
        compressed_len,
        decompression_applied,
        declared_content_length,
    } = parts
        .extensions
        .get::<Measurements>()
        .ok_or(SeriesError::MissingMeasurements)?;

    let body_bytes = to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES)
        .await
        .map_err(SeriesError::Body)?;

    // Datadog Agent sends `{}` to probe connectivity, not a metric payload. The real
    // intake accepts the probe with 202. Match it rather than 200.
    if body_bytes.as_ref() == b"{}" {
        debug!("Received connectivity probe for /api/v2/series, returning 202 Accepted.");
        return Ok(StatusCode::ACCEPTED);
    }

    let headers = parts.headers;
    let uncompressed_len = body_bytes.len() as u64;

    // Envelope and byte-size properties.
    let api_key_ok = envelope::api_key(&headers);
    let content_type_ok = envelope::content_type(&headers);
    envelope::content_encoding(&headers);
    let compressed_ok = bytes::compressed_size(compressed_len);
    let uncompressed_ok = bytes::uncompressed_size(uncompressed_len, decompression_applied);
    bytes::content_length(declared_content_length, compressed_len);

    let (observation, decode_ok) = SeriesObservation::decode(&body_bytes, decompression_applied);

    if let Some(observation) = observation.as_ref() {
        observation.assert_payload_properties(now_secs, &state.hostname);
        debug!(
            bytes = body_bytes.len(),
            series = observation.series_len(),
            "received /api/v2/series"
        );
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

//! Recording intakes: loopback HTTP listeners that stand in for the endpoints ADP sends metrics to.
//!
//! Each recipient ADP is configured with (the primary endpoint, the additional endpoints, and the multi-region
//! failover endpoint) gets its own listener, so the UI can show which metrics reach which endpoint. An intake decodes
//! metric payloads with `stele` and `datadog-protos`, as the correctness intake does, after undoing any
//! `Content-Encoding` (zstd, deflate, or gzip). It answers `GET /api/v1/validate` with `200 OK` and every other request
//! with `202 Accepted`, since ADP also sends events, service checks, and metadata, which the demo does not decode.
//!
//! Every request is recorded, including the ones an intake refuses, so that the UI shows when ADP's payloads are being
//! refused: an unsupported content encoding (`415`), a body over [`MAX_BODY_SIZE`] once decompressed (`413`), and a
//! body that cannot be decompressed or decoded (`400`). Decoding runs on a blocking thread, so a decoder that panics on
//! a malformed payload is a refused request rather than a dropped connection.
//!
//! An intake can be paused ([`IntakeRecorder::set_paused`]): it then answers every request with `503 Service
//! Unavailable` without reading it, as an intake that is down for a while does, and counts those answers apart from
//! refusals. ADP's forwarder retries such requests, so a recipient that retires while its intake is paused drains until
//! its deadline.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    Router,
};
use bytes::Bytes;
use datadog_protos::metrics::{v3::Payload as V3Payload, MetricPayload, SketchPayload};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use protobuf::Message as _;
use stele::Metric;
use tower_http::decompression::RequestDecompressionLayer;

use crate::clock::unix_millis;

mod recorder;
use self::recorder::RequestRecord;
pub use self::recorder::{IntakeRecorder, IntakeSummary};

/// The largest decompressed request body an intake accepts.
pub const MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

/// The header ADP's forwarder sends the endpoint's API key in.
const API_KEY_HEADER: &str = "dd-api-key";

/// What an intake does with a request, decided by its method and path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Route {
    /// `GET /api/v1/validate`: the API key check the forwarder runs at startup.
    Validate,

    /// A metric payload in one of the formats ADP sends.
    Metrics(PayloadFormat),

    /// Anything else, accepted and counted.
    Other,
}

/// A metric payload format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadFormat {
    /// `/api/v1/series`: JSON.
    SeriesV1,

    /// `/api/v2/series`: a protobuf `MetricPayload`.
    SeriesV2,

    /// `/api/beta/sketches`: a protobuf `SketchPayload`.
    Sketches,

    /// The v3 intake endpoints: a protobuf v3 `Payload`.
    V3,
}

fn route(method: &Method, path: &str) -> Route {
    match (method, path) {
        (&Method::GET, "/api/v1/validate") => Route::Validate,
        (&Method::POST, "/api/v1/series") => Route::Metrics(PayloadFormat::SeriesV1),
        (&Method::POST, "/api/v2/series") => Route::Metrics(PayloadFormat::SeriesV2),
        (&Method::POST, "/api/beta/sketches") => Route::Metrics(PayloadFormat::Sketches),
        (
            &Method::POST,
            "/api/intake/metrics/v3/series" | "/api/intake/metrics/v3beta/series" | "/api/intake/metrics/v3/sketches",
        ) => Route::Metrics(PayloadFormat::V3),
        _ => Route::Other,
    }
}

/// Decodes a metric payload, after its content encoding has been undone.
///
/// The forwarder probes the series endpoints with `{}` to check connectivity; that decodes to no metrics.
fn decode(format: PayloadFormat, body: &[u8]) -> Result<Vec<Metric>, String> {
    if body == b"{}" && matches!(format, PayloadFormat::SeriesV1 | PayloadFormat::SeriesV2) {
        return Ok(Vec::new());
    }
    match format {
        PayloadFormat::SeriesV1 => Metric::try_from_series_v1(body).map_err(|e| e.to_string()),
        PayloadFormat::SeriesV2 => {
            let payload = MetricPayload::parse_from_bytes(body).map_err(|e| format!("Invalid MetricPayload: {}", e))?;
            Metric::try_from_series_v2(payload).map_err(|e| e.to_string())
        }
        PayloadFormat::Sketches => {
            let payload = SketchPayload::parse_from_bytes(body).map_err(|e| format!("Invalid SketchPayload: {}", e))?;
            Metric::try_from_sketch(payload).map_err(|e| e.to_string())
        }
        PayloadFormat::V3 => {
            let payload = V3Payload::parse_from_bytes(body).map_err(|e| format!("Invalid v3 Payload: {}", e))?;
            Metric::try_from_v3(payload).map_err(|e| e.to_string())
        }
    }
}

/// Returns the API key a request carries: in the `DD-API-KEY` header, as payloads send it, or in the `api_key` query
/// parameter, as the forwarder's validation request sends it.
fn api_key(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(key) = headers.get(API_KEY_HEADER).and_then(|value| value.to_str().ok()) {
        return Some(key.to_string());
    }
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("api_key="))
        .map(str::to_string)
}

/// Builds the router of one recording intake.
pub fn router(recorder: IntakeRecorder) -> Router {
    router_with_limit(recorder, MAX_BODY_SIZE)
}

/// Builds the router of one recording intake that accepts decompressed bodies of up to `max_body_size` bytes.
fn router_with_limit(recorder: IntakeRecorder, max_body_size: usize) -> Router {
    Router::new()
        .fallback(move |state: State<IntakeRecorder>, request: Request| handle(state, request, max_body_size))
        .with_state(recorder)
        // Unsupported encodings reach the handler with their `Content-Encoding` intact, so that it records them.
        .layer(
            RequestDecompressionLayer::new()
                .deflate(true)
                .gzip(true)
                .zstd(true)
                .pass_through_unaccepted(true),
        )
}

/// Why an intake refused a request, with the status it answered.
struct Refusal(StatusCode, String);

/// Reads a request's body, after the decompression layer has undone the encodings it supports.
async fn read_body(headers: &HeaderMap, body: Body, max_body_size: usize) -> Result<Bytes, Refusal> {
    // The decompression layer removes the header of every encoding it undid; one left over is not supported.
    if let Some(encoding) = headers.get(header::CONTENT_ENCODING) {
        if encoding.as_bytes() != b"identity" {
            return Err(Refusal(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!(
                    "Unsupported content encoding {:?}: the intake decodes zstd, deflate, and gzip.",
                    String::from_utf8_lossy(encoding.as_bytes())
                ),
            ));
        }
    }
    match Limited::new(body, max_body_size).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => Err(Refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("The body is larger than {} bytes once decompressed.", max_body_size),
        )),
        Err(error) => Err(Refusal(
            StatusCode::BAD_REQUEST,
            format!("The body cannot be read or decompressed: {}", error),
        )),
    }
}

/// Decodes a metric payload on a blocking thread, turning a panic of the decoder into a refusal.
async fn decode_isolated(format: PayloadFormat, body: Bytes) -> Result<Vec<Metric>, Refusal> {
    match tokio::task::spawn_blocking(move || decode(format, &body)).await {
        Ok(Ok(metrics)) => Ok(metrics),
        Ok(Err(error)) => Err(Refusal(StatusCode::BAD_REQUEST, error)),
        Err(error) => Err(Refusal(
            StatusCode::BAD_REQUEST,
            format!("Decoding the payload failed unexpectedly: {}", error),
        )),
    }
}

async fn handle(State(recorder): State<IntakeRecorder>, request: Request, max_body_size: usize) -> StatusCode {
    let received_ms = unix_millis();
    let (parts, body) = request.into_parts();
    let api_key = api_key(&parts.headers, &parts.uri);
    let path = parts.uri.path().to_string();

    if recorder.is_paused() {
        // The body is read and discarded, so that the connection stays usable and the sender reads the answer rather
        // than a reset.
        let _ = Limited::new(body, max_body_size).collect().await;
        recorder.record_unavailable(RequestRecord {
            received_ms,
            path,
            api_key,
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            series: 0,
            error: None,
        });
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    let outcome = match read_body(&parts.headers, body, max_body_size).await {
        Err(refusal) => Err(refusal),
        Ok(body) => match route(&parts.method, &path) {
            Route::Validate => Ok((StatusCode::OK, Vec::new())),
            Route::Other => Ok((StatusCode::ACCEPTED, Vec::new())),
            Route::Metrics(format) => decode_isolated(format, body)
                .await
                .map(|metrics| (StatusCode::ACCEPTED, metrics)),
        },
    };
    let (status, metrics, error) = match outcome {
        Ok((status, metrics)) => (status, metrics, None),
        Err(Refusal(status, error)) => {
            tracing::warn!(intake = recorder.recipient().name, %path, %status, %error, "Refused a request.");
            (status, Vec::new(), Some(error))
        }
    };

    let request = RequestRecord {
        received_ms,
        path,
        api_key,
        status: status.as_u16(),
        series: metrics.len(),
        error,
    };
    recorder.record(request, metrics);
    status
}

#[cfg(test)]
mod tests;

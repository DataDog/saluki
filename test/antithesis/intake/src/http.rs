//! Axum HTTP surface for the intake.
//!
//! The `/api/v2/series` route stacks a measurement middleware ahead of the
//! decompression layer so the W5 (compressed size), W6 (uncompressed size),
//! and W22 (content-length) checks can read both the on-the-wire body length
//! and the decompressed body length. The middleware records the compressed
//! length, the `Content-Encoding`, and the declared `Content-Length` before
//! `RequestDecompressionLayer` consumes the encoding and strips the header,
//! then hands the request along unchanged with those measurements riding as
//! request extensions.

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderMap, StatusCode},
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tower::ServiceBuilder;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{handlers, intake, state::AppState};

/// Memory backstop on the body buffered by the measurement middleware. Sits
/// well above any spec-admitted body so it never preempts the W5 strict-below
/// check. Decompressed sketch payloads can be tens of MiB.
const MAX_BUFFERED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Compressed request-body length recorded before decompression and attached
/// as a request extension for the W5/W22 checks.
#[derive(Clone, Copy, Debug)]
pub struct CompressedLen(pub u64);

/// Whether the request entered the decompression path. Recorded from the
/// `Content-Encoding` header before the decompression layer consumes it,
/// attached as a request extension so the handler knows whether the W6
/// uncompressed bound applies.
#[derive(Clone, Copy, Debug)]
pub struct DecompressionApplied(pub bool);

/// The `Content-Length` value declared on the wire, read before the
/// decompression layer strips the header. Attached as a request extension so
/// the W22 check compares the declared length against the measured wire body
/// length rather than against the post-decompression headers, where the header
/// is absent.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredContentLength(pub Option<u64>);

/// Build the intake router. `/api/v2/series` carries the W-property assertions;
/// the other metric routes merge into the same store so `/metrics/dump`
/// reflects everything the Agent delivered. The fallback returns `200 OK` so
/// the Agent's connectivity probes and any unmodelled endpoint succeed.
pub fn build_router(state: AppState) -> Router {
    // W1-W6 and W22 need the compressed body and raw headers, so the series
    // route runs `measure_compressed_size` outermost, then decompresses, then
    // lifts the body limit (the middleware's own cap is the backstop).
    let series = post(intake::handle_series).layer(
        ServiceBuilder::new()
            .layer(from_fn(measure_compressed_size))
            .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
            .layer(DefaultBodyLimit::disable()),
    );
    // v1 series and sketches do not carry W assertions but still arrive
    // compressed, so they decompress before merging into the dump store.
    let decompress = || {
        ServiceBuilder::new()
            .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
            .layer(DefaultBodyLimit::max(MAX_BUFFERED_BODY_BYTES))
    };

    Router::new()
        .route("/ready", get(|| async { StatusCode::OK }))
        .route("/metrics/dump", get(handlers::handle_metrics_dump))
        .route("/api/v2/series", series)
        .route("/api/v1/series", post(handlers::handle_series_v1).layer(decompress()))
        .route("/api/beta/sketches", post(handlers::handle_sketch).layer(decompress()))
        .fallback(handlers::fallback)
        .with_state(state)
}

fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

fn decompression_applied(headers: &HeaderMap) -> bool {
    let Some(enc) = headers.get("content-encoding").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let enc = enc.trim();
    ["deflate", "gzip", "zstd"]
        .iter()
        .any(|known| enc.eq_ignore_ascii_case(known))
}

/// Buffer the body, count its bytes, observe the `Content-Encoding` and
/// `Content-Length` headers before the decompression layer consumes them, then
/// hand the request along unchanged with the measurements riding as extensions.
async fn measure_compressed_size(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BUFFERED_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let len = bytes.len() as u64;
    let applied = decompression_applied(&parts.headers);
    let declared = declared_content_length(&parts.headers);
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(CompressedLen(len));
    req.extensions_mut().insert(DecompressionApplied(applied));
    req.extensions_mut().insert(DeclaredContentLength(declared));
    next.run(req).await
}

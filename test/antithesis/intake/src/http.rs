//! Axum HTTP surface for the intake.
//!
<<<<<<< HEAD
//! This module composes the intake router while submodules keep protocol groups
//! and middleware separate.

use axum::{http::StatusCode, Router};

mod datadog;
pub(crate) mod middleware;
mod state;

use self::state::AppState;

/// Memory backstop on the compressed body buffered before decompression. Sits above any Pyld05 spec limit.
const MAX_COMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Caps the decompressed body a handler buffers. Exceeds every Pyld06 spec limit.
const MAX_DECOMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Build the intake router. `/api/v2/series` fires payload assertions. Datadog endpoints answer
/// 202. A malformed body gets 400. An oversized body gets 413. Unmatched paths answer 200.
pub fn build_router() -> Router {
    Router::new()
        .merge(datadog::routes())
        .fallback(|| async { StatusCode::OK })
        .with_state(AppState::default())
=======
//! The `/api/v2/series` route stacks measurement middleware ahead of the
//! decompression layer so Pyld05 (compressed size), Pyld06 (uncompressed size),
//! and Pyld22 (content-length) can read both the on-the-wire and decompressed
//! body lengths, recorded as request extensions before `RequestDecompressionLayer`
//! consumes the encoding headers.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request},
    http::StatusCode,
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use headers::{ContentEncoding, ContentLength, HeaderMapExt};
use tower::ServiceBuilder;
use tower_http::decompression::RequestDecompressionLayer;

use crate::intake;

/// Memory backstop on the compressed body buffered before decompression, above any Pyld05 spec limit
const MAX_COMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Wire measurements recorded before decompression, attached as a request extension for Pyld05/Pyld06/Pyld22
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measurements {
    /// Compressed, on-the-wire body length, read before decompression.
    pub(crate) compressed_len: u64,
    /// Whether the request entered the decompression path.
    pub(crate) decompression_applied: bool,
    /// The declared `Content-Length`, or `None` when the header was absent.
    pub(crate) declared_content_length: Option<u64>,
}

/// Build the intake router, `/api/v2/series` for payload assertions, others return 200 OK
pub fn build_router(hostname: Arc<str>) -> Router {
    // Pyld01-Pyld06 and Pyld22 need the compressed body and raw headers, so the series
    // route runs `measure_compressed_size` outermost, then decompresses, then
    // lifts the body limit (the middleware's own cap is the backstop).
    let series = post(intake::handle_series).layer(
        ServiceBuilder::new()
            .layer(from_fn(measure_compressed_size))
            .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
            .layer(DefaultBodyLimit::disable()),
    );

    Router::new()
        .route("/api/v2/series", series)
        .fallback(|| async { StatusCode::OK })
        .with_state(hostname)
}

/// Buffer the body and record compressed size, encoding, and content-length before decompression
async fn measure_compressed_size(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_COMPRESSED_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let len = bytes.len() as u64;
    let applied = parts
        .headers
        .typed_get::<ContentEncoding>()
        .is_some_and(|enc| enc.contains("deflate") || enc.contains("gzip") || enc.contains("zstd"));
    let declared = parts.headers.typed_get::<ContentLength>().map(|cl| cl.0);
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(Measurements {
        compressed_len: len,
        decompression_applied: applied,
        declared_content_length: declared,
    });
    next.run(req).await
>>>>>>> 9c1abdeb85 (enhancement(antithesis): Introduce rig intake API (#1826))
}

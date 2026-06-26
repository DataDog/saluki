//! Axum HTTP surface for the intake.
//!
//! This module composes the intake router while submodules keep protocol groups
//! and middleware separate.

<<<<<<< HEAD
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

use std::sync::Arc;

=======
>>>>>>> c156ece189 (chore(antithesis): Adjust how host tags are checked, transmitted (#1932))
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
}

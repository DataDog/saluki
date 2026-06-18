//! Axum HTTP surface for the intake.
//!
//! This module composes the intake router while submodules keep protocol groups
//! and middleware separate.

use std::sync::Arc;

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
pub fn build_router(hostname: Arc<str>) -> Router {
    Router::new()
        .merge(datadog::routes())
        .fallback(|| async { StatusCode::OK })
        .with_state(AppState { hostname })
}

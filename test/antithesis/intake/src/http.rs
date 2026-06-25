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

/// Build the intake router, `/api/v2/series` for payload assertions, others return 200 OK.
pub fn build_router(hostname: Arc<str>) -> Router {
    Router::new()
        .merge(datadog::routes())
        .fallback(|| async { StatusCode::OK })
        .with_state(AppState { hostname })
}

//! Axum HTTP surface for the intake.
//!
//! This module composes the full router while the submodules keep protocol
//! groups separate:
//!
//! - `datadog`: Datadog-compatible intake and health routes:
//!   - `POST /api/v2/series`
//!   - `POST /api/beta/sketches`
//!   - `POST /api/v1/events_batch`
//!   - `POST /api/v1/events`
//!   - `POST /intake/`
//!   - `POST /api/v1/check_run`
//!   - `GET /api/v1/validate`
//! - `antithesis`: private scenario-control routes used by Antithesis drivers:
//!   - `GET /antithesis/metrics/{target}`
//! - `middleware`: request body measurement used by payload assertions.
//! - `state`: shared router state for one capture target.

mod antithesis;
mod datadog;
pub(crate) mod middleware;
pub mod state;

use axum::{http::StatusCode, Router};

use self::state::AppState;

/// Memory backstop on the compressed body buffered before decompression. Sits above any Pyld05 spec limit.
const MAX_COMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Caps the decompressed body a handler buffers. Exceeds every Pyld06 spec limit.
const MAX_DECOMPRESSED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Build the intake router. `/api/v2/series` fires payload assertions. Datadog endpoints answer
/// 202. A malformed body gets 400. An oversized body gets 413. Unmatched paths answer 200.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(datadog::routes())
        .merge(antithesis::routes())
        .fallback(|| async { StatusCode::OK })
        .with_state(state)
}

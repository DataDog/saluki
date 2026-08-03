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
use http_body_util::LengthLimitError;

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

/// Whether a body read failed because it overran the byte cap rather than because the read itself
/// failed. Only the cap is the producer's fault: a mid-body read failure is what an injected network
/// fault looks like, and blaming a size property for that would redden a lane the fault broke.
pub(crate) fn body_over_cap(e: axum::Error) -> bool {
    e.into_inner().downcast_ref::<LengthLimitError>().is_some()
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};

    use super::body_over_cap;

    // The cap error comes from axum's own read path rather than a hand-built one, so this pins how
    // axum wraps it. A read failure must not be mistaken for it.
    #[tokio::test]
    async fn body_over_cap_separates_the_cap_from_a_read_failure() {
        let over_cap = to_bytes(Body::from("ab"), 1).await.expect_err("body exceeds the cap");
        assert!(body_over_cap(over_cap));
        assert!(!body_over_cap(axum::Error::new(std::io::Error::other(
            "connection reset by peer"
        ))));
    }
}

//! Datadog-compatible HTTP intake routes.
//!
//! This module owns the public routes that Datadog Agent and ADP send to:
//!
//! - `POST /api/v2/series`: accepts metric series payloads, records payload
//!   shape assertions, and captures scalar metric contexts.
//! - `POST /api/beta/sketches`: accepts distribution sketch payloads and
//!   captures sketch contexts.
//! - `POST /api/v1/events_batch`: accepts protobuf event batches.
//! - `POST /api/v1/events`: accepts JSON event intake payloads and rejects
//!   malformed bodies.
//! - `POST /intake/`: accepts the shared JSON intake endpoint and ignores
//!   non-event bodies.
//! - `POST /api/v1/check_run`: accepts service check payloads.
//! - `GET /api/v1/validate`: accepts Datadog Agent connectivity validation.

mod events;
mod metrics;
mod service_checks;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    middleware::from_fn,
    routing::{get, post},
    Router,
};
use tower::ServiceBuilder;
use tower_http::{decompression::RequestDecompressionLayer, limit::RequestBodyLimitLayer};

use super::{middleware::measure_compressed_size, state::AppState, MAX_COMPRESSED_BODY_BYTES};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .merge(series_route())
        .merge(decoded_payload_routes())
        .route("/api/v1/validate", get(|| async { StatusCode::OK }))
}

/// The `/api/v2/series` route. Pyld01-Pyld06 and Pyld22 need the compressed body and raw headers.
/// `measure_compressed_size` runs outermost, then decompression. The route lifts the default body
/// limit. The middleware cap is the backstop.
fn series_route() -> Router<AppState> {
    let layers = ServiceBuilder::new()
        .layer(from_fn(measure_compressed_size))
        .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
        .layer(DefaultBodyLimit::disable());
    Router::new().route("/api/v2/series", post(metrics::handle_series).layer(layers))
}

/// Routes that decompress and parse a body without recording `Measurements`. One shared stack
/// caps the wire body with `RequestBodyLimitLayer` as it streams. Decompression follows. Each
/// handler caps the decompressed body with `to_bytes`.
fn decoded_payload_routes() -> Router<AppState> {
    Router::new()
        .route("/api/beta/sketches", post(metrics::handle_sketches))
        .route("/api/v1/events_batch", post(events::handle_events_batch))
        .route("/api/v1/events", post(events::handle_events_v1))
        .route("/intake/", post(events::handle_intake))
        .route("/api/v1/check_run", post(service_checks::handle_check_run_v1))
        .layer(
            ServiceBuilder::new()
                .layer(RequestBodyLimitLayer::new(MAX_COMPRESSED_BODY_BYTES))
                .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true)),
        )
}

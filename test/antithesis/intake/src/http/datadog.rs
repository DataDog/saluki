//! Datadog-compatible intake routes.

use axum::{extract::DefaultBodyLimit, middleware::from_fn, routing::post, Router};
use tower::ServiceBuilder;
use tower_http::decompression::RequestDecompressionLayer;

use self::metrics::handle_series;
use super::middleware::measure_compressed_size;
use super::state::AppState;

mod metrics;

/// Build Datadog-compatible intake routes.
pub(crate) fn routes() -> Router<AppState> {
    // Pyld01-Pyld06 and Pyld22 need the compressed body and raw headers, so the series
    // route runs `measure_compressed_size` outermost, then decompresses, then
    // lifts the body limit (the middleware's own cap is the backstop).
    let series = post(handle_series).layer(
        ServiceBuilder::new()
            .layer(from_fn(measure_compressed_size))
            .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
            .layer(DefaultBodyLimit::disable()),
    );

    Router::new().route("/api/v2/series", series)
}

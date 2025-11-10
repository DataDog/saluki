use axum::{extract::DefaultBodyLimit, Router};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};

mod metrics;
mod misc;

pub fn initialize_app_router() -> Router {
    Router::new()
        .merge(metrics::build_metrics_router())
        .merge(misc::build_misc_router())
        // Ensure we can handle compressed requests.
        .route_layer(RequestDecompressionLayer::new().deflate(true).zstd(true))
        .route_layer(CompressionLayer::new().zstd(true))
        // Decompressed metrics payloads can be large (~62MB for sketches).
        .route_layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

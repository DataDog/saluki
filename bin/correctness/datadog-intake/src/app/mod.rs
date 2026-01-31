use axum::{
    extract::DefaultBodyLimit,
    http::{StatusCode, Uri},
    Router,
};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::info;

mod metrics;
mod misc;
mod traces;

pub fn initialize_app_router() -> Router {
    Router::new()
        .merge(metrics::build_metrics_router())
        .merge(traces::build_traces_router())
        .merge(misc::build_misc_router())
        .fallback(debug_fallback_handler)
        // Ensure we can handle compressed requests.
        .route_layer(RequestDecompressionLayer::new().deflate(true).gzip(true).zstd(true))
        .route_layer(CompressionLayer::new().zstd(true))
        // Decompressed metrics payloads can be large (~62MB for sketches).
        .route_layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

async fn debug_fallback_handler(uri: Uri) -> StatusCode {
    info!("Got unhandled request: path={}", uri);

    StatusCode::OK
}

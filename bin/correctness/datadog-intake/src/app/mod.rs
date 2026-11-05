use axum::{
    extract::{DefaultBodyLimit, Request},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::info;

mod dogstatsd_forwarding;
mod events;
mod logs;
mod metrics;
mod misc;
mod service_checks;
mod traces;

pub use self::dogstatsd_forwarding::DogStatsDForwardingState;

pub fn initialize_app_router(dogstatsd_forwarding_state: DogStatsDForwardingState) -> Router {
    let events_state = events::EventsState::new();
    let logs_state = logs::LogsState::new();
    let service_checks_state = service_checks::ServiceChecksState::new();

    Router::new()
        .route("/ready", get(ready_handler))
        .merge(dogstatsd_forwarding::build_dogstatsd_forwarding_router(
            dogstatsd_forwarding_state,
        ))
        .merge(events::build_events_router(events_state.clone()))
        .merge(logs::build_logs_router(logs_state))
        .merge(metrics::build_metrics_router())
        .merge(service_checks::build_service_checks_router(service_checks_state))
        .merge(traces::build_traces_router())
        .merge(misc::build_misc_router(events_state))
        .fallback(debug_fallback_handler)
        // Ensure we can handle compressed requests.
        .route_layer(RequestDecompressionLayer::new().deflate(true).gzip(true).zstd(true))
        .route_layer(CompressionLayer::new().zstd(true))
        // Decompressed metrics payloads can be large (~62MB for sketches).
        .route_layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // Outermost, so that handlers can see the headers as sent, before decompression removes `Content-Encoding`.
        .route_layer(middleware::from_fn(capture_original_headers))
}

/// The request headers as the client sent them, before any layer modified them.
#[derive(Clone)]
pub struct OriginalHeaders(pub HeaderMap);

async fn capture_original_headers(mut request: Request, next: Next) -> Response {
    let headers = request.headers().clone();
    request.extensions_mut().insert(OriginalHeaders(headers));
    next.run(request).await
}

async fn ready_handler() -> StatusCode {
    StatusCode::OK
}

async fn debug_fallback_handler(uri: Uri) -> StatusCode {
    info!("Got unhandled request: path={}", uri);

    StatusCode::OK
}

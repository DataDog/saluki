use axum::{
    extract::DefaultBodyLimit,
    http::{StatusCode, Uri},
    routing::get,
    Router,
};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::info;

mod dogstatsd_forwarding;
mod events;
mod metrics;
mod misc;
mod service_checks;
mod stateful_logs;
mod traces;

pub use self::dogstatsd_forwarding::DogStatsDForwardingState;
pub use self::stateful_logs::{StatefulLogsGrpcService, StatefulLogsState};

pub fn initialize_app_router(
    dogstatsd_forwarding_state: DogStatsDForwardingState, stateful_logs_state: StatefulLogsState,
) -> Router {
    let events_state = events::EventsState::new();
    let service_checks_state = service_checks::ServiceChecksState::new();

    Router::new()
        .route("/ready", get(ready_handler))
        .merge(dogstatsd_forwarding::build_dogstatsd_forwarding_router(
            dogstatsd_forwarding_state,
        ))
        .merge(events::build_events_router(events_state.clone()))
        .merge(metrics::build_metrics_router())
        .merge(service_checks::build_service_checks_router(service_checks_state))
        .merge(stateful_logs::build_stateful_logs_router(stateful_logs_state))
        .merge(traces::build_traces_router())
        .merge(misc::build_misc_router(events_state))
        .fallback(debug_fallback_handler)
        // Ensure we can handle compressed requests.
        .route_layer(RequestDecompressionLayer::new().deflate(true).gzip(true).zstd(true))
        .route_layer(CompressionLayer::new().zstd(true))
        // Decompressed metrics payloads can be large (~62MB for sketches).
        .route_layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

async fn ready_handler() -> StatusCode {
    StatusCode::OK
}

async fn debug_fallback_handler(uri: Uri) -> StatusCode {
    info!("Got unhandled request: path={}", uri);

    StatusCode::OK
}

use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
use self::state::MetricsState;

pub fn build_metrics_router() -> Router {
    Router::new()
        .route("/metrics/dump", get(handle_metrics_dump))
        .route("/api/v1/series", post(handle_series_v1))
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/intake/metrics/v3/series", post(handle_metrics_v3))
        .route("/api/intake/metrics/v3beta/series", post(handle_metrics_v3))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .route("/api/intake/metrics/v3/sketches", post(handle_metrics_v3))
        .with_state(MetricsState::new())
}

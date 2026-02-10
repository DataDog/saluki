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
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .route("/api/v3/series", post(handle_series_v3))
        .route("/api/v3/sketch", post(handle_sketch_v3))
        .with_state(MetricsState::new())
}

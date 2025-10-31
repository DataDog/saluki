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
        .with_state(MetricsState::new())
}

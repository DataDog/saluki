use axum::{
    routing::{get, post},
    Router,
};
use tokio::sync::watch;

mod handlers;
use self::handlers::*;

mod state;
use self::state::MetricsState;

pub fn build_metrics_router() -> (Router, watch::Receiver<usize>) {
    let (state, series_rx) = MetricsState::new();
    let router = Router::new()
        .route("/metrics/dump", get(handle_metrics_dump))
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .with_state(state);
    (router, series_rx)
}

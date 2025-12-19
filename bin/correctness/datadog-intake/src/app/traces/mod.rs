use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
use self::state::TracesState;

pub fn build_traces_router() -> Router {
    Router::new()
        .route("/traces/dump_spans", get(handle_trace_spans_dump))
        .route("/traces/dump_stats", get(handle_trace_stats_dump))
        .route("/api/v0.2/traces", post(handle_v02_traces))
        .route("/api/v0.2/stats", post(handle_v02_stats))
        .with_state(TracesState::new())
}

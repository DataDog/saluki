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
        .route("/traces/dump", get(handle_traces_dump))
        .route("/api/v0.2/traces", post(handle_v02_traces))
        .with_state(TracesState::new())
}

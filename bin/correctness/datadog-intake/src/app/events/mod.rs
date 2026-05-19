use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::{EventsState, IntakePayload};

pub fn build_events_router(state: EventsState) -> Router {
    Router::new()
        .route("/events/dump", get(handle_events_dump))
        .route("/api/v1/events_batch", post(handle_events_batch))
        .with_state(state)
}

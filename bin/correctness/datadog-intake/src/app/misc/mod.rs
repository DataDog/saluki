use axum::{
    routing::{get, post},
    Router,
};

use crate::app::events::EventsState;

mod handlers;
use self::handlers::*;

pub fn build_misc_router(events_state: EventsState) -> Router {
    Router::new()
        .route("/api/v1/validate", get(handle_validate_v1))
        .route("/api/v1/metadata", post(handle_metadata_v1))
        .route("/api/v1/check_run", post(handle_check_run_v1))
        .route("/api/v1/events", post(handle_events_v1))
        .route("/intake/", post(handle_intake))
        .with_state(events_state)
}

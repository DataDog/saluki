use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::{CheckRunItem, ServiceChecksState};

pub fn build_service_checks_router(state: ServiceChecksState) -> Router {
    Router::new()
        .route("/service_checks/dump", get(handle_service_checks_dump))
        .route("/api/v1/check_run", post(handle_check_run_v1))
        .with_state(state)
}

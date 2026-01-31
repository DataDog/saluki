use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

pub fn build_misc_router() -> Router {
    Router::new()
        .route("/api/v1/validate", get(handle_validate_v1))
        .route("/api/v1/metadata", post(handle_metadata_v1))
        .route("/api/v1/check_run", post(handle_check_run_v1))
        .route("/intake/", post(handle_intake))
}

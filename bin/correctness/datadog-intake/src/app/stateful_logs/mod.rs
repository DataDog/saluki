use axum::{
    routing::{get, post},
    Router,
};

mod grpc;
pub use self::grpc::StatefulLogsGrpcService;

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::StatefulLogsState;

pub fn build_stateful_logs_router(state: StatefulLogsState) -> Router {
    Router::new()
        .route("/stateful-logs/dump", get(handle_stateful_logs_dump))
        .route("/api/v2/logs", post(handle_unsupported_http_logs))
        .with_state(state)
}

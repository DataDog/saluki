use axum::{extract::State, http::StatusCode, Json};
use stele::StatefulLog;
use tracing::{info, warn};

use super::StatefulLogsState;

pub async fn handle_stateful_logs_dump(State(state): State<StatefulLogsState>) -> Json<Vec<StatefulLog>> {
    info!("Got request to dump stateful logs.");
    Json(state.dump_logs())
}

pub async fn handle_unsupported_http_logs() -> StatusCode {
    warn!("Received logs HTTP payload on the stateful logs correctness intake; only gRPC stateful logs are decoded.");
    StatusCode::ACCEPTED
}

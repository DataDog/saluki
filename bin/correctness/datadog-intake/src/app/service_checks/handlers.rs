use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use stele::ServiceCheck;
use tracing::{error, info};

use super::{CheckRunItem, ServiceChecksState};

pub async fn handle_service_checks_dump(State(state): State<ServiceChecksState>) -> Json<Vec<ServiceCheck>> {
    info!("Got request to dump service checks.");
    Json(state.dump_checks())
}

pub async fn handle_check_run_v1(State(state): State<ServiceChecksState>, body: Bytes) -> StatusCode {
    info!("Received check_run v1 payload.");

    let items = match serde_json::from_slice::<Vec<CheckRunItem>>(&body) {
        Ok(items) => items,
        Err(e) => {
            error!(error = %e, "Failed to parse check_run v1 JSON payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    state.merge_check_run_payload(items);
    info!("Processed check_run v1 payload.");

    StatusCode::ACCEPTED
}

use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use datadog_protos::traces::AgentPayload;
use protobuf::Message as _;
use tracing::{debug, error};

use super::TracesState;

pub async fn handle_traces_dump(State(state): State<TracesState>) -> Json<Vec<()>> {
    Json(state.dump_traces())
}

pub async fn handle_v02_traces(State(state): State<TracesState>, body: Bytes) -> StatusCode {
    debug!("Received v0.2 traces payload.");

    let payload = match AgentPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse trace payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_agent_payload(payload) {
        Ok(()) => {
            debug!("Processed trace payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge trace payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

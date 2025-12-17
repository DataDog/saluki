use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use base64::Engine;
use datadog_protos::traces::{AgentPayload, StatsPayload};
use protobuf::Message as _;
use stele::Span;
use tracing::{debug, error};

use super::TracesState;

pub async fn handle_traces_dump(State(state): State<TracesState>) -> Json<Vec<Span>> {
    Json(state.dump())
}

pub async fn handle_v02_traces(State(state): State<TracesState>, body: Bytes) -> StatusCode {
    tracing::info!("Received v0.2 traces payload.");

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

pub async fn handle_v02_stats(State(state): State<TracesState>, body: Bytes) -> StatusCode {
    debug!("Received v0.2 stats payload.");

    let payload = match rmp_serde::from_slice::<StatsPayload>(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = ?e, "Failed to parse stats payload.");
            error!(
                "Raw payload: {}",
                base64::engine::general_purpose::STANDARD.encode(body)
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    tracing::info!("Received stats payload. ({} bytes)", body.len());

    match state.merge_stats_payload(payload) {
        Ok(()) => {
            debug!("Processed stats payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge stats payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use serde_json::Value;
use tracing::{info, warn};

use super::ApmTelemetryState;

/// Handles `GET /agent-telemetry/dump` — returns all collected APM telemetry payloads as JSON.
pub async fn handle_agent_telemetry_dump(State(state): State<ApmTelemetryState>) -> Json<Vec<Value>> {
    info!("Got request to dump agent telemetry payloads.");
    Json(state.dump_payloads())
}

/// Handles `POST /api/v2/apmtelemetry` — stores the raw JSON payload for later retrieval.
pub async fn handle_apmtelemetry(State(state): State<ApmTelemetryState>, body: Bytes) -> StatusCode {
    match serde_json::from_slice::<Value>(&body) {
        Ok(payload) => {
            info!("Received APM telemetry payload.");
            state.add_payload(payload);
            StatusCode::ACCEPTED
        }
        Err(e) => {
            warn!(error = %e, bytes = body.len(), "Failed to parse APM telemetry payload as JSON.");
            StatusCode::BAD_REQUEST
        }
    }
}

use axum::{body::Bytes, extract::State, http::StatusCode};
use tracing::{debug, warn};

use crate::app::events::{EventsState, IntakePayload};

pub async fn handle_validate_v1() -> StatusCode {
    debug!("Received validate v1 payload.");

    StatusCode::OK
}

pub async fn handle_metadata_v1() -> StatusCode {
    debug!("Received metadata v1 payload.");

    StatusCode::OK
}

pub async fn handle_check_run_v1() -> StatusCode {
    debug!("Received check_run v1 payload.");

    StatusCode::OK
}

pub async fn handle_events_v1(State(state): State<EventsState>, body: Bytes) -> StatusCode {
    debug!(bytes = body.len(), "Received events v1 payload.");

    let payload = match serde_json::from_slice::<IntakePayload>(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to parse events v1 JSON payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    state.merge_intake_payload(payload);

    StatusCode::ACCEPTED
}

pub async fn handle_intake(State(state): State<EventsState>, body: Bytes) -> StatusCode {
    debug!(bytes = body.len(), "Received intake payload.");

    let payload = match serde_json::from_slice::<IntakePayload>(&body) {
        Ok(p) => p,
        Err(e) => {
            // Not all intake payloads contain events (e.g. host metadata), so parse
            // failures are expected and non-fatal.
            debug!(error = %e, "Could not parse intake payload as JSON with events (non-fatal).");
            return StatusCode::OK;
        }
    };

    let event_count = payload
        .events
        .as_ref()
        .map(|m| m.values().map(|v| v.len()).sum::<usize>())
        .unwrap_or(0);

    if event_count > 0 {
        debug!(event_count, "Extracted events from intake payload.");
        state.merge_intake_payload(payload);
    }

    StatusCode::OK
}

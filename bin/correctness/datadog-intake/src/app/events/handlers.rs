use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use datadog_protos::events::EventsPayload;
use protobuf::Message as _;
use stele::Event;
use tracing::{error, info};

use super::EventsState;

pub async fn handle_events_dump(State(state): State<EventsState>) -> Json<Vec<Event>> {
    info!("Got request to dump events.");
    Json(state.dump_events())
}

pub async fn handle_events_batch(State(state): State<EventsState>, body: Bytes) -> StatusCode {
    info!("Received events batch payload.");

    let payload = match EventsPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse events batch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    state.merge_events_payload(payload);
    info!("Processed events batch payload.");

    StatusCode::ACCEPTED
}

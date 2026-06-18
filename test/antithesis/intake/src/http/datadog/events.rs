//! Event intake handlers.

use std::collections::HashMap;

use axum::{
    body::{to_bytes, Body},
    http::StatusCode,
};
use datadog_protos::events::EventsPayload;
use protobuf::Message;
use serde::Deserialize;
use tracing::{debug, error};

use crate::http::MAX_DECOMPRESSED_BODY_BYTES;

/// Handler for `POST /api/v1/events_batch`.
pub(crate) async fn handle_events_batch(body: Body) -> StatusCode {
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(error = %e, cap = MAX_DECOMPRESSED_BODY_BYTES, "Rejected events batch body at the decompressed cap.");
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    match EventsPayload::parse_from_bytes(&body) {
        Ok(_) => StatusCode::ACCEPTED,
        Err(e) => {
            error!(error = %e, "failed to parse events batch payload");
            StatusCode::BAD_REQUEST
        }
    }
}

/// Handler for `POST /api/v1/events`.
pub(crate) async fn handle_events_v1(body: Body) -> StatusCode {
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(error = %e, cap = MAX_DECOMPRESSED_BODY_BYTES, "Rejected events body at the decompressed cap.");
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    record_intake_events(&body, true)
}

/// Handler for `POST /intake/`.
pub(crate) async fn handle_intake(body: Body) -> StatusCode {
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(error = %e, cap = MAX_DECOMPRESSED_BODY_BYTES, "Rejected intake body at the decompressed cap.");
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    record_intake_events(&body, false)
}

fn record_intake_events(body: &[u8], strict: bool) -> StatusCode {
    let payload = match serde_json::from_slice::<IntakePayload>(body) {
        Ok(payload) => payload,
        Err(e) if strict => {
            error!(error = %e, "failed to parse events intake payload");
            return StatusCode::BAD_REQUEST;
        }
        Err(e) => {
            debug!(error = %e, "intake payload did not contain events");
            return StatusCode::OK;
        }
    };
    payload.touch();
    if strict {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

#[derive(Deserialize)]
struct IntakePayload {
    events: Option<HashMap<String, Vec<IntakeEvent>>>,
}

impl IntakePayload {
    fn touch(self) {
        let Some(events_by_source) = self.events else {
            return;
        };
        for events in events_by_source.into_values() {
            for event in events {
                event.touch();
            }
        }
    }
}

#[derive(Deserialize)]
struct IntakeEvent {
    msg_title: Option<String>,
    msg_text: Option<String>,
    alert_type: Option<String>,
    aggregation_key: Option<String>,
    host: Option<String>,
    priority: Option<String>,
    tags: Option<Vec<String>>,
    timestamp: Option<i64>,
}

impl IntakeEvent {
    fn touch(self) {
        let _ = (
            self.msg_title,
            self.msg_text,
            self.alert_type,
            self.aggregation_key,
            self.host,
            self.priority,
            self.tags,
            self.timestamp,
        );
    }
}

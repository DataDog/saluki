//! Service check intake handlers.

use axum::{
    body::{to_bytes, Body},
    http::StatusCode,
};
use serde::Deserialize;
use tracing::error;

use crate::http::MAX_DECOMPRESSED_BODY_BYTES;

/// Handler for `POST /api/v1/check_run`.
pub(crate) async fn handle_check_run_v1(body: Body) -> StatusCode {
    let body = match to_bytes(body, MAX_DECOMPRESSED_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!(error = %e, cap = MAX_DECOMPRESSED_BODY_BYTES, "Rejected check_run body at the decompressed cap.");
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    };
    let items = match serde_json::from_slice::<Vec<CheckRunItem>>(&body) {
        Ok(items) => items,
        Err(e) => {
            error!(error = %e, "failed to parse check_run payload");
            return StatusCode::BAD_REQUEST;
        }
    };
    for item in items {
        item.touch();
    }
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct CheckRunItem {
    #[serde(rename = "check")]
    name: String,
    status: u8,
    #[serde(rename = "host_name")]
    hostname: Option<String>,
    message: Option<String>,
    tags: Option<Vec<String>>,
    timestamp: Option<u64>,
}

impl CheckRunItem {
    fn touch(self) {
        let _ = (
            self.name,
            self.status,
            self.hostname,
            self.message,
            self.tags,
            self.timestamp,
        );
    }
}

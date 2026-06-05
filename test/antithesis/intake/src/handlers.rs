//! Non-asserting handlers: the metrics dump, the v1-series and sketch merge
//! endpoints, and the permissive fallback.
//!
//! These do not carry W-property assertions. The v1-series and sketch
//! endpoints exist so the Agent's other metric submissions still reach
//! `/metrics/dump`, which `finally_verify_delivery` polls. The fallback returns
//! `200 OK` so the Agent's connectivity probes and any unmodelled endpoint
//! succeed, matching the prior `datadog-intake` behaviour.

use axum::{body::Bytes, extract::State, http::StatusCode, http::Uri, Json};
use datadog_protos::metrics::SketchPayload;
use protobuf::Message as _;
use stele::Metric;
use tracing::{debug, error};

use crate::state::AppState;

/// `GET /metrics/dump`: returns every ingested metric in simplified form.
pub async fn handle_metrics_dump(State(state): State<AppState>) -> Json<Vec<Metric>> {
    Json(state.metrics.dump_metrics())
}

/// `POST /api/v1/series`: merges a legacy JSON series payload into the dump
/// store. A `{}` body is a connectivity probe and is accepted without merging.
pub async fn handle_series_v1(State(state): State<AppState>, body: Bytes) -> StatusCode {
    if body == b"{}"[..] {
        return StatusCode::ACCEPTED;
    }
    match state.metrics.merge_series_v1_payload(&body) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(e) => {
            error!(error = %e, "Failed to merge series v1 payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

/// `POST /api/beta/sketches`: merges a sketch payload into the dump store.
pub async fn handle_sketch(State(state): State<AppState>, body: Bytes) -> StatusCode {
    let payload = match SketchPayload::parse_from_bytes(&body) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse sketch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };
    match state.metrics.merge_sketch_payload(payload) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(e) => {
            error!(error = %e, "Failed to merge sketch payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

/// Permissive fallback for unmodelled endpoints and connectivity probes.
pub async fn fallback(uri: Uri) -> StatusCode {
    debug!("Got unhandled request: path={}", uri);
    StatusCode::OK
}

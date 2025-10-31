use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use protobuf::Message as _;
use stele::Metric;
use tracing::{debug, error};

use super::MetricsState;

pub async fn handle_metrics_dump(State(state): State<MetricsState>) -> Json<Vec<Metric>> {
    Json(state.dump_metrics())
}

pub async fn handle_series_v2(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    debug!("Received series payload.");

    let payload = match MetricPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse series payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_series_payload(payload) {
        Ok(()) => {
            debug!("Processed series payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge series payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

pub async fn handle_sketch_beta(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    debug!("Received sketch payload.");

    let payload = match SketchPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse sketch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_sketch_payload(payload) {
        Ok(()) => {
            debug!("Processed sketch payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge sketch payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

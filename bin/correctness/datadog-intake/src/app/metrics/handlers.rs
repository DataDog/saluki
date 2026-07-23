use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use datadog_protos::metrics::{v3::Payload as V3Payload, MetricPayload, SketchPayload};
use protobuf::Message as _;
use stele::Metric;
use tracing::{error, info};

use super::MetricsState;

pub async fn handle_metrics_dump(State(state): State<MetricsState>) -> Json<Vec<Metric>> {
    info!("Got request to dump metrics.");
    Json(state.dump_metrics())
}

pub async fn handle_series_v1(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    // Fast path check to see if this is a diagnostic request.
    //
    // The Datadog Agent will send dummy payloads to certain endpoints when checking for connectivity, so if we see `{}`
    // here, we can return early without parsing the payload.
    if body == b"{}"[..] {
        info!("Received diagnostic request for series v1 endpoint, ignoring.");
        return StatusCode::ACCEPTED;
    }

    info!("Received series v1 payload.");

    match state.merge_series_v1_payload(&body[..]) {
        Ok(()) => {
            info!("Processed series v1 payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge series v1 payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

pub async fn handle_series_v2(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    // Fast path check to see if this is a diagnostic request.
    //
    // The Datadog Agent will send dummy payloads to certain endpoints when checking for connectivity, so if we see `{}`
    // here, we can return early without parsing the payload.
    if body == b"{}"[..] {
        info!("Received diagnostic request for series v2 endpoint, ignoring.");
        return StatusCode::ACCEPTED;
    }

    info!("Received series v2 payload.");

    let payload = match MetricPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse series v2 payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_series_v2_payload(payload) {
        Ok(()) => {
            info!("Processed series v2 payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge series v2 payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

pub async fn handle_sketch_beta(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    info!("Received sketch payload.");

    let payload = match SketchPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse sketch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_sketch_payload(payload.clone()) {
        Ok(()) => {
            info!("Processed sketch payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            for sketch in &payload.sketches {
                for dogsketch in &sketch.dogsketches {
                    if dogsketch.cnt < 0 {
                        error!(
                            metric = sketch.metric(),
                            count = dogsketch.cnt,
                            bin_count = dogsketch.n.len(),
                            "Rejected sketch has a negative count."
                        );
                    }
                }
            }
            error!(error = %e, "Failed to merge sketch payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

pub async fn handle_metrics_v3(State(state): State<MetricsState>, body: Bytes) -> StatusCode {
    info!("Received metrics v3 payload.");

    let payload = match V3Payload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse metrics v3 payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_v3_payload(payload) {
        Ok(()) => {
            info!("Processed metrics v3 payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge metrics v3 payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

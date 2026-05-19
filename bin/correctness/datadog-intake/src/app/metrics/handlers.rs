use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use datadog_protos::metrics::v3::Payload as V3Payload;
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use protobuf::Message as _;
use stele::Metric;
use tracing::{error, info};

use super::MetricsState;

/// Extracts the validation batch headers from a request.
///
/// Returns `(batch_id, batch_seq, batch_len)` if all three headers are present, otherwise `None`.
fn extract_batch_info(headers: &HeaderMap) -> Option<(String, usize, usize)> {
    let id = headers.get("x-metrics-request-id")?.to_str().ok()?.to_string();
    let seq = headers.get("x-metrics-request-seq")?.to_str().ok()?.parse().ok()?;
    let len = headers.get("x-metrics-request-len")?.to_str().ok()?.parse().ok()?;
    Some((id, seq, len))
}

pub async fn handle_metrics_dump(State(state): State<MetricsState>) -> Json<Vec<Metric>> {
    info!("Got request to dump metrics.");
    Json(state.dump_metrics())
}

pub async fn handle_metrics_validation_status(State(state): State<MetricsState>) -> (StatusCode, String) {
    info!("Got request to dump metrics validation status.");

    let status = state.validation_status();
    if status.is_ok() {
        (StatusCode::OK, "ok\n".to_string())
    } else {
        let mut body = format!(
            "metrics validation failed: failures={}, pending_series_batches={}, pending_sketches_batches={}\n",
            status.failures.len(),
            status.pending_series_batches,
            status.pending_sketches_batches
        );
        for failure in status.failures {
            body.push_str("- ");
            body.push_str(&failure);
            body.push('\n');
        }
        (StatusCode::INTERNAL_SERVER_ERROR, body)
    }
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

pub async fn handle_series_v2(State(state): State<MetricsState>, headers: HeaderMap, body: Bytes) -> StatusCode {
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

    if let Some((batch_id, batch_seq, batch_len)) = extract_batch_info(&headers) {
        info!(batch_id, batch_seq, batch_len, "Received V2 series validation pair.");
        match state.accumulate_v2_series(payload, batch_id, batch_seq, batch_len) {
            Ok(()) => StatusCode::ACCEPTED,
            Err(e) => {
                error!(error = %e, "Failed to accumulate V2 series validation pair.");
                StatusCode::BAD_REQUEST
            }
        }
    } else {
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
}

pub async fn handle_sketch_beta(State(state): State<MetricsState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let payload = match SketchPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse sketch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Some((batch_id, batch_seq, batch_len)) = extract_batch_info(&headers) {
        info!(batch_id, batch_seq, batch_len, "Received V2 sketches validation pair.");
        match state.accumulate_v2_sketches(payload, batch_id, batch_seq, batch_len) {
            Ok(()) => StatusCode::ACCEPTED,
            Err(e) => {
                error!(error = %e, "Failed to accumulate V2 sketches validation pair.");
                StatusCode::BAD_REQUEST
            }
        }
    } else {
        info!("Received sketch payload.");
        match state.merge_sketch_payload(payload) {
            Ok(()) => {
                info!("Processed sketch payload.");
                StatusCode::ACCEPTED
            }
            Err(e) => {
                error!(error = %e, "Failed to merge sketch payload.");
                StatusCode::BAD_REQUEST
            }
        }
    }
}

pub async fn handle_series_v3(State(state): State<MetricsState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let payload = match V3Payload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse v3 series payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Some((batch_id, batch_seq, batch_len)) = extract_batch_info(&headers) {
        info!(batch_id, batch_seq, batch_len, "Received V3 series validation pair.");
        match state.accumulate_v3_series_and_merge(payload, batch_id, batch_seq, batch_len) {
            Ok(()) => StatusCode::ACCEPTED,
            Err(e) => {
                error!(error = %e, "Failed to accumulate V3 series validation pair.");
                StatusCode::BAD_REQUEST
            }
        }
    } else {
        info!("Received v3 series payload.");
        match state.merge_v3_payload(payload) {
            Ok(()) => {
                info!("Processed v3 series payload.");
                StatusCode::ACCEPTED
            }
            Err(e) => {
                error!(error = %e, "Failed to merge v3 series payload.");
                StatusCode::BAD_REQUEST
            }
        }
    }
}

pub async fn handle_sketch_v3(State(state): State<MetricsState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let payload = match V3Payload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse v3 sketch payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Some((batch_id, batch_seq, batch_len)) = extract_batch_info(&headers) {
        info!(batch_id, batch_seq, batch_len, "Received V3 sketches validation pair.");
        match state.accumulate_v3_sketches_and_merge(payload, batch_id, batch_seq, batch_len) {
            Ok(()) => StatusCode::ACCEPTED,
            Err(e) => {
                error!(error = %e, "Failed to accumulate V3 sketches validation pair.");
                StatusCode::BAD_REQUEST
            }
        }
    } else {
        info!("Received v3 sketch payload.");
        match state.merge_v3_payload(payload) {
            Ok(()) => {
                info!("Processed v3 sketch payload.");
                StatusCode::ACCEPTED
            }
            Err(e) => {
                error!(error = %e, "Failed to merge v3 sketch payload.");
                StatusCode::BAD_REQUEST
            }
        }
    }
}

use axum::{body::Bytes, extract::State, http::StatusCode, Json};
use datadog_protos::traces::{AgentPayload, StatsPayload};
use protobuf::Message as _;
use stele::{ClientStatisticsAggregator, IntakeRequest, Span};
use tracing::{error, info};

use super::TracesState;
use crate::app::RequestHead;

pub async fn handle_trace_spans_dump(State(state): State<TracesState>) -> Json<Vec<Span>> {
    info!("Got request to dump trace spans.");
    Json(state.dump_spans())
}

pub async fn handle_trace_stats_dump(State(state): State<TracesState>) -> Json<ClientStatisticsAggregator> {
    info!("Got request to dump trace stats.");
    Json(state.dump_stats())
}

pub async fn handle_trace_span_requests_dump(State(state): State<TracesState>) -> Json<Vec<IntakeRequest<Vec<Span>>>> {
    info!("Got request to dump trace span requests.");
    Json(state.dump_span_requests())
}

pub async fn handle_trace_stats_requests_dump(
    State(state): State<TracesState>,
) -> Json<Vec<IntakeRequest<ClientStatisticsAggregator>>> {
    info!("Got request to dump trace stats requests.");
    Json(state.dump_stats_requests())
}

pub async fn handle_v02_traces(State(state): State<TracesState>, head: RequestHead, body: Bytes) -> StatusCode {
    info!("Received v0.2 traces payload.");

    let payload = match AgentPayload::parse_from_bytes(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = %e, "Failed to parse trace payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_agent_payload(head, payload) {
        Ok(()) => {
            info!("Processed trace payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge trace payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

pub async fn handle_v02_stats(State(state): State<TracesState>, head: RequestHead, body: Bytes) -> StatusCode {
    info!("Received v0.2 stats payload.");

    let payload = match rmp_serde::from_slice::<StatsPayload>(&body[..]) {
        Ok(payload) => payload,
        Err(e) => {
            error!(error = ?e, "Failed to parse stats payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.merge_stats_payload(head, payload) {
        Ok(()) => {
            info!("Processed stats payload.");
            StatusCode::ACCEPTED
        }
        Err(e) => {
            error!(error = %e, "Failed to merge stats payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

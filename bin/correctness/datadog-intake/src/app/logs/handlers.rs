use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    Extension, Json,
};
use stele::{Log, LogsRequest};
use tracing::{error, info};

use super::LogsState;
use crate::app::OriginalHeaders;

pub async fn handle_logs_dump(State(state): State<LogsState>) -> Json<Vec<LogsRequest>> {
    info!("Got request to dump logs.");
    Json(state.dump_requests())
}

pub async fn handle_logs(
    State(state): State<LogsState>, uri: Uri, original_headers: Option<Extension<OriginalHeaders>>, headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let logs = match Log::try_from_payload(&body) {
        Ok(logs) => logs,
        Err(e) => {
            error!(error = %e, path = uri.path(), "Failed to parse logs payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Record the headers as the client sent them, before request decompression removed `Content-Encoding`.
    let headers = original_headers
        .map(|Extension(OriginalHeaders(headers))| headers)
        .unwrap_or(headers);
    let header_pairs = headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value)));

    state.merge_request(LogsRequest::new(uri.path(), header_pairs, logs));
    StatusCode::ACCEPTED
}

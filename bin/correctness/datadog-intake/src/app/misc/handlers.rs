use axum::http::StatusCode;
use tracing::debug;

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

pub async fn handle_intake() -> StatusCode {
    debug!("Received intake payload.");

    StatusCode::OK
}

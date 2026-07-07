//! Private HTTP routes used by the Antithesis scenario drivers.
//!
//! This module owns the private query/control routes:
//!
//! - `GET /antithesis/metrics/{target}`: returns one lane's captured contexts and the intake's
//!   current time for `agent` or `adp`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};

use super::state::AppState;
use crate::capture;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/antithesis/metrics/{target}", get(metrics))
}

async fn metrics(
    State(state): State<AppState>, Path(target): Path<String>,
) -> Result<Json<capture::LaneView>, StatusCode> {
    let Some(target) = capture::Target::parse(&target) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(now) = capture::EpochSeconds::now() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(Json(capture::LaneView {
        now,
        contexts: state.recorder.contexts(target),
    }))
}

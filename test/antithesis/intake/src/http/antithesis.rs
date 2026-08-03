//! Private HTTP routes used by the Antithesis scenario drivers.
//!
//! This module owns the private query/control routes:
//!
//! - `GET /antithesis/metrics/{target}`: returns one lane's captured contexts and the intake's
//!   current time for `agent` or `adp`.
//! - `GET /contexts?n=N`: serves `N` contexts from the shared pool over the binary codec, so the
//!   drivers render recurring identities.

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use harness::contexts::encode_response;
use rand::rand_core::UnwrapErr;
use serde::Deserialize;
use serde_json::json;

use super::state::AppState;
use crate::capture;

/// The largest working set a single `/contexts` request may ask for. A request over this is rejected
/// rather than served, so a malformed `n` cannot make the pool mint an unbounded response.
const MAX_CONTEXTS_PER_REQUEST: usize = 65_536;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/antithesis/metrics/{target}", get(metrics))
        .route("/contexts", get(contexts))
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

/// The `n` query parameter of `GET /contexts`.
#[derive(Debug, Deserialize)]
struct ContextQuery {
    /// How many contexts to serve.
    n: usize,
}

/// Serve `n` contexts from the shared pool, encoded with the binary codec so non-UTF-8 names and tags
/// round-trip. Rejects an out-of-range `n` with 400 and a pool read error with 500.
async fn contexts(State(state): State<AppState>, Query(query): Query<ContextQuery>) -> Result<Vec<u8>, StatusCode> {
    if query.n == 0 || query.n > MAX_CONTEXTS_PER_REQUEST {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut rng = UnwrapErr(AntithesisRng);
    let contexts = state
        .pool
        .serve(query.n, &mut rng)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    assert_reachable!("context source served a request", &json!({ "n": query.n }));
    Ok(encode_response(&contexts))
}

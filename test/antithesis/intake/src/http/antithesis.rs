//! Private HTTP routes used by the Antithesis scenario drivers.
//!
//! This module owns the private query/control routes:
//!
//! - `GET /antithesis/metrics/{target}`: returns one lane's captured contexts and the intake's
//!   current time for `agent` or `adp`.
//! - `GET /antithesis/curves/{target}`: returns one lane's settled aggregation curves and the
//!   intake's monotone high-water clock for `agent` or `adp`.
//! - `GET /contexts?n=N`: serves N contexts from the shared pool as a length-prefixed binary
//!   body the drivers draw their load from.

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use harness::context::encode_response;
use serde::Deserialize;
use serde_json::json;

use super::state::AppState;
use crate::capture;

/// Largest number of contexts one `/contexts` request may ask for, bounding the response size.
const MAX_CONTEXTS_PER_REQUEST: usize = 65_536;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/antithesis/metrics/{target}", get(metrics))
        .route("/antithesis/curves/{target}", get(curves))
        .route("/contexts", get(contexts))
}

/// The `n` query parameter of `GET /contexts`.
#[derive(Debug, Deserialize)]
struct ContextQuery {
    /// How many contexts to return. Required.
    n: Option<usize>,
}

/// `GET /contexts?n=N`: N contexts from the shared pool, length-prefixed binary
/// ([`encode_response`]). `n` is required and must be in `1..=MAX_CONTEXTS_PER_REQUEST`
/// (else `400`); an unreadable pool config is `500`.
async fn contexts(State(state): State<AppState>, Query(query): Query<ContextQuery>) -> Response {
    let Some(n) = query.n.filter(|&n| (1..=MAX_CONTEXTS_PER_REQUEST).contains(&n)) else {
        return (StatusCode::BAD_REQUEST, format!("n must be in 1..={MAX_CONTEXTS_PER_REQUEST}")).into_response();
    };
    match state.pool.serve(n, &mut AntithesisRng) {
        Ok(contexts) => {
            assert_reachable!("context source served a request", &json!({ "n": n }));
            (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                encode_response(&contexts),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /antithesis/curves/{target}`: one lane's settled aggregation curves plus the intake's
/// monotone high-water clock. The differential curve oracle reads this.
async fn curves(
    State(state): State<AppState>, Path(target): Path<String>,
) -> Result<Json<capture::LaneView>, StatusCode> {
    let Some(target) = capture::Target::parse(&target) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    Ok(Json(state.recorder.view(target)))
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use harness::config::ContextSourceConfig;
    use harness::context::decode_response;
    use tower::ServiceExt as _;

    use super::super::{build_router, state::AppState};
    use crate::capture;
    use crate::context_pool::Pool;

    /// A router whose pool is capped at `cap` via a temp `context_source.yaml`.
    fn router(cap: usize) -> axum::Router {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("ctxroute-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("create temp config dir");
        let yaml = ContextSourceConfig { context_cap: cap }.to_yaml().expect("render config");
        std::fs::write(dir.join("context_source.yaml"), yaml).expect("write config");
        let capture = capture::State::new();
        build_router(AppState::agent(&capture, &Arc::new(Pool::new(dir))))
    }

    #[tokio::test]
    async fn contexts_returns_n_decodable_contexts() {
        let response = router(1_000)
            .oneshot(Request::builder().uri("/contexts?n=5").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.expect("body");
        assert_eq!(decode_response(&body).map(|c| c.len()), Some(5));
    }

    #[tokio::test]
    async fn contexts_without_n_is_bad_request() {
        let response = router(1_000)
            .oneshot(Request::builder().uri("/contexts").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

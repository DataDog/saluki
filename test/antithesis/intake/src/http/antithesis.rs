//! Private HTTP routes used by the Antithesis scenario drivers.
//!
//! The two oracle routes assert in place. The intake owns the store, so shipping either comparison
//! out for a client to redo would be a lossy re-encoding of data that never needs to leave.
//!
//! - `POST /antithesis/metrics/contexts`: symmetric difference of the two lanes' context sets.
//! - `POST /antithesis/metrics/frechet_distance`: equivalence of each context's aggregation curve.
//! - `GET /contexts?n=N`: serves `N` contexts from the shared pool over the binary codec, so the
//!   drivers render recurring identities.

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
#[cfg(feature = "differential")]
use axum::routing::post;
#[cfg(feature = "differential")]
use axum::Json;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Router,
};
use harness::contexts::encode_response;
#[cfg(feature = "differential")]
use harness::Phase;
use rand::rand_core::UnwrapErr;
use serde::Deserialize;
use serde_json::json;

use super::state::AppState;
#[cfg(feature = "differential")]
use crate::capture;

/// The largest working set a single `/contexts` request may ask for. A request over this is rejected
/// rather than served, so a malformed `n` cannot make the pool mint an unbounded response.
const MAX_CONTEXTS_PER_REQUEST: usize = 65_536;

pub(super) fn routes() -> Router<AppState> {
    let router = Router::new().route("/contexts", get(contexts));
    // The oracle routes exist only in a differential build. See the `differential` feature in Cargo.toml
    // for why a runtime switch cannot do this job.
    #[cfg(feature = "differential")]
    let router = router
        .route("/antithesis/metrics/contexts", post(contexts_oracle))
        .route("/antithesis/metrics/frechet_distance", post(series_oracle));
    router
}

/// Body of `POST /antithesis/metrics/contexts`.
#[cfg(feature = "differential")]
#[derive(Debug, Deserialize)]
struct ContextsParams {
    /// Seconds a context may sit in the difference before it counts as a divergence.
    acceptable_flush_delay: i64,
    /// Which check posted. Picks the assertion name and the condition.
    phase: Phase,
}

/// Widest leash the series oracle accepts, in buckets.
///
/// The leash exists to forgive the flush-timing skew between the two lanes, which is a bucket or two. The
/// oracle runs at `W=1`, so eight is already generous, and a leash approaching the series length forgives
/// every reordering and measures nothing at all.
#[cfg(feature = "differential")]
const MAX_LEASH_WIDTH: usize = 8;

/// Body of `POST /antithesis/metrics/frechet_distance`.
#[cfg(feature = "differential")]
#[derive(Debug, Deserialize)]
struct SeriesParams {
    bucket_width: i64,
    leash_width: usize,
    equivalence_threshold: f64,
    /// Which check posted. Picks the assertion name and the condition.
    phase: Phase,
}

/// Compare the two lanes' context sets and assert. No result body.
#[cfg(feature = "differential")]
async fn contexts_oracle(
    State(state): State<AppState>, Json(params): Json<ContextsParams>,
) -> Result<StatusCode, StatusCode> {
    if params.acceptable_flush_delay < 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Some(now) = capture::EpochSeconds::now() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let report = state.recorder.compare_contexts(now, params.acceptable_flush_delay);
    let details = json!(report);

    // While load runs, a member still inside the budget is in flight rather than a divergence, so only an
    // overdue one is a fault. The finally check runs after load stops and asserts exact convergence: a
    // residual that arrived in this same second has age zero, which no budget can call overdue, so an
    // overdue-only test there would pass a genuine one-lane flush.
    // Antithesis catalogues an assertion by its literal name, so each phase needs its own call site.
    match params.phase {
        Phase::Eventually => assert_always!(
            report.overdue == 0,
            "differential.contexts_eventually_equivalent",
            &details
        ),
        Phase::Finally => assert_always!(
            report.diverged == 0,
            "differential.contexts_finally_converged",
            &details
        ),
    }
    if report.compared > 0 {
        assert_reachable!("differential.contexts_observed", &details);
    }
    Ok(StatusCode::OK)
}

/// Compare every context's aggregation curve across both lanes and assert. No result body.
#[cfg(feature = "differential")]
async fn series_oracle(
    State(state): State<AppState>, Json(params): Json<SeriesParams>,
) -> Result<StatusCode, StatusCode> {
    if params.bucket_width <= 0 || !params.equivalence_threshold.is_finite() || params.equivalence_threshold < 0.0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    // The leash is the one parameter the oracle would otherwise take unbounded, and it sizes the Fréchet
    // rows. Rejected rather than clamped, so a caller never gets a comparison it did not ask for.
    if params.leash_width > MAX_LEASH_WIDTH {
        return Err(StatusCode::BAD_REQUEST);
    }

    let report = state.recorder.compare_series(
        params.bucket_width,
        params.leash_width,
        params.equivalence_threshold,
        params.phase,
    );
    let details = json!(report);

    // Antithesis catalogues an assertion by its literal name, so each phase needs its own call site.
    match params.phase {
        Phase::Eventually => assert_always!(
            report.failed == 0,
            "differential.series_eventually_equivalent",
            &details
        ),
        Phase::Finally => assert_always!(report.failed == 0, "differential.series_finally_converged", &details),
    }
    // No failures reads as agreement only where something was compared. Asserting that as an always
    // would red early in a run, before either lane has flushed. A reachable fires instead: a run that
    // never compares a context reds for coverage, which is the fault we want caught.
    if !report.vacuous() {
        assert_reachable!("differential.series_compared_a_context", &details);
    }
    Ok(StatusCode::OK)
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

// Every test here posts to an oracle route, which only a differential build has.
#[cfg(all(test, feature = "differential"))]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::context_pool::Pool;
    use crate::http::build_router;

    /// POST a JSON body to one oracle and read the status. Neither oracle touches the pool, so its
    /// config path is never read.
    async fn post_oracle(path: &str, body: &Value) -> StatusCode {
        let config_dir = PathBuf::from("/nonexistent");
        let pool = Arc::new(Pool::new(config_dir.clone()));
        let app = build_router(AppState::agent(&capture::State::new(), pool, &config_dir));
        let request = Request::builder()
            .method("POST")
            .uri(format!("/antithesis/metrics/{path}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request");
        app.oneshot(request).await.expect("router response").status()
    }

    /// The parameters each check posts, less the phase.
    fn contexts_params() -> Value {
        json!({ "acceptable_flush_delay": 30 })
    }

    fn series_params() -> Value {
        json!({ "bucket_width": 10, "leash_width": 1, "equivalence_threshold": 0.02 })
    }

    fn with_phase(mut params: Value, phase: &str) -> Value {
        params["phase"] = json!(phase);
        params
    }

    // Both oracles take a phase, spelled as the checks send it. The phase picks which assertion the
    // handler fires, so a body without one is rejected rather than filed under the other phase's name.
    #[tokio::test]
    async fn an_oracle_takes_a_phase_and_requires_it() {
        for phase in ["eventually", "finally"] {
            assert_eq!(
                post_oracle("contexts", &with_phase(contexts_params(), phase)).await,
                StatusCode::OK
            );
            assert_eq!(
                post_oracle("frechet_distance", &with_phase(series_params(), phase)).await,
                StatusCode::OK
            );
        }

        assert!(post_oracle("contexts", &contexts_params()).await.is_client_error());
        assert!(post_oracle("frechet_distance", &series_params())
            .await
            .is_client_error());
    }

    // An unknown phase names no assertion, so the handler rejects it rather than picking one.
    #[tokio::test]
    async fn an_unknown_phase_is_rejected() {
        assert!(post_oracle("contexts", &with_phase(contexts_params(), "midway"))
            .await
            .is_client_error());
    }
}

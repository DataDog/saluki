//! End-to-end for value-bearing curve capture: drive real protobuf `/api/v2/series` payloads into
//! both lanes through the actual HTTP handlers, then read the captured curves back over the real
//! `/antithesis/curves/{target}` control route. A settled bucket surfaces its value; a within-lane
//! collision surfaces a conflict without last-writer-wins; an unsettled (future) bucket is withheld.
//!
//! Mirrors the old `count_oracle_e2e.rs`: same drive-through-HTTP, read-back-over-the-control-route
//! shape, now judging the value-bearing curve rather than a point-count skew.

use std::sync::Arc;

use antithesis_intake::capture;
use antithesis_intake::context_pool::Pool;
use antithesis_intake::http::build_router;
use antithesis_intake::http::state::AppState;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, MetricType};
use datadog_protos::metrics::MetricPayload;
use protobuf::Message as _;
use serde_json::Value;
use tower::ServiceExt as _;

/// A throwaway pool for capture tests that never hit `/contexts`, so the config dir is unread.
fn test_pool() -> Arc<Pool> {
    Arc::new(Pool::new(std::env::temp_dir()))
}

/// A one-series `/api/v2/series` payload for `name` carrying a single point `(timestamp, value)`.
fn series_payload(name: &str, timestamp: i64, value: f64) -> Vec<u8> {
    let mut payload = MetricPayload::new();
    let mut series = MetricSeries::new();
    series.set_metric(name.to_string());
    series.set_type(MetricType::COUNT);
    series.tags.push("host:antithesis-differential".into());
    let mut point = MetricPoint::new();
    point.value = value;
    point.timestamp = timestamp;
    series.points.push(point);
    payload.series.push(series);
    payload.write_to_bytes().expect("serialize MetricPayload")
}

/// POST one series into a lane and assert the intake accepted it.
async fn flush(lane: &AppState, name: &str, timestamp: i64, value: f64) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v2/series")
        .header("dd-api-key", "antithesis-test-api-key")
        .header("content-type", "application/x-protobuf")
        .body(Body::from(series_payload(name, timestamp, value)))
        .expect("build request");
    let status = build_router(lane.clone())
        .oneshot(request)
        .await
        .expect("router response")
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// Fetch a lane's settled curves over the real control route as raw JSON.
async fn curves(lane: &AppState, target: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/antithesis/curves/{target}"))
        .body(Body::empty())
        .expect("build request");
    let response = build_router(lane.clone())
        .oneshot(request)
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("read body");
    serde_json::from_slice(&body).expect("deserialize curves JSON")
}

/// The one served context with `name` in a curves response, or `None` when withheld.
fn context_by_name<'a>(view: &'a Value, name: &str) -> Option<&'a Value> {
    view["contexts"]
        .as_array()
        .expect("contexts array")
        .iter()
        .find(|c| c["name"] == name)
}

// A settled point recorded on the Datadog Agent lane surfaces on the curve with its value and no
// conflict. `1_600_000_000` (2020) is many years past the intake's wall clock, so it settles at once.
#[tokio::test]
async fn settled_bucket_surfaces_its_value() {
    let capture = capture::State::new();
    let agent = AppState::agent(&capture, &test_pool());
    flush(&agent, "adp.requests", 1_600_000_000, 7.0).await;

    let view = curves(&agent, "agent").await;
    let context = context_by_name(&view, "adp.requests").expect("context served");
    let buckets = context["buckets"].as_array().expect("buckets array");
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0]["bucket_start"], 1_600_000_000_u64);
    assert_eq!(buckets[0]["value"]["Scalar"], 7.0);
    assert_eq!(buckets[0]["conflict"], false);
}

// Two differing values at the same bucket-start on the same lane surface a conflict without
// last-writer-wins: the first value stands and the cell is flagged.
#[tokio::test]
async fn same_bucket_differing_values_surface_a_conflict() {
    let capture = capture::State::new();
    let adp = AppState::adp(&capture, &test_pool());
    flush(&adp, "adp.requests", 1_600_000_000, 1.0).await;
    flush(&adp, "adp.requests", 1_600_000_000, 2.0).await;

    let view = curves(&adp, "adp").await;
    let context = context_by_name(&view, "adp.requests").expect("context served");
    let bucket = &context["buckets"].as_array().expect("buckets array")[0];
    assert_eq!(bucket["value"]["Scalar"], 1.0, "the first value stands");
    assert_eq!(bucket["conflict"], true);
}

// A future bucket-start has not aged past the settle horizon, so the whole context is withheld from
// the served curve until the high-water clock catches up.
#[tokio::test]
async fn unsettled_future_bucket_is_withheld() {
    let capture = capture::State::new();
    let adp = AppState::adp(&capture, &test_pool());
    // Year 2096: far ahead of the intake's wall clock, so the bucket cannot be settled.
    flush(&adp, "adp.future", 4_000_000_000, 5.0).await;

    let view = curves(&adp, "adp").await;
    assert!(
        context_by_name(&view, "adp.future").is_none(),
        "unsettled context must be withheld"
    );
}

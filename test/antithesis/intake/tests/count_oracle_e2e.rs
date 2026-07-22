//! End-to-end for the differential count oracle: drive real protobuf `/api/v2/series` payloads into
//! both lanes through the actual HTTP handlers, read the captured lanes back over the real control
//! route, then judge them with the real check-side `count_skews`. A context flushed unevenly across
//! the lanes must show up as a point-count skew; an even one must not.

use antithesis_intake::capture;
use antithesis_intake::http::{build_router, state::AppState};
use antithesis_scenario_differential::contexts::{count_skews, LaneView};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, MetricType};
use datadog_protos::metrics::MetricPayload;
use protobuf::Message as _;
use tower::ServiceExt as _;

/// A one-series payload for `name`, so one POST is one flush of one context.
fn series_payload(name: &str) -> Vec<u8> {
    let mut payload = MetricPayload::new();
    let mut series = MetricSeries::new();
    series.set_metric(name.to_string());
    series.set_type(MetricType::COUNT);
    series.tags.push("host:antithesis-differential".into());
    let mut point = MetricPoint::new();
    point.value = 1.0;
    point.timestamp = 1_600_000_000;
    series.points.push(point);
    payload.series.push(series);
    payload.write_to_bytes().expect("serialize MetricPayload")
}

/// POST one series payload into a lane and assert the intake accepted it.
async fn flush(lane: AppState, name: &str) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v2/series")
        .header("dd-api-key", "antithesis-test-api-key")
        .header("content-type", "application/x-protobuf")
        .body(Body::from(series_payload(name)))
        .expect("build request");
    let status = build_router(lane)
        .oneshot(request)
        .await
        .expect("router response")
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// Fetch a lane's captured view over the real control route and deserialize the check-side shape.
async fn lane_view(lane: AppState, target: &str) -> LaneView {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/antithesis/metrics/{target}"))
        .body(Body::empty())
        .expect("build request");
    let response = build_router(lane).oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("read body");
    serde_json::from_slice(&body).expect("deserialize LaneView")
}

#[tokio::test]
async fn uneven_flushes_surface_as_a_count_skew() {
    let capture = capture::State::new();

    // Same context on both lanes, but adp flushes it four times to the agent's once: a real, large
    // point-count skew of the kind ADP double-flushing or the agent expiring early would produce.
    flush(AppState::agent(&capture), "adp.requests").await;
    for _ in 0..4 {
        flush(AppState::adp(&capture), "adp.requests").await;
    }

    let agent = lane_view(AppState::agent(&capture), "agent").await;
    let adp = lane_view(AppState::adp(&capture), "adp").await;
    let skews = count_skews(&agent, &adp, 1);

    println!(
        "uneven: {}",
        serde_json::to_string_pretty(&skews).expect("serialize skews")
    );
    assert_eq!(skews.len(), 1, "the shared context must be flagged");
    let first = serde_json::to_value(&skews[0]).expect("serialize skew");
    assert_eq!(first["name"], "adp.requests");
    assert!(skews[0].adp_points > skews[0].agent_points);
    assert!(skews[0].skew > 1);
}

#[tokio::test]
async fn even_flushes_stay_silent() {
    let capture = capture::State::new();

    // Identical flush counts on both lanes: the oracle must find nothing.
    for _ in 0..3 {
        flush(AppState::agent(&capture), "adp.requests").await;
        flush(AppState::adp(&capture), "adp.requests").await;
    }

    let agent = lane_view(AppState::agent(&capture), "agent").await;
    let adp = lane_view(AppState::adp(&capture), "adp").await;
    let skews = count_skews(&agent, &adp, 1);

    println!(
        "even: {}",
        serde_json::to_string_pretty(&skews).expect("serialize skews")
    );
    assert!(skews.is_empty(), "matched flush counts must not be a skew");
}

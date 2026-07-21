//! Reproduction: the intake must decode a real Datadog Agent `/api/v2/series`
//! payload, not just an ADP one.
//!
//! Both SUTs post zstd-compressed protobuf. ADP compresses with `async-compression`,
//! the same Rust library `tower-http` decodes with. The Datadog Agent compresses with
//! libzstd via `DataDog/zstd`. These cases exercise the router's real decode path for
//! each producer.

use std::io::Write as _;

use antithesis_intake::capture;
use antithesis_intake::http::{build_router, state::AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, MetricType};
use datadog_protos::metrics::MetricPayload;
use protobuf::Message as _;
use tokio::io::AsyncWriteExt as _;
use tower::ServiceExt as _;

/// A non-trivial protobuf `MetricPayload`, big enough to compress meaningfully.
fn payload_bytes() -> Vec<u8> {
    let mut payload = MetricPayload::new();
    for i in 0..2000 {
        let mut series = MetricSeries::new();
        series.set_metric(format!("adp.requests.{i}"));
        series.set_type(MetricType::COUNT);
        series.tags.push(format!("env:prod-{i}"));
        series.tags.push("host:antithesis-differential".into());
        let mut point = MetricPoint::new();
        point.value = f64::from(i);
        point.timestamp = 1_600_000_000 + i64::from(i);
        series.points.push(point);
        payload.series.push(series);
    }
    payload.write_to_bytes().expect("serialize MetricPayload")
}

/// Compress the way the Datadog Agent does: libzstd stream writer with a mid-stream
/// flush, as the Agent flushes between items.
fn agent_libzstd(raw: &[u8]) -> Vec<u8> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("zstd encoder");
    let half = raw.len() / 2;
    enc.write_all(&raw[..half]).expect("write half");
    enc.flush().expect("mid-stream flush");
    enc.write_all(&raw[half..]).expect("write rest");
    enc.finish().expect("finish zstd")
}

/// Compress the way ADP does: `async-compression` zstd.
async fn adp_async_compression(raw: &[u8]) -> Vec<u8> {
    let mut enc = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
    enc.write_all(raw).await.expect("write");
    enc.shutdown().await.expect("shutdown");
    enc.into_inner()
}

/// A `User-Agent` the backend classifies as `SourceAgent`, the source whose feral v2 tags are kept.
const AGENT_UA: &str = "datadog-agent/7.55.0";

async fn post_series(encoding: Option<&str>, user_agent: Option<&str>, body: Vec<u8>) -> StatusCode {
    let pool = std::sync::Arc::new(antithesis_intake::context_pool::Pool::new(std::env::temp_dir()));
    let state = AppState::agent(&capture::State::new(), &pool);
    let app = build_router(state);
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v2/series")
        .header("dd-api-key", "antithesis-test-api-key")
        .header("content-type", "application/x-protobuf");
    if let Some(enc) = encoding {
        builder = builder.header("content-encoding", enc);
    }
    if let Some(ua) = user_agent {
        builder = builder.header("user-agent", ua);
    }
    let request = builder.body(Body::from(body)).expect("build request");
    app.oneshot(request).await.expect("router response").status()
}

#[tokio::test]
async fn identity_protobuf_decodes() {
    let raw = payload_bytes();
    assert_eq!(post_series(None, Some(AGENT_UA), raw).await, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn adp_async_compression_zstd_decodes() {
    let raw = payload_bytes();
    let body = adp_async_compression(&raw).await;
    assert_eq!(
        post_series(Some("zstd"), Some(AGENT_UA), body).await,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn agent_libzstd_zstd_decodes() {
    let raw = payload_bytes();
    let body = agent_libzstd(&raw);
    assert_eq!(
        post_series(Some("zstd"), Some(AGENT_UA), body).await,
        StatusCode::ACCEPTED
    );
}

/// A `MetricPayload` with a marker in the metric name and one tag, serialized. The caller
/// corrupts the chosen marker to non-UTF-8, mimicking what the Datadog Agent forwards.
fn payload_with_marker() -> Vec<u8> {
    let mut payload = MetricPayload::new();
    let mut series = MetricSeries::new();
    series.set_metric("NAME".into());
    series.set_type(MetricType::COUNT);
    series.tags.push("TAGG:antithesis-differential".into());
    let mut point = MetricPoint::new();
    point.value = 1.0;
    point.timestamp = 1_600_000_000;
    series.points.push(point);
    payload.series.push(series);
    payload.write_to_bytes().expect("serialize")
}

fn corrupt(mut bytes: Vec<u8>, marker: &[u8]) -> Vec<u8> {
    let pos = bytes
        .windows(marker.len())
        .position(|w| w == marker)
        .expect("find marker");
    for b in &mut bytes[pos..pos + marker.len()] {
        *b = 0xFF;
    }
    bytes
}

/// The stock rust-protobuf parser rejects any non-UTF-8 string outright — this is the
/// strictness that dropped whole agent-lane payloads for one bad byte.
#[test]
fn non_utf8_rejected_by_strict_protobuf_decode() {
    let decoded = MetricPayload::parse_from_bytes(&corrupt(payload_with_marker(), b"TAGG"));
    assert!(
        decoded.is_err(),
        "expected strict decode to reject non-UTF-8, got {decoded:?}"
    );
}

/// From the `datadog-agent` source a non-UTF-8 tag must not drop the payload. The backend retries into
/// a tags-as-bytes message ONLY for that source and keeps it, so the intake captures its contexts.
#[tokio::test]
async fn non_utf8_tag_accepted_from_agent_source() {
    let body = agent_libzstd(&corrupt(payload_with_marker(), b"TAGG"));
    assert_eq!(
        post_series(Some("zstd"), Some(AGENT_UA), body).await,
        StatusCode::ACCEPTED
    );
}

/// From any non-agent source the same non-UTF-8 tag whole-payload-rejects: the backend runs its
/// bytes-tag retry only for `SourceAgent`, so a non-`datadog-agent` User-Agent gets 400. This is the
/// primary source-gating contract the intake previously violated by sanitizing tags for every source.
#[tokio::test]
async fn non_utf8_tag_rejected_from_non_agent_source() {
    let corrupt_body = corrupt(payload_with_marker(), b"TAGG");
    // A recognizably non-agent User-Agent, and the absent-header case, both reject.
    assert_eq!(
        post_series(Some("zstd"), Some("vector/0.30"), agent_libzstd(&corrupt_body)).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_series(Some("zstd"), None, agent_libzstd(&corrupt_body)).await,
        StatusCode::BAD_REQUEST
    );
}

/// A non-UTF-8 metric name still drops the payload even from the Agent source — the backend's fallback
/// re-types only tags as bytes, so a bad name fails there too. The intake must match, not over-accept.
#[tokio::test]
async fn non_utf8_metric_name_dropped_even_from_agent_source() {
    let body = agent_libzstd(&corrupt(payload_with_marker(), b"NAME"));
    assert_eq!(
        post_series(Some("zstd"), Some(AGENT_UA), body).await,
        StatusCode::BAD_REQUEST
    );
}

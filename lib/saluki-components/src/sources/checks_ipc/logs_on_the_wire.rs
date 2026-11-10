//! Characterization tests for check logs, from the Checks IPC data model to the Datadog logs encoder's request.
//!
//! These tests record what the encoder sends for check logs today, byte for byte. A change to the wire format should
//! show up here as a change to an expected request or entry. The comparison is exact on purpose: key order and string
//! escaping are part of the format the Agent sends, so a parsed-JSON comparison would miss changes to them.
//!
//! The tests stop at the request the encoder builds. Headers that the forwarder adds when it sends a request, and the
//! transport itself, aren't covered here.
//!
//! Today, each entry wraps the check's message in a JSON-encoded `{"message": ...}` object, carries a lowercase
//! status, and gets an `@timestamp` from the encoder's clock. Check logs have no hostname, service, or source, so
//! those keys are absent.

use std::time::Duration;

use chrono::{DateTime, Utc};
use datadog_protos::checks::{check_data::Data, log::Log as ProtoLog, log::LogLevel};
use http_body_util::BodyExt as _;
use saluki_core::components::test_util::TestComponentDriver;
use stringtheory::MetaString;

use super::check_data_to_event;
use crate::encoders::{BufferedIncrementalConfiguration, DatadogLogsConfiguration};

/// The encoder's clock in these tests, as it appears in `@timestamp`.
const NOW: &str = "2026-01-02T03:04:05.678Z";

fn fixed_clock() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW)
        .expect("NOW should be RFC3339")
        .with_timezone(&Utc)
}

/// Returns the entry for a check log, given its `message` value as a JSON string literal.
fn entry(message: &str, status: &str) -> String {
    format!(
        r#"{{"@timestamp":"{}","message":{},"status":"{}"}}"#,
        NOW, message, status
    )
}

/// A request the logs encoder produced, with its body decompressed.
#[derive(Debug, PartialEq)]
struct WireRequest {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn log_data(level: i32, message: &str) -> Data {
    Data::Log(ProtoLog {
        message: message.to_string(),
        level,
    })
}

/// Converts `data` the way the Checks IPC source does, and runs the resulting logs through the logs encoder.
///
/// The encoder compresses with zstd, as it does by default. Its flush timeout is long enough that it flushes only when
/// it stops, so the logs from one call share a request unless they exceed a payload limit.
async fn encode_check_data(data: impl IntoIterator<Item = Data>) -> Vec<WireRequest> {
    let events = data
        .into_iter()
        .filter_map(|data| check_data_to_event(data, &MetaString::from_static("default-host")))
        .collect::<Vec<_>>();

    let encoder = BufferedIncrementalConfiguration::new(
        DatadogLogsConfiguration::new("zstd", 3).with_clock(fixed_clock),
        Duration::from_secs(60),
    );
    let (control, mut payloads) = TestComponentDriver::encoder(encoder)
        .await
        .expect("logs encoder should build");
    control.send_events(events).await;
    control.shutdown().await.expect("logs encoder should stop cleanly");

    let mut requests = Vec::new();
    for payload in payloads.collect().await {
        let (_, request) = payload
            .try_into_http_payload()
            .expect("logs are sent over HTTP")
            .into_parts();
        let (parts, body) = request.into_parts();
        let body = body.collect().await.expect("body should collect").to_bytes();
        let body = zstd::stream::decode_all(body.as_ref()).expect("body should decompress");
        requests.push(WireRequest {
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            headers: parts
                .headers
                .iter()
                .map(|(name, value)| {
                    let value = value.to_str().expect("header value should be text");
                    (name.to_string(), value.to_string())
                })
                .collect(),
            body: String::from_utf8(body).expect("body should be UTF-8"),
        });
    }
    requests
}

/// Encodes a single check log, and returns the one entry in the one request it produces.
async fn encode_check_log(level: i32, message: &str) -> String {
    let requests = encode_check_data([log_data(level, message)]).await;
    let [request] = requests.as_slice() else {
        panic!("one check log should produce one request, got {requests:?}");
    };
    request
        .body
        .strip_prefix('[')
        .and_then(|body| body.strip_suffix(']'))
        .unwrap_or_else(|| panic!("body should be a JSON array: {}", request.body))
        .to_string()
}

#[tokio::test]
async fn request() {
    let requests = encode_check_data([
        log_data(LogLevel::Info as i32, "first"),
        log_data(LogLevel::Error as i32, "second"),
    ])
    .await;

    let expected = WireRequest {
        method: "POST".to_string(),
        uri: "/api/v2/logs".to_string(),
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("content-encoding".to_string(), "zstd".to_string()),
        ],
        body: [
            "[",
            &entry(r#""{\"message\":\"first\"}""#, "info"),
            ",",
            &entry(r#""{\"message\":\"second\"}""#, "error"),
            "]",
        ]
        .concat(),
    };
    assert_eq!(vec![expected], requests);
}

#[tokio::test]
async fn levels() {
    let cases = [
        (
            LogLevel::Unspecified as i32,
            entry(r#""{\"message\":\"hello\"}""#, "info"),
        ),
        (LogLevel::Trace as i32, entry(r#""{\"message\":\"hello\"}""#, "trace")),
        (LogLevel::Debug as i32, entry(r#""{\"message\":\"hello\"}""#, "debug")),
        (LogLevel::Info as i32, entry(r#""{\"message\":\"hello\"}""#, "info")),
        (LogLevel::Warning as i32, entry(r#""{\"message\":\"hello\"}""#, "warn")),
        (LogLevel::Error as i32, entry(r#""{\"message\":\"hello\"}""#, "error")),
        (
            LogLevel::Critical as i32,
            entry(r#""{\"message\":\"hello\"}""#, "emergency"),
        ),
    ];

    for (level, expected) in cases {
        assert_eq!(expected, encode_check_log(level, "hello").await, "level {level}");
    }
}

#[tokio::test]
async fn messages() {
    let cases = [
        ("", entry(r#""{\"message\":\"\"}""#, "info")),
        (
            r#"{"key":"value","n":1}"#,
            entry(r#""{\"message\":\"{\\\"key\\\":\\\"value\\\",\\\"n\\\":1}\"}""#, "info"),
        ),
        (
            r#"say "hi" \ bye"#,
            entry(r#""{\"message\":\"say \\\"hi\\\" \\\\ bye\"}""#, "info"),
        ),
        (
            "line one\nline two\ttabbed",
            entry(r#""{\"message\":\"line one\\nline two\\ttabbed\"}""#, "info"),
        ),
        (
            "<a href=\"x\">&amp;</a>",
            entry(r#""{\"message\":\"<a href=\\\"x\\\">&amp;</a>\"}""#, "info"),
        ),
        (
            "café ☕ 日本語 \u{2028}",
            entry("\"{\\\"message\\\":\\\"café ☕ 日本語 \u{2028}\\\"}\"", "info"),
        ),
    ];

    for (message, expected) in cases {
        assert_eq!(
            expected,
            encode_check_log(LogLevel::Info as i32, message).await,
            "message {message:?}"
        );
    }
}

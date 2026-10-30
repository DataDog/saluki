//! Characterization tests for check logs, from the Checks IPC data model to the Datadog logs encoder's request.
//!
//! These tests record what ADP sends for check logs today, byte for byte, rather than what the Agent sends. A change to
//! the wire format should show up here as a change to an expected request or entry.
//!
//! Today, each entry wraps the check's message in a JSON-encoded `{"message": ...}` object, carries the capitalized
//! status, and gets an `@timestamp` from the encoder's clock. Check logs have no hostname, service, or source, so
//! those keys are absent.

use agent_data_plane_config::SharedConfiguration;
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

/// Converts `data` the way the Checks IPC source does, and runs the resulting logs through the logs encoder, built as
/// the topology builds it from a default configuration.
async fn encode_check_data(data: impl IntoIterator<Item = Data>) -> Vec<WireRequest> {
    let events = data
        .into_iter()
        .filter_map(|data| check_data_to_event(data, &MetaString::from_static("default-host")))
        .collect::<Vec<_>>();

    let compression = SharedConfiguration::default().endpoints.compression;
    let encoder = BufferedIncrementalConfiguration::from_encoder_builder(
        DatadogLogsConfiguration::new(&compression.compressor_kind, compression.effective_zstd_level())
            .with_clock(fixed_clock),
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
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"first\"}","status":"Info"}"#,
            ",",
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"second\"}","status":"Error"}"#,
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
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Info"}"#,
        ),
        (
            LogLevel::Trace as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Trace"}"#,
        ),
        (
            LogLevel::Debug as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Debug"}"#,
        ),
        (
            LogLevel::Info as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Info"}"#,
        ),
        (
            LogLevel::Warning as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Warning"}"#,
        ),
        (
            LogLevel::Error as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Error"}"#,
        ),
        (
            LogLevel::Critical as i32,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"hello\"}","status":"Emergency"}"#,
        ),
    ];

    for (level, expected) in cases {
        assert_eq!(expected, encode_check_log(level, "hello").await, "level {level}");
    }
}

#[tokio::test]
async fn unknown_level_is_not_sent() {
    // 99 isn't part of the `LogLevel` enum, so the source drops the log, and the encoder has nothing to send.
    let requests = encode_check_data([log_data(99, "hello")]).await;
    assert_eq!(Vec::<WireRequest>::new(), requests);
}

#[tokio::test]
async fn messages() {
    let cases = [
        (
            "",
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"\"}","status":"Info"}"#,
        ),
        (
            r#"{"key":"value","n":1}"#,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"{\\\"key\\\":\\\"value\\\",\\\"n\\\":1}\"}","status":"Info"}"#,
        ),
        (
            r#"say "hi" \ bye"#,
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"say \\\"hi\\\" \\\\ bye\"}","status":"Info"}"#,
        ),
        (
            "line one\nline two\ttabbed",
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"line one\\nline two\\ttabbed\"}","status":"Info"}"#,
        ),
        (
            "<a href=\"x\">&amp;</a>",
            r#"{"@timestamp":"2026-01-02T03:04:05.678Z","message":"{\"message\":\"<a href=\\\"x\\\">&amp;</a>\"}","status":"Info"}"#,
        ),
        (
            "café ☕ 日本語 \u{2028}",
            "{\"@timestamp\":\"2026-01-02T03:04:05.678Z\",\"message\":\"{\\\"message\\\":\\\"café ☕ 日本語 \u{2028}\\\"}\",\"status\":\"Info\"}",
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

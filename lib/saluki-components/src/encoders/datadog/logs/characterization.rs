//! Characterization of the requests that the Datadog logs encoder builds.
//!
//! These tests run the encoder the way the topology does, wrapped in `BufferedIncrementalConfiguration`, and record
//! what it does today: the request line and headers, compression, the 1000-log cap, splitting by compressed size, the
//! handling of a single oversized log, and that no log is lost or reordered across requests. They don't describe what
//! the encoder should do. Where the target behavior differs, an ignored test states it, named after the step of the
//! OTLP logs wire format work (DataDog/saluki#2970) that adopts it:
//!
//! - `intake-headers`: the target logs sender sets `DD-PROTOCOL: agent-json`, `DD-EVP-ORIGIN: agent`,
//!   `DD-EVP-ORIGIN-VERSION`, and `dd-message-timestamp`. It also sets `dd-current-timestamp` on each send attempt,
//!   which belongs to the forwarder rather than the encoder.
//! - `logs-sender-batching`: the target logs sender batches up to 1000 logs or 5 MB of content before compression,
//!   and drops a single log whose content is over that limit.
//!
//! Every test sends at most 1024 logs, so that they all go to the encoder in a single event buffer. The encoder handles
//! a whole buffer before its flush timeout can fire, so the requests depend only on the logs, not on timing.

use std::collections::BTreeMap;
use std::time::Duration;

use async_compression::tokio::bufread::GzipDecoder;
use http_body_util::BodyExt as _;
use saluki_core::components::test_util::TestComponentDriver;
use saluki_core::data_model::event::{log::Log, Event};
use saluki_metrics::test::TestRecorder;
use serde_json::Value;
use tokio::io::AsyncReadExt as _;

use super::{DatadogLogsConfiguration, MAX_LOGS_PER_PAYLOAD};
use crate::common::datadog::test_util::incompressible_string;
use crate::common::datadog::DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT;
use crate::encoders::BufferedIncrementalConfiguration;

/// The target logs sender's limit on the content of a batch, before compression.
const TARGET_BATCH_MAX_CONTENT_SIZE: usize = 5_000_000;

/// The flush timeout that the topology gives the encoder by default.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// A request built by the encoder.
struct EncodedRequest {
    method: http::Method,
    path: String,
    /// Header names and values, in `HeaderMap` iteration order, which is arbitrary.
    headers: Vec<(String, String)>,
    /// The number of events in the payload metadata.
    event_count: usize,
    compressed_len: usize,
    /// The body after decompression.
    body: Vec<u8>,
}

impl EncodedRequest {
    /// Returns the messages of the logs in the request, parsed the way the fake intake parses them.
    fn messages(&self) -> Vec<String> {
        stele::Log::try_from_payload(&self.body)
            .expect("payload should parse")
            .iter()
            .map(|log| match log.display_message() {
                Some(Value::String(message)) => message.clone(),
                other => panic!("log should have a string message, got {:?}", other),
            })
            .collect()
    }
}

/// Encodes `logs` with the given compression, and returns the requests the encoder builds.
async fn encode(compressor_kind: &str, logs: Vec<Log>) -> Vec<EncodedRequest> {
    assert!(logs.len() <= 1024, "logs must fit in a single event buffer");

    let config =
        BufferedIncrementalConfiguration::new(DatadogLogsConfiguration::new(compressor_kind, 3), FLUSH_TIMEOUT);
    let (control, mut payloads) = TestComponentDriver::encoder(config)
        .await
        .expect("logs encoder should build");
    control.send_events(logs.into_iter().map(Event::Log)).await;
    control.shutdown().await.expect("logs encoder should stop cleanly");

    let mut requests = Vec::new();
    for payload in payloads.collect().await {
        let (metadata, request) = payload
            .try_into_http_payload()
            .expect("logs are sent over HTTP")
            .into_parts();
        let (parts, body) = request.into_parts();
        let compressed = body.collect().await.expect("body should collect").to_bytes();
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| {
                let value = value.to_str().expect("header value should be ASCII");
                (name.to_string(), value.to_string())
            })
            .collect::<Vec<_>>();
        let encoding = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .map(|value| value.to_str().expect("content encoding should be ASCII"));
        let body = match encoding {
            Some("zstd") => zstd::stream::decode_all(compressed.as_ref()).expect("body should decompress"),
            Some("gzip") => {
                let mut body = Vec::new();
                GzipDecoder::new(compressed.as_ref())
                    .read_to_end(&mut body)
                    .await
                    .expect("body should decompress");
                body
            }
            other => panic!("unexpected content encoding {:?}", other),
        };

        requests.push(EncodedRequest {
            method: parts.method,
            path: parts.uri.to_string(),
            headers,
            event_count: metadata.event_count(),
            compressed_len: compressed.len(),
            body,
        });
    }
    requests
}

/// Returns `count` logs with distinct messages of about `len` bytes that compress well.
fn compressible_logs(count: usize, len: usize) -> Vec<Log> {
    (0..count)
        .map(|index| Log::new(format!("{:06}:{}", index, "a".repeat(len.saturating_sub(7)))))
        .collect()
}

/// Returns `count` logs with messages of `len` pseudo-random alphanumeric characters, which barely compress.
///
/// Each log's characters come from a generator seeded with its index, so every run produces the same logs.
fn incompressible_logs(count: usize, len: usize) -> Vec<Log> {
    (0..count)
        .map(|index| Log::new(incompressible_string(len, index as u64)))
        .collect()
}

/// Sorts header pairs, so that comparisons don't depend on `HeaderMap` iteration order.
fn sorted(mut headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers.sort();
    headers
}

/// Returns the logs the encoder counted as dropped because it failed to encode or send them.
fn encoder_failure_drops(recorder: &TestRecorder) -> Option<u64> {
    recorder.counter((
        "component_events_dropped_total",
        &[
            ("component_id", "test"),
            ("component_type", "encoder"),
            ("intentional", "false"),
            ("drop_reason", "encoder_failure"),
        ][..],
    ))
}

fn messages_of(logs: &[Log]) -> Vec<String> {
    logs.iter().map(|log| log.message().to_string()).collect()
}

/// Returns the messages of every request, in order, checking that each request's metadata counts its logs.
fn all_messages(requests: &[EncodedRequest]) -> Vec<String> {
    requests
        .iter()
        .flat_map(|request| {
            let messages = request.messages();
            assert_eq!(
                request.event_count,
                messages.len(),
                "metadata should count the request's logs"
            );
            messages
        })
        .collect()
}

#[tokio::test]
async fn request_line_and_headers() {
    for (compressor_kind, content_encoding) in [("zstd", "zstd"), ("gzip", "gzip")] {
        let requests = encode(compressor_kind, vec![Log::new("hello")]).await;
        assert_eq!(requests.len(), 1, "{}", compressor_kind);
        let request = &requests[0];

        assert_eq!(request.method, http::Method::POST);
        assert_eq!(request.path, "/api/v2/logs");
        // Only the content headers: none of the target `DD-*` headers yet (`intake-headers`).
        assert_eq!(
            sorted(request.headers.clone()),
            sorted(vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("content-encoding".to_string(), content_encoding.to_string()),
            ]),
            "{}",
            compressor_kind
        );
        assert_eq!(request.event_count, 1);
        assert_eq!(request.messages(), vec!["hello".to_string()]);
    }
}

#[tokio::test]
async fn unknown_compressor_falls_back_to_zstd() {
    let requests = encode("brotli", vec![Log::new("hello")]).await;
    assert_eq!(requests.len(), 1);
    let encoding = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name == "content-encoding")
        .map(|(_, value)| value.as_str());
    assert_eq!(encoding, Some("zstd"));
}

#[tokio::test]
async fn caps_requests_at_1000_logs() {
    assert_eq!(MAX_LOGS_PER_PAYLOAD, 1000);
    let logs = compressible_logs(1001, 64);
    let requests = encode("zstd", logs.clone()).await;

    let counts = requests.iter().map(|request| request.event_count).collect::<Vec<_>>();
    assert_eq!(counts, vec![1000, 1]);
    assert_eq!(all_messages(&requests), messages_of(&logs));
}

#[tokio::test]
async fn splits_requests_by_compressed_size() {
    // About 6.5 MB of content that compresses to roughly three quarters of its size, so it doesn't fit under the
    // compressed size limit in one request.
    let logs = incompressible_logs(400, 16 * 1024);
    let requests = encode("zstd", logs.clone()).await;

    let counts = requests.iter().map(|request| request.event_count).collect::<Vec<_>>();
    // The request builder starts a new request once its compressed size estimate, about the realized zstd ratio times
    // the logs written, would pass 99% of the compressed limit. The exact split point depends on that ratio, so only
    // check that the logs were split and that none went missing.
    assert!(
        counts.len() > 1,
        "expected the logs to be split across requests, got {:?}",
        counts
    );
    assert_eq!(counts.iter().sum::<usize>(), logs.len());
    for request in &requests {
        assert!(
            request.compressed_len <= DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
            "compressed request of {} bytes is over the limit",
            request.compressed_len
        );
    }

    // No request is cut off mid-log. `all_messages` parses every request as a JSON array of logs, and checks that no log
    // is lost or reordered across the requests.
    for request in &requests {
        assert!(request.body.ends_with(b"}]"), "a request should end with its last log");
    }
    assert_eq!(all_messages(&requests), messages_of(&logs));
}

#[tokio::test]
async fn does_not_split_compressible_requests_by_content_size() {
    // About 10 MB of content that compresses to almost nothing. The encoder limits requests by their compressed size,
    // so this fits in one request, and only the 1000-log cap applies.
    let logs = compressible_logs(1000, 10_000);
    let requests = encode("zstd", logs.clone()).await;

    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.len() > TARGET_BATCH_MAX_CONTENT_SIZE);
    assert_eq!(all_messages(&requests), messages_of(&logs));
}

#[tokio::test]
async fn drops_a_single_log_over_the_compressed_size_limit() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let oversized = incompressible_logs(1, DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT * 3 / 2).remove(0);
    let logs = vec![Log::new("before"), oversized, Log::new("after")];
    let requests = encode("zstd", logs).await;

    // Before the compressor has written a block, the request builder doesn't estimate the compressed size, so the
    // oversized log joins the first request. That request is then too large and gets split in half, and the half that
    // holds only the oversized log is dropped.
    let messages = requests.iter().map(EncodedRequest::messages).collect::<Vec<_>>();
    assert_eq!(messages, vec![vec!["before".to_string()], vec!["after".to_string()]]);

    // NOTE: the dropped log isn't counted (DataDog/saluki#3045). The encoder only logs the error of a split half it can't
    // send, while a log it can't encode counts toward `component_events_dropped_total`. Once the drop is counted, this
    // should become 1.
    assert_eq!(encoder_failure_drops(&recorder), Some(0));
}

#[tokio::test]
async fn sends_a_compressible_log_over_the_target_content_limit() {
    // The encoder only limits a single log by its compressed size, so a log over the target content limit is sent.
    let logs = compressible_logs(1, TARGET_BATCH_MAX_CONTENT_SIZE + 1_000_000);
    let requests = encode("zstd", logs.clone()).await;

    assert_eq!(all_messages(&requests), messages_of(&logs));
}

#[tokio::test]
#[ignore = "target for `intake-headers` (DataDog/saluki#2970)"]
async fn target_intake_headers() {
    let requests = encode("zstd", vec![Log::new("hello")]).await;
    let headers = requests[0]
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(headers.get("dd-protocol"), Some(&"agent-json"));
    assert_eq!(headers.get("dd-evp-origin"), Some(&"agent"));
    assert!(headers.contains_key("dd-evp-origin-version"));
    let message_timestamp = headers
        .get("dd-message-timestamp")
        .expect("dd-message-timestamp should be set");
    assert!(
        message_timestamp.parse::<u64>().is_ok(),
        "dd-message-timestamp should be in milliseconds"
    );
}

#[tokio::test]
#[ignore = "target for `logs-sender-batching` (DataDog/saluki#2970)"]
async fn target_logs_sender_batching_splits_at_the_target_content_limit() {
    let logs = compressible_logs(1000, 10_000);
    let requests = encode("zstd", logs.clone()).await;

    assert!(requests.len() > 1);
    for request in &requests {
        assert!(request.body.len() <= TARGET_BATCH_MAX_CONTENT_SIZE);
    }
    assert_eq!(all_messages(&requests), messages_of(&logs));
}

#[tokio::test]
#[ignore = "target for `logs-sender-batching` (DataDog/saluki#2970)"]
async fn target_logs_sender_batching_drops_a_log_over_the_target_content_limit() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let mut logs = vec![Log::new("before")];
    logs.extend(compressible_logs(1, TARGET_BATCH_MAX_CONTENT_SIZE + 1_000_000));
    logs.push(Log::new("after"));
    let requests = encode("zstd", logs).await;

    let messages = requests
        .into_iter()
        .flat_map(|request| request.messages())
        .collect::<Vec<_>>();
    assert_eq!(messages, vec!["before".to_string(), "after".to_string()]);
    assert_eq!(encoder_failure_drops(&recorder), Some(1));
}

//! Golden vectors for OTLP logs: the target output of a reference implementation next to the current output.
//!
//! Each vector in [`vectors`] is an OTLP request and the target logs on the wire for it. The test sends
//! every request to the OTLP source over gRPC, encodes the logs the source dispatches with the Datadog logs encoder,
//! the way the topology connects `otlp_in.logs` to `dd_logs_encode`, and parses the encoded request body with
//! `stele::Log`, the parser the fake intake uses.
//!
//! The test checks two things for every vector:
//!
//! - **Current output.** The output must match the vector's current fixture exactly. Vectors whose current output
//!   differs from the target have a fixture in `testdata/golden_vectors_current.json`; for the others, the target is
//!   the current fixture. Any change to the output, whether a regression or an accidental fix, fails the test.
//! - **Gap manifest.** Every field difference between the target and the current output must be covered by an entry
//!   in [`manifest`], which names the gap that explains it. An entry that no longer matches a difference in one of its
//!   vectors is stale, and fails the test too.
//!
//! When a change fixes a gap, run the test with `SALUKI_UPDATE_GOLDEN_VECTORS=1` to rewrite the current fixtures, then
//! remove the manifest entries that the test reports as stale. A vector whose output now matches its target loses its
//! current fixture.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use http_body_util::BodyExt as _;
use otlp_protos::opentelemetry::proto::collector::logs::v1::logs_service_client::LogsServiceClient;
use saluki_core::components::test_util::{ComponentControl, NoInput, Output, TestComponentDriver};
use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
use saluki_core::topology::EventsBuffer;
use saluki_io::net::BoundListenAddress;
use serde_json::Value;
use stele::Log;
use tonic::transport::Channel;

use crate::encoders::{BufferedIncrementalConfiguration, DatadogLogsConfiguration};
use crate::sources::otlp::tests::source_configuration;

mod manifest;
use self::manifest::{DiffKind, KnownGap, KNOWN_GAPS};

mod vectors;
use self::vectors::{golden_vectors, GoldenVector};

/// Placeholder for an envelope `timestamp` set to the time the log was encoded, in milliseconds.
const ENCODE_TIME_MS: &str = "<encode-time-ms>";

/// Placeholder for an `@timestamp` that the encoder sets to the time the log was encoded.
const ENCODE_TIME_RFC3339: &str = "<encode-time-rfc3339>";

/// The default hostname, which the target uses when a log has none.
const DEFAULT_HOSTNAME: &str = "default-hostname";

/// Path of the current fixtures, for the vectors whose current output differs from their target.
const CURRENT_FIXTURES_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/sources/otlp/logs/testdata/golden_vectors_current.json"
);

/// Environment variable that makes the test rewrite the current fixtures from the current output.
const UPDATE_ENV_VAR: &str = "SALUKI_UPDATE_GOLDEN_VECTORS";

/// A difference between a target log and a current log.
#[derive(Debug)]
struct FieldDiff {
    /// Location-qualified path of the field, as in the panoramic logs analyzer: `envelope.status`,
    /// `envelope.ddtags`, `envelope_properties.<key>`, or `content.<key>`.
    path: String,
    kind: DiffKind,
    target: Option<Value>,
    current: Option<Value>,
}

/// Runs the OTLP source, with its gRPC server on an ephemeral port.
async fn start_source() -> (
    ComponentControl<NoInput>,
    Output<EventsBuffer>,
    LogsServiceClient<Channel>,
) {
    let mut config = source_configuration();
    config.receiver.logs_enabled = true;
    config.receiver.grpc.endpoint = "127.0.0.1:0".to_string();
    config.receiver.grpc.max_recv_msg_size_mib = 4;
    // The configuration translator resolves a zero keepalive to these defaults.
    config.receiver.grpc.keepalive.time = Duration::from_secs(2 * 60 * 60);
    config.receiver.grpc.keepalive.timeout = Duration::from_secs(20);
    config.receiver.http.endpoint = "127.0.0.1:0".to_string();

    let (control, mut outputs) = TestComponentDriver::options()
        .with_interconnect_capacity(NonZeroUsize::MIN)
        .source(config)
        .await
        .expect("OTLP source should build");
    let logs = outputs.take_named_output("logs");

    let mut bound = control
        .supervisor()
        .dataspace()
        .subscribe::<BoundListenAddress>(IdentifierFilter::exact("http-server-otlp-grpc"));
    let address: SocketAddr = match bound.recv().await {
        Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address))) => address,
        update => panic!("expected the gRPC server to bind, got {:?}", update),
    };
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", address))
        .expect("endpoint should parse")
        .connect()
        .await
        .expect("should connect to the gRPC server");

    (control, logs, LogsServiceClient::new(channel))
}

/// Sends a vector's request to the source, and returns the logs on the wire, normalized.
async fn current_output(
    vector: &GoldenVector, client: &mut LogsServiceClient<Channel>, logs: &mut Output<EventsBuffer>,
) -> Vec<Value> {
    let expected_events = vector
        .request
        .resource_logs
        .iter()
        .flat_map(|resource_logs| &resource_logs.scope_logs)
        .map(|scope_logs| scope_logs.log_records.len())
        .sum::<usize>();

    client
        .export(vector.request.clone())
        .await
        .unwrap_or_else(|e| panic!("{}: export should succeed: {}", vector.name, e));
    let mut events = Vec::new();
    while events.len() < expected_events {
        let buffer = logs.next().await.expect("the source should dispatch logs");
        events.extend(buffer);
    }
    assert_eq!(
        events.len(),
        expected_events,
        "{}: unexpected number of logs",
        vector.name
    );

    // The topology connects the encoder like this, with the same compression settings and flush timeout by default.
    let encoder =
        BufferedIncrementalConfiguration::new(DatadogLogsConfiguration::new("zstd", 3), Duration::from_secs(2));
    let encode_start_ms = unix_millis();
    let (encoder, mut payloads) = TestComponentDriver::encoder(encoder)
        .await
        .expect("logs encoder should build");
    encoder.send_events(events).await;
    encoder.shutdown().await.expect("logs encoder should stop cleanly");
    let encode_end_ms = unix_millis();

    let mut output = Vec::new();
    for payload in payloads.collect().await {
        let (_, request) = payload
            .try_into_http_payload()
            .expect("logs are sent over HTTP")
            .into_parts();
        assert_eq!(request.uri().path(), "/api/v2/logs", "{}: unexpected path", vector.name);
        let content_encoding = request
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .map(|value| value.to_str().expect("content encoding should be ASCII").to_string());
        let body = request
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        let body = match content_encoding.as_deref() {
            Some("zstd") => zstd::stream::decode_all(body.as_ref()).expect("body should decompress"),
            other => panic!("{}: unexpected content encoding {:?}", vector.name, other),
        };

        // Parse the payload as the intake does first, so that a malformed payload fails here.
        Log::try_from_payload(&body).unwrap_or_else(|e| panic!("{}: payload should parse: {}", vector.name, e));
        let items: Vec<Value> = serde_json::from_slice(&body).expect("payload should be a JSON array");
        output.extend(
            items
                .into_iter()
                .map(|item| normalize_encode_times(item, encode_start_ms, encode_end_ms)),
        );
    }
    output
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the epoch")
        .as_millis() as u64
}

/// Replaces the encode-time timestamps of a log with placeholders.
///
/// The envelope `timestamp` and the encoder's `@timestamp` are replaced only when they fall within the time the log was being
/// encoded, so a timestamp taken from the OTLP record stays as it is.
fn normalize_encode_times(mut item: Value, start_ms: u64, end_ms: u64) -> Value {
    let object = item.as_object_mut().expect("a log should be a JSON object");
    let during_encode = |ms: u64| (start_ms..=end_ms).contains(&ms);

    if let Some(timestamp) = object.get_mut("timestamp") {
        if timestamp.as_u64().is_some_and(during_encode) {
            *timestamp = Value::String(ENCODE_TIME_MS.to_string());
        }
    }
    if let Some(timestamp) = object.get_mut("@timestamp") {
        let parsed = timestamp
            .as_str()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok());
        if parsed.is_some_and(|parsed| u64::try_from(parsed.timestamp_millis()).is_ok_and(during_encode)) {
            *timestamp = Value::String(ENCODE_TIME_RFC3339.to_string());
        }
    }
    item
}

/// Returns a fixture log as it would be on the wire: a `message` given as a JSON object becomes its serialization.
fn to_wire(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(message) = item.get_mut("message") {
        if message.is_object() {
            *message = Value::String(message.to_string());
        }
    }
    item
}

/// Returns a log as it is written to a fixture: a `message` holding a JSON object is shown as that object, if it
/// serializes back to the same string.
fn to_fixture(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(message) = item.get_mut("message") {
        let parsed = message.as_str().and_then(|raw| {
            serde_json::from_str::<Value>(raw)
                .ok()
                .filter(|parsed| serde_json::to_string(parsed).is_ok_and(|serialized| serialized == raw))
        });
        if let Some(parsed @ Value::Object(_)) = parsed {
            *message = parsed;
        }
    }
    item
}

/// Parses fixture logs with the intake's parser.
fn parse(items: &[Value]) -> Vec<Log> {
    let wire = Value::Array(items.iter().map(to_wire).collect());
    Log::try_from_payload(wire.to_string().as_bytes()).expect("fixture logs should parse")
}

/// Diffs a target log with a current log, field by field.
///
/// This uses the same paths as the panoramic logs analyzer. When both logs have content, the raw `message` is left
/// out: the target's serialization only stands in for the reference serialization, so only the content properties are compared.
fn diff_logs(target: &Log, current: &Log) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();

    let both_have_content = target.content_properties().is_some() && current.content_properties().is_some();
    for field in ["message", "status", "hostname", "service", "ddsource", "timestamp"] {
        if field == "message" && both_have_content {
            continue;
        }
        diff_optional(
            format!("envelope.{}", field),
            target.envelope_field(field),
            current.envelope_field(field),
            &mut diffs,
        );
    }

    let tag_values = |tags: &BTreeSet<String>| (!tags.is_empty()).then(|| tags.iter().cloned().collect::<Value>());
    if target.ddtags() != current.ddtags() {
        diff_optional(
            "envelope.ddtags".to_string(),
            tag_values(target.ddtags()).as_ref(),
            tag_values(current.ddtags()).as_ref(),
            &mut diffs,
        );
    }

    let empty = BTreeMap::new();
    let target_locations = property_locations(target, &empty);
    let current_locations = property_locations(current, &empty);
    let keys = target_locations
        .iter()
        .chain(&current_locations)
        .flat_map(|(_, properties)| properties.keys())
        .collect::<BTreeSet<_>>();

    for key in keys {
        let found = |locations: &PropertyLocations<'_>| {
            locations
                .iter()
                .filter_map(|(prefix, properties)| properties.get(key).map(|value| (*prefix, value.clone())))
                .collect::<Vec<_>>()
        };
        let in_target = found(&target_locations);
        let in_current = found(&current_locations);

        // A key found in exactly one location on each side, but a different one, has moved.
        if let ([(target_prefix, target_value)], [(current_prefix, current_value)]) =
            (in_target.as_slice(), in_current.as_slice())
        {
            if target_prefix != current_prefix {
                let path = format!("{}.{}", target_prefix, key);
                diffs.push(FieldDiff {
                    path: path.clone(),
                    kind: DiffKind::Location,
                    target: Some(target_value.clone()),
                    current: Some(current_value.clone()),
                });
                if target_value != current_value {
                    diffs.push(FieldDiff {
                        path,
                        kind: DiffKind::Value,
                        target: Some(target_value.clone()),
                        current: Some(current_value.clone()),
                    });
                }
                continue;
            }
        }

        for prefix in ["envelope_properties", "content"] {
            let at = |found: &[(&str, Value)]| {
                found
                    .iter()
                    .find(|(found_prefix, _)| *found_prefix == prefix)
                    .map(|(_, value)| value.clone())
            };
            diff_optional(
                format!("{}.{}", prefix, key),
                at(&in_target).as_ref(),
                at(&in_current).as_ref(),
                &mut diffs,
            );
        }
    }

    diffs
}

type PropertyLocations<'a> = [(&'static str, &'a BTreeMap<String, Value>); 2];

/// Returns the properties of a log, by the path prefix of their location.
fn property_locations<'a>(log: &'a Log, empty: &'a BTreeMap<String, Value>) -> PropertyLocations<'a> {
    [
        ("envelope_properties", log.envelope_properties()),
        ("content", log.content_properties().unwrap_or(empty)),
    ]
}

fn diff_optional(path: String, target: Option<&Value>, current: Option<&Value>, diffs: &mut Vec<FieldDiff>) {
    let kind = match (target, current) {
        (None, None) => return,
        (Some(target), Some(current)) if target == current => return,
        (Some(_), Some(_)) => DiffKind::Value,
        (Some(_), None) => DiffKind::Missing,
        (None, Some(_)) => DiffKind::Extra,
    };
    diffs.push(FieldDiff {
        path,
        kind,
        target: target.cloned(),
        current: current.cloned(),
    });
}

/// Returns `true` if `path` matches the glob `pattern`.
///
/// Paths are matched segment by segment, splitting on `.`. Within a segment, `*` matches any run of characters, and a
/// segment of `**` matches any number of segments, including none. This is the same syntax as the known differences of
/// the panoramic logs analyzer.
fn path_matches(pattern: &str, path: &str) -> bool {
    fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
            Some((segment, rest)) => match path.split_first() {
                Some((path_segment, path_rest)) => {
                    segment_matches(segment.as_bytes(), path_segment.as_bytes()) && segments_match(rest, path_rest)
                }
                None => false,
            },
        }
    }

    fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
            Some((c, rest)) => text.first() == Some(c) && segment_matches(rest, &text[1..]),
        }
    }

    let pattern = pattern.split('.').collect::<Vec<_>>();
    let path = path.split('.').collect::<Vec<_>>();
    segments_match(&pattern, &path)
}

impl KnownGap {
    fn covers(&self, vector: &str, diff: &FieldDiff) -> bool {
        self.vectors.contains(&vector)
            && (self.kinds.is_empty() || self.kinds.contains(&diff.kind))
            && self.paths.iter().any(|pattern| path_matches(pattern, &diff.path))
    }
}

fn read_current_fixtures() -> BTreeMap<String, Vec<Value>> {
    let raw = std::fs::read_to_string(CURRENT_FIXTURES_PATH)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", CURRENT_FIXTURES_PATH, e));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {}", CURRENT_FIXTURES_PATH, e))
}

fn write_current_fixtures(fixtures: &BTreeMap<String, Vec<Value>>) {
    let mut raw = serde_json::to_string_pretty(fixtures).expect("fixtures should serialize");
    raw.push('\n');
    std::fs::write(CURRENT_FIXTURES_PATH, raw)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", CURRENT_FIXTURES_PATH, e));
}

#[tokio::test(flavor = "multi_thread")]
async fn otlp_logs_golden_vectors() {
    let vectors = golden_vectors();
    let names = vectors.iter().map(|vector| vector.name).collect::<BTreeSet<_>>();
    assert_eq!(names.len(), vectors.len(), "vector names should be unique");

    let update = std::env::var_os(UPDATE_ENV_VAR).is_some();
    let fixtures = read_current_fixtures();
    let mut updated_fixtures = BTreeMap::new();
    // Failures that updating the current fixtures resolves, and failures that it doesn't.
    let mut fixture_failures = Vec::new();
    let mut failures = Vec::new();

    for name in fixtures.keys() {
        if !names.contains(name.as_str()) {
            fixture_failures.push(format!("{}: current fixture for an unknown vector", name));
        }
    }
    for gap in KNOWN_GAPS {
        for vector in gap.vectors {
            if !names.contains(vector) {
                failures.push(format!(
                    "{}: manifest entry for {} names an unknown vector",
                    vector, gap.gap
                ));
            }
        }
    }

    // The vectors and the paths for which each manifest entry matched a diff.
    let mut used_vectors = BTreeSet::<(usize, &str)>::new();
    let mut used_paths = BTreeSet::<(usize, &str)>::new();

    let (control, mut logs, mut client) = start_source().await;
    for vector in &vectors {
        let current = current_output(vector, &mut client, &mut logs).await;
        let current_logs = parse(&current);
        let target_logs = parse(&vector.target);

        // The current output must match the current fixture exactly, or the target when there's none.
        let expected = fixtures
            .get(vector.name)
            .map(|fixture| parse(fixture))
            .unwrap_or_else(|| target_logs.clone());
        if current_logs != expected {
            fixture_failures.push(format!(
                "{}: current output changed; run with {}=1 to update the current fixtures\n  output: {}",
                vector.name,
                UPDATE_ENV_VAR,
                Value::Array(current.iter().map(to_fixture).collect())
            ));
        }
        if current_logs != target_logs {
            updated_fixtures.insert(vector.name.to_string(), current.iter().map(to_fixture).collect());
        }

        // Every difference from the target must be a known gap.
        if current_logs.len() != target_logs.len() {
            failures.push(format!(
                "{}: {} logs, but the target has {}",
                vector.name,
                current_logs.len(),
                target_logs.len()
            ));
            continue;
        }
        for (index, (target_log, current_log)) in target_logs.iter().zip(&current_logs).enumerate() {
            for diff in diff_logs(target_log, current_log) {
                let mut covered = false;
                for (entry_index, gap) in KNOWN_GAPS.iter().enumerate() {
                    if !gap.covers(vector.name, &diff) {
                        continue;
                    }
                    covered = true;
                    used_vectors.insert((entry_index, vector.name));
                    for pattern in gap.paths {
                        if path_matches(pattern, &diff.path) {
                            used_paths.insert((entry_index, pattern));
                        }
                    }
                }
                if !covered {
                    failures.push(format!(
                        "{}: log {}: {} differs from the target with no known gap ({:?}; target: {}, current: {})",
                        vector.name,
                        index,
                        diff.path,
                        diff.kind,
                        diff.target
                            .map_or_else(|| "<absent>".to_string(), |value| value.to_string()),
                        diff.current
                            .map_or_else(|| "<absent>".to_string(), |value| value.to_string()),
                    ));
                }
            }
        }
    }
    control.shutdown().await.expect("OTLP source should stop cleanly");

    // Every vector and every path of an entry must still match a diff, so that a fix has to remove them.
    for (entry_index, gap) in KNOWN_GAPS.iter().enumerate() {
        for vector in gap.vectors {
            if !used_vectors.contains(&(entry_index, *vector)) {
                failures.push(format!(
                    "{}: manifest entry for {} ({}) no longer matches; remove the vector from it",
                    vector, gap.gap, gap.reason
                ));
            }
        }
        for pattern in gap.paths {
            if !used_paths.contains(&(entry_index, *pattern)) {
                failures.push(format!(
                    "manifest entry for {} ({}) no longer matches path `{}`; remove the path from it",
                    gap.gap, gap.reason, pattern
                ));
            }
        }
    }

    if update {
        write_current_fixtures(&updated_fixtures);
    } else {
        failures.extend(fixture_failures);
    }
    assert!(
        failures.is_empty(),
        "{} golden vector failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

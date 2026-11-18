//! Tests for the v0.4 and v0.1 JSON decoders.
//!
//! The primary tests are table-driven runs over `testdata/v04_json_cases.json` and `testdata/v01_json_cases.json`. Each case's expected value is what the
//! trace-agent's receiver decodes from the same body: the fixtures were produced by running its
//! `decodeConvertedTracerPayload` (Go 1.26.11, the toolchain the Agent pins) on each body, and dumping the resulting
//! tracer payload or classifying the error as `httpDecodingError` does. The remaining tests cover the syntax pass, the
//! nesting limit, and allocation bounds.

use base64::{engine::general_purpose, Engine as _};
use proptest::prelude::*;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Span};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use stringtheory::MetaString;

use super::scan::{decoder_read, scan, Outcome};
use super::{decode_v01_json, decode_v04_any, decode_v04_json, DecodeError, DecodedV1};
use crate::decoders::datadog::v04::tests::alloc_tracking;

const FIXTURE_CASES_JSON: &str = include_str!("../testdata/v04_json_cases.json");
const V01_FIXTURE_CASES_JSON: &str = include_str!("../testdata/v01_json_cases.json");

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    format: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_b64: Option<String>,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Expected {
    Payload(Value),
    Error { tag: String, reason: String },
}

/// Classifies an error as the receiver's `httpDecodingError` tags it and as it counts the dropped traces.
fn classify(error: &DecodeError) -> (&'static str, &'static str) {
    match error {
        DecodeError::UnexpectedEof => ("unexpected-eof", "unexpected_eof"),
        DecodeError::ShortBytes { .. } => ("decoding-error", "msgp_short_bytes"),
        _ => ("decoding-error", "decoding_error"),
    }
}

/// Renders decoded traces in the fixtures' form.
fn dump(decoded: &DecodedV1) -> Value {
    let payload = decoded
        .traces
        .first()
        .map(|trace| trace.payload.clone())
        .unwrap_or_default();
    let chunks: Vec<Value> = decoded
        .traces
        .iter()
        .map(|trace| {
            json!({
                "priority": trace.priority.unwrap_or(i32::from(i8::MIN)),
                "origin": trace.origin.as_ref(),
                "sampling_mechanism": trace.sampling_mechanism,
                "dropped_trace": trace.dropped_trace,
                "trace_id": format!("{:016x}{:016x}", trace.trace_id_high, trace.trace_id_low),
                "attributes": dump_attributes(&trace.attributes),
                "spans": trace.spans().iter().map(dump_span).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "env": payload.env.as_ref(),
        "hostname": payload.hostname.as_ref(),
        "app_version": payload.app_version.as_ref(),
        "language_name": payload.language_name.as_ref(),
        "language_version": payload.language_version.as_ref(),
        "tracer_version": payload.tracer_version.as_ref(),
        "container_id": payload.container_id.as_ref(),
        "runtime_id": payload.runtime_id.as_ref(),
        "attributes": dump_attributes(&decoded.payload_attributes),
        "chunks": chunks,
    })
}

fn dump_span(span: &Span) -> Value {
    json!({
        "service": span.service(),
        "name": span.name(),
        "resource": span.resource(),
        "type": span.span_type(),
        "span_id": span.span_id(),
        "parent_id": span.parent_id(),
        "start": span.start(),
        "duration": span.duration(),
        "error": span.error() != 0,
        "env": span.env.as_ref(),
        "version": span.version.as_ref(),
        "component": span.component.as_ref(),
        "kind": span.kind,
        "attributes": dump_attributes(&span.attributes),
        "links": span.span_links().iter().map(|link| json!({
            "trace_id": format!("{:016x}{:016x}", link.trace_id_high(), link.trace_id()),
            "span_id": link.span_id(),
            "flags": link.flags(),
            "tracestate": link.tracestate(),
            "attributes": dump_attributes(link.attributes()),
        })).collect::<Vec<_>>(),
        "events": span.span_events().iter().map(|event| json!({
            "time": event.time_unix_nano(),
            "name": event.name(),
            "attributes": dump_attributes(event.attributes()),
        })).collect::<Vec<_>>(),
    })
}

fn dump_attributes(attributes: &FastHashMap<MetaString, AttributeValue>) -> Value {
    Value::Object(
        attributes
            .iter()
            .map(|(key, value)| (key.to_string(), dump_value(value)))
            .collect(),
    )
}

fn dump_value(value: &AttributeValue) -> Value {
    match value {
        AttributeValue::String(s) => json!({ "string": s.as_ref() }),
        AttributeValue::Bool(b) => json!({ "bool": b }),
        AttributeValue::Int(i) => json!({ "int": i }),
        AttributeValue::Float(f) => json!({ "double": f }),
        AttributeValue::Bytes(b) => json!({ "bytes": general_purpose::STANDARD.encode(b) }),
        AttributeValue::Array(values) => json!({ "array": values.iter().map(dump_value).collect::<Vec<_>>() }),
        AttributeValue::KeyValueList(_) => json!({ "unknown": "key value list" }),
    }
}

/// Turns the fixtures' doubles, which are Go's text for them, into numbers.
fn parse_doubles(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let (1, Some(Value::String(text))) = (map.len(), map.get("double")) {
                let parsed: f64 = text.parse().expect("a fixture double parses");
                map.insert("double".to_string(), json!(parsed));
                return;
            }
            map.values_mut().for_each(parse_doubles);
        }
        Value::Array(values) => values.iter_mut().for_each(parse_doubles),
        _ => {}
    }
}

fn case_body(case: &Case) -> Vec<u8> {
    match (&case.body, &case.body_b64) {
        (Some(body), None) => body.as_bytes().to_vec(),
        (None, Some(body)) => general_purpose::STANDARD.decode(body).unwrap(),
        _ => panic!("{}: exactly one of body and body_b64", case.name),
    }
}

/// Checks that each fixture case decodes, with `decode`, as the reference decoded it. With `any_chunk_order`, the chunks
/// are compared in any order, as the reference's own order is random.
fn check_fixture(fixture: &str, decode: impl Fn(&str, &[u8]) -> Result<DecodedV1, DecodeError>, any_chunk_order: bool) {
    let fixture: Fixture = serde_json::from_str(fixture).unwrap();
    assert!(fixture.cases.len() > 30);
    for case in &fixture.cases {
        let _ = &case.description;
        let body = case_body(case);
        match (&case.expected, decode(&case.format, &body)) {
            (Expected::Payload(expected), Ok(decoded)) => {
                let mut expected = expected.clone();
                parse_doubles(&mut expected);
                let mut actual = dump(&decoded);
                if any_chunk_order {
                    sort_chunks(&mut expected);
                    sort_chunks(&mut actual);
                }
                assert_eq!(
                    actual,
                    expected,
                    "{}:\nactual:   {}\nexpected: {}",
                    case.name,
                    serde_json::to_string(&actual).unwrap(),
                    serde_json::to_string(&expected).unwrap()
                );
            }
            (Expected::Payload(_), Err(error)) => panic!("{}: expected a payload, got {error:?}", case.name),
            (Expected::Error { tag, reason }, Ok(decoded)) => {
                panic!("{}: expected {tag}/{reason}, got {}", case.name, dump(&decoded))
            }
            (Expected::Error { tag, reason }, Err(error)) => {
                assert_eq!(
                    classify(&error),
                    (tag.as_str(), reason.as_str()),
                    "{}: {error}",
                    case.name
                );
            }
        }
    }
}

/// Sorts a dumped payload's chunks by their rendering.
fn sort_chunks(payload: &mut Value) {
    if let Some(Value::Array(chunks)) = payload.get_mut("chunks") {
        chunks.sort_by_cached_key(|chunk| chunk.to_string());
    }
}

#[test]
fn fixture_cases_decode_as_the_reference_decodes_them() {
    let decode = |format: &str, body: &[u8]| match format {
        "json" => decode_v04_json(body),
        "any" => decode_v04_any(body),
        format => panic!("unknown format {format}"),
    };
    check_fixture(FIXTURE_CASES_JSON, decode, false);
}

#[test]
fn v01_fixture_cases_decode_as_the_reference_decodes_them() {
    // The reference decodes a v0.1 body as JSON whatever its media type.
    check_fixture(V01_FIXTURE_CASES_JSON, |_, body| decode_v01_json(body), true);
}

#[test]
fn v01_traces_come_in_the_order_their_ids_first_appear() {
    let body = br#"[{"trace_id":3,"span_id":1},null,{"trace_id":1,"span_id":2},{"trace_id":3,"span_id":3},{"trace_id":2,"span_id":4},{"trace_id":1,"span_id":5}]"#;
    let decoded = decode_v01_json(body).unwrap();
    let traces: Vec<(u64, Vec<u64>)> = decoded
        .traces
        .iter()
        .map(|trace| (trace.trace_id_low, trace.spans().iter().map(Span::span_id).collect()))
        .collect();
    assert_eq!(traces, [(3, vec![1, 3]), (1, vec![2, 5]), (2, vec![4])]);
}

#[test]
fn v01_payloads_of_distinct_trace_ids_allocate_in_proportion_to_their_size() {
    // Each span is its own trace, so grouping makes one chunk and one map entry per span.
    let count = 20_000;
    let body = format!(
        "[{}]",
        (0..count)
            .map(|id| format!(r#"{{"trace_id":{id}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    let (decoded, peak) = alloc_tracking::peak_during(|| decode_v01_json(body.as_bytes()).unwrap());
    assert_eq!(decoded.traces.len(), count);
    // A classic span, the chunk holding it and its map entry, then a unified span and the trace holding it.
    let per_span = std::mem::size_of::<datadog_protos::traces::Span>()
        + std::mem::size_of::<datadog_protos::traces::TraceChunk>()
        + std::mem::size_of::<Span>()
        + std::mem::size_of::<saluki_core::data_model::event::trace::Trace>()
        + 128;
    let bound = 2 * count * per_span;
    assert!(peak <= bound, "peak {peak} bytes (bound {bound})");
}

#[test]
fn the_scan_stops_where_go_s_decoder_stops() {
    let cases: &[(&[u8], Outcome, usize)] = &[
        (b"", Outcome::Eof, 0),
        (b"  ", Outcome::Eof, 2),
        (b" [] x", Outcome::Value { len: 3 }, 3),
        (b"[[{}]]", Outcome::Value { len: 6 }, 6),
        // A value that isn't an array or object is only complete at the byte after it.
        (b"123 ", Outcome::Value { len: 3 }, 4),
        (b"123", Outcome::Value { len: 3 }, 3),
        (b"nullx", Outcome::Value { len: 4 }, 5),
        (b"\"a\"b", Outcome::Value { len: 3 }, 4),
        (b"-", Outcome::Eof, 1),
        (b"1.", Outcome::Eof, 2),
        (b"1e+", Outcome::Eof, 3),
        (b"[1,", Outcome::Eof, 3),
        (b"[1 2]", Outcome::Syntax { at: 3 }, 4),
        (b"[01]", Outcome::Syntax { at: 2 }, 3),
        (b"{\"a\" 1}", Outcome::Syntax { at: 5 }, 6),
        (b"{\"a\":1,}", Outcome::Syntax { at: 7 }, 8),
        (b"[\"\\q\"]", Outcome::Syntax { at: 3 }, 4),
        (b"[\"\\u12x4\"]", Outcome::Syntax { at: 6 }, 7),
        (b"[\"\x1f\"]", Outcome::Syntax { at: 2 }, 3),
        (b"[\"\xff\"]", Outcome::Value { len: 5 }, 5),
        (b"tru", Outcome::Eof, 3),
        (b"trux", Outcome::Syntax { at: 3 }, 4),
    ];
    for (body, outcome, examined) in cases {
        let scanned = scan(body);
        assert_eq!(&scanned.outcome, outcome, "{:?}", String::from_utf8_lossy(body));
        assert_eq!(scanned.examined, *examined, "{:?}", String::from_utf8_lossy(body));
    }
}

#[test]
fn the_decoder_reads_in_growing_chunks() {
    assert_eq!(decoder_read(0, 0), 0);
    assert_eq!(decoder_read(1, 100), 100);
    assert_eq!(decoder_read(1, 10_000), 512);
    assert_eq!(decoder_read(512, 10_000), 512);
    assert_eq!(decoder_read(513, 10_000), 1536);
    assert_eq!(decoder_read(1537, 10_000), 3584);
    assert_eq!(decoder_read(3585, 10_000), 7680);
    assert_eq!(decoder_read(9000, 10_000), 10_000);
}

/// `[[{"x": [[...]]}]]`, with arrays and objects nested `depth` deep in all.
fn nested(depth: usize) -> Vec<u8> {
    let inner = depth - 3;
    let mut body = b"[[{\"x\":".to_vec();
    body.extend(std::iter::repeat_n(b'[', inner));
    body.extend(std::iter::repeat_n(b']', inner));
    body.extend_from_slice(b"}]]");
    body
}

#[test]
fn nesting_is_limited_as_go_s_scanner_limits_it() {
    // Checked against the reference: 10,000 levels decode, and 10,001 are a syntax error.
    let decoded = decode_v04_json(&nested(10_000)).unwrap();
    assert_eq!(decoded.traces.len(), 1);
    assert!(matches!(
        decode_v04_json(&nested(10_001)),
        Err(DecodeError::Json { .. })
    ));
    // Deep nesting after the first value isn't scanned.
    let mut body = b"[]".to_vec();
    body.extend(nested(20_000));
    assert!(decode_v04_json(&body).unwrap().traces.is_empty());
}

#[test]
fn null_heavy_payloads_allocate_in_proportion_to_their_size() {
    // About 600 KB of `null` spans, links, events and array values, which decode to one span with nothing in it.
    let nulls = |count: usize| vec!["null"; count].join(",");
    let body = format!(
        r#"[[{nulls},{{"span_links":[{nulls}],"span_events":[{{"attributes":{{"a":{{"type":4,"array_value":{{"values":[{nulls}]}}}}}}}},{nulls}]}}],{nulls}]"#,
        nulls = nulls(30_000)
    );
    let (decoded, peak) = alloc_tracking::peak_during(|| decode_v04_json(body.as_bytes()).unwrap());
    assert_eq!(decoded.traces.len(), 1);
    // A `null` is kept as one pointer until its list is done, as Go keeps it, and a list's buffer at most doubles.
    let bound = 4 * body.len();
    assert!(
        peak <= bound,
        "peak {peak} bytes for a {}-byte payload (bound {bound})",
        body.len()
    );
}

#[test]
fn empty_span_payloads_allocate_in_proportion_to_their_size() {
    let body = format!("[[{}]]", vec!["{}"; 20_000].join(","));
    let (decoded, peak) = alloc_tracking::peak_during(|| decode_v04_json(body.as_bytes()).unwrap());
    assert_eq!(decoded.traces[0].spans().len(), 20_000);
    // Each 3-byte span is a classic span, then a unified one: the cost of the data model, as in the reference.
    let per_span = std::mem::size_of::<datadog_protos::traces::Span>() + std::mem::size_of::<Span>() + 64;
    let bound = 2 * 20_000 * per_span;
    assert!(peak <= bound, "peak {peak} bytes (bound {bound})");
}

/// Arbitrary JSON values, including the field names the decoder looks for.
fn json_strategy() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("service".to_string()),
        Just("trace_id".to_string()),
        Just("span_id".to_string()),
        Just("start".to_string()),
        Just("error".to_string()),
        Just("meta".to_string()),
        Just("metrics".to_string()),
        Just("meta_struct".to_string()),
        Just("span_links".to_string()),
        Just("span_events".to_string()),
        Just("attributes".to_string()),
        Just("type".to_string()),
        Just("array_value".to_string()),
        Just("values".to_string()),
        Just("_dd.p.dm".to_string()),
        Just("_dd.p.tid".to_string()),
        "[a-z_.]{0,8}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        "[ -~]{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(6, 64, 8, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((key.clone(), inner), 0..6)
                .prop_map(|entries| Value::Object(entries.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

proptest! {
    /// Arbitrary bytes decode or fail, without panicking.
    #[test]
    fn property_test_arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = decode_v04_json(&bytes);
        let _ = decode_v04_any(&bytes);
    }

    /// Arbitrary JSON, shaped like traces or not, and truncated or mutated, decodes or fails without panicking, and
    /// within a loose allocation ceiling.
    #[test]
    fn property_test_arbitrary_json_never_panics(
        traces in prop::collection::vec(prop::collection::vec(json_strategy(), 0..4), 0..4),
        truncate_at in any::<prop::sample::Index>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..3),
    ) {
        let mut body = serde_json::to_vec(&traces).unwrap();
        // Complete JSON never ends early, whatever its shape.
        prop_assert!(!matches!(decode_v04_json(&body), Err(DecodeError::UnexpectedEof)));
        for (index, byte) in &flips {
            let i = index.index(body.len());
            body[i] = *byte;
        }
        body.truncate(truncate_at.index(body.len() + 1));
        let (_, peak) = alloc_tracking::peak_during(|| decode_v04_json(&body));
        let bound = 1024 * 1024 + 2048 * body.len();
        prop_assert!(peak <= bound, "peak {} bytes for {} bytes (bound {})", peak, body.len(), bound);
        let _ = decode_v04_any(&body);
    }
}

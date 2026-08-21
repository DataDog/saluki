//! Tests for the v1.0 APM trace decoder.
//!
//! The primary test is a table-driven run over `testdata/v1_decoder_cases.json`, which pairs
//! base64-encoded payloads with the values they must decode to. Every payload in that file was
//! produced by the trace agent's reference encoder (`pkg/proto/pbgo/trace/idx`) using its inline
//! string-streaming `MarshalMsg`, so the fixtures exercise the encoding real tracers emit rather
//! than our reading of the format. The remaining tests exercise individual primitives and error
//! paths with small hand-built MessagePack inputs.

use std::collections::BTreeMap;

use base64::{engine::general_purpose, Engine as _};
use proptest::prelude::*;
use rmp::encode;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::AttributeValue;
use serde::Deserialize;
use stringtheory::MetaString;

use super::error::DecodeError;
use super::string_table::StringTable;
use super::{decode_span, decode_v1_payload, read, split_trace_id, value};

fn ms(s: &str) -> MetaString {
    MetaString::from(s)
}

/// Fixture cases, each pairing a base64-encoded payload with the value it must decode to.
const FIXTURE_CASES_JSON: &str = include_str!("testdata/v1_decoder_cases.json");

#[derive(Deserialize)]
struct FixtureDoc {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    description: String,
    payload_base64: String,
    expected: ExpectedPayload,
}

/// Expected tracer payload. Every field defaults, mirroring the encoder's habit of omitting fields
/// that hold their zero value; `deny_unknown_fields` keeps a misspelled fixture key from silently
/// defaulting into a vacuous assertion.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpectedPayload {
    container_id: String,
    language_name: String,
    language_version: String,
    tracer_version: String,
    runtime_id: String,
    env: String,
    hostname: String,
    app_version: String,
    attributes: BTreeMap<String, AnyJson>,
    chunks: Vec<ExpectedChunk>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpectedChunk {
    priority: Option<i32>,
    origin: String,
    dropped_trace: bool,
    sampling_mechanism: u32,
    trace_id_hex: String,
    attributes: BTreeMap<String, AnyJson>,
    spans: Vec<ExpectedSpan>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpectedSpan {
    service: String,
    name: String,
    resource: String,
    span_id: u64,
    parent_id: u64,
    start: u64,
    duration: u64,
    error: bool,
    #[serde(rename = "type")]
    span_type: String,
    env: String,
    version: String,
    component: String,
    /// Absent means `SPAN_KIND_UNSPECIFIED`, which `String::default` cannot express.
    kind: Option<String>,
    attributes: BTreeMap<String, AnyJson>,
    links: Vec<ExpectedLink>,
    events: Vec<ExpectedEvent>,
}

impl ExpectedSpan {
    /// The fixture's span kind name, defaulting to unspecified when the field is absent.
    fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or("SPAN_KIND_UNSPECIFIED")
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpectedLink {
    trace_id_hex: String,
    span_id: u64,
    tracestate: String,
    flags: u32,
    attributes: BTreeMap<String, AnyJson>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpectedEvent {
    time: u64,
    name: String,
    attributes: BTreeMap<String, AnyJson>,
}

/// One `AnyValue`, tagged by its `type` field. Variant fields are required, so a typo in a fixture
/// value is a parse error rather than a silent default.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnyJson {
    String { string: String },
    Bool { bool: bool },
    Double { double: f64 },
    Int { int: i64 },
    Bytes { bytes_base64: String },
    Array { array: Vec<AnyJson> },
    KeyValueList { key_values: Vec<KvJson> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KvJson {
    key: String,
    value: AnyJson,
}

impl From<&AnyJson> for AttributeValue {
    fn from(value: &AnyJson) -> Self {
        match value {
            AnyJson::String { string } => Self::String(ms(string)),
            AnyJson::Bool { bool } => Self::Bool(*bool),
            AnyJson::Double { double } => Self::Float(*double),
            AnyJson::Int { int } => Self::Int(*int),
            AnyJson::Bytes { bytes_base64 } => Self::Bytes(
                general_purpose::STANDARD
                    .decode(bytes_base64)
                    .expect("fixture bytes_base64 should be valid base64"),
            ),
            AnyJson::Array { array } => Self::Array(array.iter().map(Self::from).collect()),
            AnyJson::KeyValueList { key_values } => Self::KeyValueList(
                key_values
                    .iter()
                    .map(|kv| (ms(&kv.key), Self::from(&kv.value)))
                    .collect(),
            ),
        }
    }
}

fn expected_attributes(attributes: &BTreeMap<String, AnyJson>) -> FastHashMap<MetaString, AttributeValue> {
    attributes.iter().map(|(k, v)| (ms(k), v.into())).collect()
}

/// Decodes a fixture's 32-hex-char `trace_id_hex` into the same (high, low) halves the decoder
/// produces, empty string mapping to (0, 0).
fn trace_id_halves(hex: &str) -> (u64, u64) {
    if hex.is_empty() {
        return (0, 0);
    }
    assert_eq!(hex.len(), 32, "fixture trace_id_hex should be 32 hex chars: {hex}");

    let mut bytes = [0u8; 16];
    faster_hex::hex_decode(hex.as_bytes(), &mut bytes).expect("valid hex trace ID fixture");
    split_trace_id(&bytes)
}

/// Maps a fixture's `SPAN_KIND_*` name to its numeric value, per the `idx` proto's `SpanKind` enum.
fn kind_value(name: &str) -> u32 {
    match name {
        "SPAN_KIND_UNSPECIFIED" => 0,
        "SPAN_KIND_INTERNAL" => 1,
        "SPAN_KIND_SERVER" => 2,
        "SPAN_KIND_CLIENT" => 3,
        "SPAN_KIND_PRODUCER" => 4,
        "SPAN_KIND_CONSUMER" => 5,
        other => panic!("fixture uses unknown span kind {other}"),
    }
}

/// Asserts `actual == expected`, labelling the failure with the fixture location and the accessor
/// that disagreed.
macro_rules! check {
    ($ctx:expr, $actual:expr, $expected:expr) => {
        assert_eq!($actual, $expected, "{}: {}", $ctx, stringify!($actual))
    };
}

/// Checks a list of same-named string fields, comparing `payload.<field>` against
/// `expected.<field>` for each.
macro_rules! check_payload_strings {
    ($ctx:expr, $payload:expr, $expected:expr, [$($field:ident),+ $(,)?]) => {
        $(assert_eq!($payload.$field.as_ref(), $expected.$field, "{}: {}", $ctx, stringify!($field));)+
    };
}

#[test]
fn fixture_cases_decode_as_expected() {
    let doc: FixtureDoc = serde_json::from_str(FIXTURE_CASES_JSON).expect("fixture JSON should parse");
    assert!(!doc.cases.is_empty(), "fixture file should define at least one case");

    for case in &doc.cases {
        check_case(case);
    }
}

fn check_case(case: &Case) {
    let Case {
        name,
        description,
        payload_base64,
        expected,
    } = case;

    let bytes = general_purpose::STANDARD
        .decode(payload_base64)
        .unwrap_or_else(|e| panic!("case {name}: payload_base64 is not valid base64: {e}"));
    let traces =
        decode_v1_payload(&bytes).unwrap_or_else(|e| panic!("case {name} ({description}): failed to decode: {e}"));

    check!(name, traces.len(), expected.chunks.len());

    // Payload-level metadata is cloned onto every trace, so checking the first one covers it.
    if let Some(trace) = traces.first() {
        check_payload_strings!(
            name,
            trace.payload,
            expected,
            [
                container_id,
                language_name,
                language_version,
                tracer_version,
                runtime_id,
                env,
                hostname,
                app_version,
            ]
        );
    }

    let payload_attributes = expected_attributes(&expected.attributes);

    for (index, (trace, chunk)) in traces.iter().zip(expected.chunks.iter()).enumerate() {
        let ctx = format!("case {name}, chunk {index}");

        check!(ctx, trace.origin.as_ref(), chunk.origin);
        check!(ctx, trace.priority, chunk.priority);
        check!(ctx, trace.dropped_trace, chunk.dropped_trace);
        check!(ctx, trace.sampling_mechanism, chunk.sampling_mechanism);

        let (want_high, want_low) = trace_id_halves(&chunk.trace_id_hex);
        check!(ctx, trace.trace_id_high, want_high);
        check!(ctx, trace.trace_id_low, want_low);

        // Payload-level attributes form the base; chunk-level attributes override on key conflict.
        let mut want_attributes = payload_attributes.clone();
        want_attributes.extend(expected_attributes(&chunk.attributes));
        check!(ctx, *trace.attributes, want_attributes);

        check!(ctx, trace.spans().len(), chunk.spans.len());
        for (index, (span, want)) in trace.spans().iter().zip(chunk.spans.iter()).enumerate() {
            let ctx = format!("{ctx}, span {index}");

            check!(ctx, span.service(), want.service);
            check!(ctx, span.name(), want.name);
            check!(ctx, span.resource(), want.resource);
            check!(ctx, span.span_id(), want.span_id);
            check!(ctx, span.parent_id(), want.parent_id);
            check!(ctx, span.start(), want.start);
            check!(ctx, span.duration(), want.duration);
            check!(ctx, span.error(), i32::from(want.error));
            check!(ctx, span.span_type(), want.span_type);
            check!(ctx, span.env.as_ref(), want.env);
            check!(ctx, span.version.as_ref(), want.version);
            check!(ctx, span.component.as_ref(), want.component);
            check!(ctx, span.kind, kind_value(want.kind()));
            check!(ctx, span.attributes, expected_attributes(&want.attributes));

            check!(ctx, span.span_links().len(), want.links.len());
            for (index, (link, want)) in span.span_links().iter().zip(want.links.iter()).enumerate() {
                let ctx = format!("{ctx}, link {index}");

                let (want_high, want_low) = trace_id_halves(&want.trace_id_hex);
                check!(ctx, link.trace_id_high(), want_high);
                check!(ctx, link.trace_id(), want_low);
                check!(ctx, link.span_id(), want.span_id);
                check!(ctx, link.tracestate(), want.tracestate);
                check!(ctx, link.flags(), want.flags);
                check!(ctx, *link.attributes(), expected_attributes(&want.attributes));
            }

            check!(ctx, span.span_events().len(), want.events.len());
            for (index, (event, want)) in span.span_events().iter().zip(want.events.iter()).enumerate() {
                let ctx = format!("{ctx}, event {index}");

                check!(ctx, event.time_unix_nano(), want.time);
                check!(ctx, event.name(), want.name);
                check!(ctx, *event.attributes(), expected_attributes(&want.attributes));
            }
        }
    }
}

#[test]
fn streaming_string_literal_then_reference() {
    // A literal string is added to the table; a later uint32 index resolves to the same value.
    let mut strings = StringTable::new();

    let mut buf = Vec::new();
    encode::write_str(&mut buf, "abc").unwrap();
    let mut r = buf.as_slice();
    let v = value::read_streaming_string(&mut r, &mut strings, "test").unwrap();
    assert_eq!(v.as_ref(), "abc");
    assert_eq!(strings.len(), 2, "literal should have been added to the table");

    let mut buf = Vec::new();
    encode::write_uint(&mut buf, 1).unwrap();
    let mut r = buf.as_slice();
    let v = value::read_streaming_string(&mut r, &mut strings, "test").unwrap();
    assert_eq!(
        v.as_ref(),
        "abc",
        "index 1 should resolve to the previously added string"
    );
}

#[test]
fn streaming_string_unseen_index_rejected() {
    let mut strings = StringTable::new();
    let mut buf = Vec::new();
    encode::write_uint(&mut buf, 5).unwrap();
    let mut r = buf.as_slice();
    let err = value::read_streaming_string(&mut r, &mut strings, "test").unwrap_err();
    assert!(matches!(err, DecodeError::UnseenStringIndex { index: 5, .. }));
}

#[test]
fn attributes_array_not_multiple_of_three_rejected() {
    let mut strings = StringTable::new();
    let mut buf = Vec::new();
    encode::write_array_len(&mut buf, 2).unwrap();
    encode::write_nil(&mut buf).unwrap();
    encode::write_nil(&mut buf).unwrap();
    let mut r = buf.as_slice();
    let err = value::read_attributes_map(&mut r, &mut strings, "test").unwrap_err();
    assert!(matches!(err, DecodeError::InvalidAttributeArrayLen { len: 2 }));
}

#[test]
fn any_value_array_not_multiple_of_two_rejected() {
    let mut strings = StringTable::new();
    let mut buf = Vec::new();
    encode::write_uint(&mut buf, 6).unwrap(); // array type
    encode::write_array_len(&mut buf, 3).unwrap();
    encode::write_nil(&mut buf).unwrap();
    encode::write_nil(&mut buf).unwrap();
    encode::write_nil(&mut buf).unwrap();
    let mut r = buf.as_slice();
    let err = value::read_any_value(&mut r, &mut strings).unwrap_err();
    assert!(matches!(err, DecodeError::InvalidArrayValueLen { len: 3 }));
}

#[test]
fn any_value_unknown_type_rejected() {
    let mut strings = StringTable::new();
    let mut buf = Vec::new();
    encode::write_uint(&mut buf, 99).unwrap();
    let mut r = buf.as_slice();
    let err = value::read_any_value(&mut r, &mut strings).unwrap_err();
    assert!(matches!(err, DecodeError::UnknownAnyValueType { value_type: 99 }));
}

#[test]
fn unknown_field_number_is_skipped() {
    // A span map with an unknown field (99) between known fields must still decode, proving the
    // unknown value is skipped and the stream stays aligned.
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 3).unwrap();
    encode::write_uint(&mut buf, 1).unwrap(); // service
    encode::write_str(&mut buf, "svc").unwrap();
    encode::write_uint(&mut buf, 99).unwrap(); // unknown field
    encode::write_uint(&mut buf, 12345).unwrap(); // ...its value, to be skipped
    encode::write_uint(&mut buf, 2).unwrap(); // name
    encode::write_str(&mut buf, "op").unwrap();

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let span = decode_span(&mut r, &mut strings).unwrap();
    assert_eq!(span.service(), "svc");
    assert_eq!(span.name(), "op");
}

#[test]
fn unknown_field_inline_string_is_harvested() {
    // An unknown field carrying an inline string still consumed a string-table slot on the encoding
    // side, so the decoder must add it. Here the unknown field's string becomes index 1, and the
    // following known field references it by index.
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 2).unwrap();
    encode::write_uint(&mut buf, 99).unwrap(); // unknown field...
    encode::write_str(&mut buf, "harvested").unwrap(); // ...carrying a new string -> index 1
    encode::write_uint(&mut buf, 1).unwrap(); // service
    encode::write_uint(&mut buf, 1).unwrap(); // ...by reference to index 1

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let span = decode_span(&mut r, &mut strings).unwrap();
    assert_eq!(
        span.service(),
        "harvested",
        "index 1 must resolve to the string carried by the unknown field"
    );
    assert_eq!(strings.len(), 2);
}

#[test]
fn unknown_field_nested_strings_are_harvested() {
    // Strings nested inside an unknown field's arrays and maps count too, in stream order, and map
    // keys are harvested as well as map values.
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 3).unwrap();
    encode::write_uint(&mut buf, 99).unwrap(); // unknown field...
    encode::write_array_len(&mut buf, 3).unwrap(); // ...whose value is an array
    encode::write_str(&mut buf, "one").unwrap(); // -> index 1
    encode::write_map_len(&mut buf, 1).unwrap();
    encode::write_str(&mut buf, "two").unwrap(); // key   -> index 2
    encode::write_str(&mut buf, "three").unwrap(); // value -> index 3
    encode::write_uint(&mut buf, 7).unwrap(); // a scalar, carrying no string
    encode::write_uint(&mut buf, 1).unwrap(); // service
    encode::write_uint(&mut buf, 2).unwrap(); // ...-> "two"
    encode::write_uint(&mut buf, 2).unwrap(); // name
    encode::write_uint(&mut buf, 3).unwrap(); // ...-> "three"

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let span = decode_span(&mut r, &mut strings).unwrap();
    assert_eq!(strings.len(), 4, "three nested strings should have been harvested");
    assert_eq!(strings.get(1).unwrap().as_ref(), "one");
    assert_eq!(span.service(), "two");
    assert_eq!(span.name(), "three");
}

#[test]
fn unknown_field_depth_cap_enforced() {
    // Nesting an unknown field's value past the depth cap must error rather than overflow the stack.
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 1).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    for _ in 0..205 {
        encode::write_array_len(&mut buf, 1).unwrap();
    }
    encode::write_uint(&mut buf, 0).unwrap();

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let err = decode_span(&mut r, &mut strings).unwrap_err();
    assert!(matches!(err, DecodeError::DepthExceeded { .. }));
}

#[test]
fn unknown_fields_at_every_level_are_harvested() {
    // End-to-end: an unknown field at the payload, chunk, span, span link, and span event levels,
    // each carrying a new inline string, with every known string referenced by index afterwards. A
    // decoder that skipped these fields without harvesting would misresolve every reference.
    let mut buf = Vec::new();

    // Tracer payload: unknown field 99 -> "svc", then env (field 7) by reference, then chunks.
    encode::write_map_len(&mut buf, 3).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    encode::write_str(&mut buf, "svc").unwrap(); // -> index 1
    encode::write_uint(&mut buf, 7).unwrap(); // env
    encode::write_uint(&mut buf, 1).unwrap(); // -> "svc"
    encode::write_uint(&mut buf, 11).unwrap(); // chunks
    encode::write_array_len(&mut buf, 1).unwrap();

    // Trace chunk: unknown field 99 -> "origin-x", then origin (field 2) by reference, then spans.
    encode::write_map_len(&mut buf, 3).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    encode::write_str(&mut buf, "origin-x").unwrap(); // -> index 2
    encode::write_uint(&mut buf, 2).unwrap(); // origin
    encode::write_uint(&mut buf, 2).unwrap(); // -> "origin-x"
    encode::write_uint(&mut buf, 4).unwrap(); // spans
    encode::write_array_len(&mut buf, 1).unwrap();

    // Span: unknown field 99 -> "op", then name (field 2) by reference, then a link and an event.
    encode::write_map_len(&mut buf, 4).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    encode::write_str(&mut buf, "op").unwrap(); // -> index 3
    encode::write_uint(&mut buf, 2).unwrap(); // name
    encode::write_uint(&mut buf, 3).unwrap(); // -> "op"

    // Span link: unknown field 99 -> "ts=9", then tracestate (field 4) by reference.
    encode::write_uint(&mut buf, 11).unwrap(); // links
    encode::write_array_len(&mut buf, 1).unwrap();
    encode::write_map_len(&mut buf, 2).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    encode::write_str(&mut buf, "ts=9").unwrap(); // -> index 4
    encode::write_uint(&mut buf, 4).unwrap(); // tracestate
    encode::write_uint(&mut buf, 4).unwrap(); // -> "ts=9"

    // Span event: unknown field 99 -> "evt", then name (field 2) by reference.
    encode::write_uint(&mut buf, 12).unwrap(); // events
    encode::write_array_len(&mut buf, 1).unwrap();
    encode::write_map_len(&mut buf, 2).unwrap();
    encode::write_uint(&mut buf, 99).unwrap();
    encode::write_str(&mut buf, "evt").unwrap(); // -> index 5
    encode::write_uint(&mut buf, 2).unwrap(); // name
    encode::write_uint(&mut buf, 5).unwrap(); // -> "evt"

    let traces = decode_v1_payload(&buf).expect("payload with unknown fields should decode");
    assert_eq!(traces.len(), 1);
    let t = &traces[0];
    assert_eq!(t.payload.env.as_ref(), "svc");
    assert_eq!(t.origin.as_ref(), "origin-x");
    assert_eq!(t.spans().len(), 1);
    let s = &t.spans()[0];
    assert_eq!(s.name(), "op");
    assert_eq!(s.span_links()[0].tracestate(), "ts=9");
    assert_eq!(s.span_events()[0].name(), "evt");
}

#[test]
fn any_value_depth_cap_enforced() {
    // Nest arrays past the depth cap: each level is `[type=6, arraylen=2, <inner element>]`.
    let mut buf = Vec::new();
    for _ in 0..205 {
        encode::write_uint(&mut buf, 6).unwrap();
        encode::write_array_len(&mut buf, 2).unwrap();
    }
    encode::write_uint(&mut buf, 4).unwrap(); // innermost: int
    encode::write_sint(&mut buf, 0).unwrap();

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let err = value::read_any_value(&mut r, &mut strings).unwrap_err();
    assert!(matches!(err, DecodeError::DepthExceeded { .. }));
}

#[test]
fn oversize_array_header_rejected() {
    let mut buf = Vec::new();
    encode::write_array_len(&mut buf, read::MAX_SIZE + 1).unwrap();
    let mut r = buf.as_slice();
    let err = read::read_array_len(&mut r, "test").unwrap_err();
    assert!(matches!(err, DecodeError::OversizeHeader { .. }));
}

#[test]
fn implausible_array_count_rejected() {
    // Under MAX_SIZE, but a tiny payload can't possibly back 1000 elements.
    let mut buf = Vec::new();
    encode::write_array_len(&mut buf, 1000).unwrap();
    encode::write_uint(&mut buf, 0).unwrap(); // far fewer bytes than 1000 elements require
    let mut r = buf.as_slice();
    let err = read::read_array_len(&mut r, "test").unwrap_err();
    assert!(matches!(err, DecodeError::ImplausibleHeaderCount { len: 1000, .. }));
}

#[test]
fn implausible_map_count_rejected() {
    // Under MAX_SIZE, but a tiny payload can't possibly back 1000 entries.
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 1000).unwrap();
    encode::write_uint(&mut buf, 0).unwrap(); // far fewer bytes than 1000 entries require
    let mut r = buf.as_slice();
    let err = read::read_map_len(&mut r, "test").unwrap_err();
    assert!(matches!(err, DecodeError::ImplausibleHeaderCount { len: 1000, .. }));
}

#[test]
fn split_trace_id_short_is_zero_padded() {
    // Fewer than 16 bytes: right-aligned, high bytes zero.
    let (high, low) = split_trace_id(&[0xaa, 0xbb]);
    assert_eq!(high, 0);
    assert_eq!(low, 0xaabb);

    // Exactly 16 bytes.
    let id: Vec<u8> = (0..16).collect();
    let (high, low) = split_trace_id(&id);
    assert_eq!(high, 0x0001_0203_0405_0607);
    assert_eq!(low, 0x0809_0a0b_0c0d_0e0f);
}

#[test]
fn split_trace_id_overlong_keeps_final_16_bytes() {
    // 17 bytes: the leading byte must be dropped, not the trailing one.
    let id: Vec<u8> = (0..17).collect();
    let (high, low) = split_trace_id(&id);
    assert_eq!(high, 0x0102_0304_0506_0708);
    assert_eq!(low, 0x090a_0b0c_0d0e_0f10);
}

#[test]
fn overlong_trace_id_in_chunk_uses_final_16_bytes() {
    let id: Vec<u8> = (0..17).collect();

    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 1).unwrap(); // tracer payload
    encode::write_uint(&mut buf, 11).unwrap(); // chunks
    encode::write_array_len(&mut buf, 1).unwrap();
    encode::write_map_len(&mut buf, 1).unwrap(); // trace chunk
    encode::write_uint(&mut buf, 6).unwrap(); // traceID
    encode::write_bin(&mut buf, &id).unwrap();

    let traces = decode_v1_payload(&buf).expect("payload with overlong trace ID should decode");
    assert_eq!(traces.len(), 1);
    let (expected_high, expected_low) = split_trace_id(&id[1..]);
    assert_eq!(traces[0].trace_id_high, expected_high);
    assert_eq!(traces[0].trace_id_low, expected_low);
}

#[test]
fn overlong_trace_id_in_span_link_uses_final_16_bytes() {
    let id: Vec<u8> = (0..17).collect();

    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 1).unwrap(); // span
    encode::write_uint(&mut buf, 11).unwrap(); // links
    encode::write_array_len(&mut buf, 1).unwrap();
    encode::write_map_len(&mut buf, 1).unwrap(); // span link
    encode::write_uint(&mut buf, 1).unwrap(); // traceID
    encode::write_bin(&mut buf, &id).unwrap();

    let mut strings = StringTable::new();
    let mut r = buf.as_slice();
    let span = decode_span(&mut r, &mut strings).unwrap();
    let (expected_high, expected_low) = split_trace_id(&id[1..]);
    assert_eq!(span.span_links()[0].trace_id_high(), expected_high);
    assert_eq!(span.span_links()[0].trace_id(), expected_low);
}

#[test]
fn trailing_bytes_after_payload_rejected_reserved_marker() {
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 0).unwrap(); // empty, otherwise-valid tracer payload
    buf.push(0xc1); // reserved MessagePack marker

    let err = decode_v1_payload(&buf).unwrap_err();
    assert!(matches!(err, DecodeError::TrailingBytes { len: 1 }));
}

#[test]
fn trailing_bytes_after_payload_rejected_valid_value() {
    let mut buf = Vec::new();
    encode::write_map_len(&mut buf, 0).unwrap(); // empty, otherwise-valid tracer payload
    encode::write_uint(&mut buf, 42).unwrap(); // an otherwise-valid MessagePack value

    let err = decode_v1_payload(&buf).unwrap_err();
    assert!(matches!(err, DecodeError::TrailingBytes { len: 1 }));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]
    #[test]
    fn property_test_arbitrary_bytes_never_panic(input in prop::collection::vec(any::<u8>(), 0..2048)) {
        // The decoder must never panic on malformed input; it returns Ok or Err. This is not
        // exhaustive but catches simple robustness regressions on every test run.
        let _ = decode_v1_payload(&input);
    }
}

//! Tests for the v0.4 decoder.
//!
//! Payloads are built with a small MessagePack value tree, so each test can pick exact wire encodings (integer widths,
//! `bin` versus `str`, field order). The scenarios cover the trace-agent's v0.4 decoding and its number reads.

use proptest::prelude::*;
use rmp::encode;
use saluki_core::data_model::event::trace::{AttributeValue, Span, Trace};
use stringtheory::MetaString;

use super::{
    decode_span_events, decode_span_links, decode_span_list, decode_v04_payload, ChunkPromoted, DecodeError, Strings,
};
use crate::decoders::datadog::{read, DecodedV1};

/// A MessagePack value. Integers use the smallest encoding unless written as `Raw`.
#[derive(Clone, Debug)]
enum V {
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    F64(f64),
    F32(f32),
    Str(String),
    Bin(Vec<u8>),
    Arr(Vec<V>),
    Map(Vec<(V, V)>),
    /// Bytes written as they are, for exact or invalid encodings.
    Raw(Vec<u8>),
}

fn s(value: &str) -> V {
    V::Str(value.to_string())
}

/// A map keyed by strings, the shape of spans, links and events.
fn obj(fields: Vec<(&str, V)>) -> V {
    V::Map(fields.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// A `map[string]string`, the shape of `meta`.
fn str_map(entries: &[(&str, &str)]) -> V {
    V::Map(entries.iter().map(|(k, v)| (s(k), s(v))).collect())
}

/// A `map[string]float64`, the shape of `metrics`.
fn f64_map(entries: &[(&str, f64)]) -> V {
    V::Map(entries.iter().map(|(k, v)| (s(k), V::F64(*v))).collect())
}

fn write(out: &mut Vec<u8>, value: &V) {
    match value {
        V::Nil => encode::write_nil(out).unwrap(),
        V::Bool(b) => encode::write_bool(out, *b).unwrap(),
        V::Int(i) => {
            encode::write_sint(out, *i).unwrap();
        }
        V::Uint(u) => {
            encode::write_uint(out, *u).unwrap();
        }
        V::F64(f) => encode::write_f64(out, *f).unwrap(),
        V::F32(f) => encode::write_f32(out, *f).unwrap(),
        V::Str(text) => encode::write_str(out, text).unwrap(),
        V::Bin(bytes) => encode::write_bin(out, bytes).unwrap(),
        V::Arr(values) => {
            encode::write_array_len(out, values.len() as u32).unwrap();
            for value in values {
                write(out, value);
            }
        }
        V::Map(entries) => {
            encode::write_map_len(out, entries.len() as u32).unwrap();
            for (key, value) in entries {
                write(out, key);
                write(out, value);
            }
        }
        V::Raw(bytes) => out.extend_from_slice(bytes),
    }
}

fn encode_value(value: &V) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, value);
    out
}

/// Encodes a `[][]Span` payload.
fn payload(chunks: Vec<Vec<V>>) -> Vec<u8> {
    encode_value(&V::Arr(chunks.into_iter().map(V::Arr).collect()))
}

fn decode(chunks: Vec<Vec<V>>) -> DecodedV1 {
    decode_v04_payload(&payload(chunks)).expect("payload should decode")
}

/// Decodes a payload holding one span, returning its trace.
fn decode_one(span: V) -> Trace {
    let mut decoded = decode(vec![vec![span]]);
    assert_eq!(decoded.traces.len(), 1);
    decoded.traces.remove(0)
}

fn decode_err(chunks: Vec<Vec<V>>) -> DecodeError {
    decode_v04_payload(&payload(chunks)).expect_err("payload should fail to decode")
}

fn ms(value: &str) -> MetaString {
    MetaString::from(value)
}

fn attr<'a>(span: &'a Span, key: &str) -> Option<&'a AttributeValue> {
    span.attributes.get(key)
}

fn string_attr(value: &str) -> AttributeValue {
    AttributeValue::String(ms(value))
}

/// A reference span, with the fields in the order the trace-agent's v0.4 encoder writes them.
fn reference_span(meta: &[(&str, &str)], metrics: &[(&str, f64)], trace_id: u64) -> Vec<(&'static str, V)> {
    vec![
        ("service", s("my-service")),
        ("name", s("span-name")),
        ("resource", s("GET /res")),
        ("trace_id", V::Uint(trace_id)),
        ("span_id", V::Uint(12345678)),
        ("parent_id", V::Uint(1111)),
        ("start", V::Int(171615)),
        ("duration", V::Int(234)),
        ("error", V::Int(0)),
        ("meta", str_map(meta)),
        ("metrics", f64_map(metrics)),
        ("type", s("web")),
    ]
}

const REFERENCE_META: &[(&str, &str)] = &[
    ("someStr", "bar"),
    ("_dd.p.tid", "BABA"),
    ("env", "production"),
    ("version", "1.2.3"),
    ("component", "http-client"),
    ("span.kind", "client"),
    ("_dd.git.commit.sha", "abc123def456"),
    ("_dd.p.dm", "-1"),
    ("_dd.hostname", "my-hostname"),
];

const REFERENCE_METRICS: &[(&str, f64)] = &[("someNum", 1.0), ("_sampling_priority_v1", 2.0)];

#[test]
fn converted_span_matches_reference() {
    // TestConvertedSpan.
    let decoded = decode(vec![vec![obj(reference_span(
        REFERENCE_META,
        REFERENCE_METRICS,
        556677,
    ))]]);
    let trace = &decoded.traces[0];
    let span = &trace.spans()[0];

    assert_eq!(span.service(), "my-service");
    assert_eq!(span.name(), "span-name");
    assert_eq!(span.resource(), "GET /res");
    assert_eq!(span.span_type(), "web");
    assert_eq!(span.span_id(), 12345678);
    assert_eq!(span.parent_id(), 1111);
    assert_eq!(span.start(), 171615);
    assert_eq!(span.duration(), 234);
    assert_eq!(span.error(), 0);
    assert_eq!(attr(span, "someNum"), Some(&AttributeValue::Float(1.0)));
    assert_eq!(attr(span, "someStr"), Some(&string_attr("bar")));
    assert_eq!(attr(span, "span.kind"), Some(&string_attr("client")));
    assert_eq!(attr(span, "_dd.convertedv1"), Some(&string_attr("v04")));
    assert_eq!(span.env, "production");
    assert_eq!(span.version, "1.2.3");
    assert_eq!(span.component, "http-client");
    assert_eq!(span.kind, 3, "client");

    assert_eq!(trace.trace_id_low, 556677);
    assert_eq!(trace.trace_id_high, 0xBABA);
    assert_eq!(trace.sampling_mechanism, 1);
    assert_eq!(trace.priority, Some(2));
    assert_eq!(trace.payload.hostname, "my-hostname");
    assert_eq!(trace.payload.env, "production");
    assert_eq!(trace.payload.app_version, "1.2.3");
    assert_eq!(
        decoded.payload_attributes.get("_dd.git.commit.sha"),
        Some(&string_attr("abc123def456"))
    );
}

#[test]
fn converted_trace_chunk_matches_reference() {
    // TestConvertedTraceChunk: the second span adds the origin.
    let mut second_meta = REFERENCE_META.to_vec();
    second_meta.push(("_dd.origin", "lambda"));
    let trace = &decode(vec![vec![
        obj(reference_span(REFERENCE_META, REFERENCE_METRICS, 0xAA)),
        obj(reference_span(&second_meta, REFERENCE_METRICS, 0xAA)),
    ]])
    .traces[0];

    assert_eq!(trace.spans().len(), 2);
    assert_eq!((trace.trace_id_high, trace.trace_id_low), (0xBABA, 0xAA));
    assert_eq!(trace.sampling_mechanism, 1);
    assert_eq!(trace.priority, Some(2));
    assert_eq!(trace.origin, "lambda");
}

#[test]
fn converted_trace_payload_matches_reference() {
    // TestConvertedTracePayload: the payload takes each field from wherever it appears.
    let without = |keys: &[&str]| -> Vec<(&'static str, &'static str)> {
        REFERENCE_META
            .iter()
            .copied()
            .filter(|(k, _)| !keys.contains(k))
            .collect()
    };
    let first = without(&["_dd.hostname"]);
    let mut second = without(&["_dd.hostname", "version"]);
    second.extend([("_dd.origin", "lambda"), ("_dd.apm_mode", "edge")]);
    let third = without(&["version"]);

    let decoded = decode(vec![
        vec![
            obj(reference_span(&first, REFERENCE_METRICS, 556677)),
            obj(reference_span(&second, REFERENCE_METRICS, 556677)),
        ],
        vec![obj(reference_span(&third, REFERENCE_METRICS, 5566881))],
    ]);

    assert_eq!(decoded.traces.len(), 2);
    for trace in &decoded.traces {
        assert_eq!(trace.payload.env, "production");
        assert_eq!(trace.payload.hostname, "my-hostname");
        assert_eq!(trace.payload.app_version, "1.2.3");
    }
    assert_eq!(
        decoded.payload_attributes.get("_dd.git.commit.sha"),
        Some(&string_attr("abc123def456"))
    );
    assert_eq!(
        decoded.payload_attributes.get("_dd.apm_mode"),
        Some(&string_attr("edge"))
    );
}

fn priority_span(parent_id: u64, priority: Option<f64>) -> V {
    let metrics = match priority {
        Some(priority) => f64_map(&[("_sampling_priority_v1", priority)]),
        None => V::Nil,
    };
    obj(vec![("parent_id", V::Uint(parent_id)), ("metrics", metrics)])
}

#[test]
fn root_span_priority_wins_over_later_spans() {
    // TestUnmarshalMsgConvertedRootSpanPriorityNotOverwritten, and
    // TestReconcileSamplingPriorityAfterChunkSpan_preservesChildWhenRootHasNoSamplingMetric.
    let cases: &[(&str, Vec<V>, Option<i32>)] = &[
        (
            "root then child",
            vec![priority_span(0, Some(2.0)), priority_span(1, Some(-1.0))],
            Some(2),
        ),
        (
            "root without a priority then child",
            vec![priority_span(0, None), priority_span(1, Some(1.0))],
            Some(1),
        ),
        (
            "child before root, which carries the child's value forward",
            vec![
                priority_span(1, Some(1.0)),
                priority_span(0, None),
                priority_span(1, Some(0.0)),
            ],
            Some(1),
        ),
        (
            "child, root, child",
            vec![
                priority_span(1, Some(1.0)),
                priority_span(0, Some(2.0)),
                priority_span(1, Some(0.0)),
            ],
            Some(2),
        ),
        (
            "only children: the last wins",
            vec![priority_span(1, Some(1.0)), priority_span(2, Some(-1.0))],
            Some(-1),
        ),
        ("no priority", vec![priority_span(0, None)], None),
    ];
    for (name, spans, expected) in cases {
        let trace = &decode(vec![spans.clone()]).traces[0];
        assert_eq!(trace.priority, *expected, "{name}");
    }
}

// The six differences from the trace-agent's other v0.x conversion listed in the module documentation.

#[test]
fn difference_1_promoted_tags_stay_in_attributes() {
    let trace = decode_one(obj(vec![(
        "meta",
        str_map(&[
            ("env", "prod"),
            ("version", "1.0"),
            ("component", "grpc"),
            ("span.kind", "server"),
        ]),
    )]));
    let span = &trace.spans()[0];
    for (key, value) in [
        ("env", "prod"),
        ("version", "1.0"),
        ("component", "grpc"),
        ("span.kind", "server"),
    ] {
        assert_eq!(attr(span, key), Some(&string_attr(value)), "{key}");
    }
    assert_eq!(
        (span.env.as_ref(), span.version.as_ref(), span.component.as_ref()),
        ("prod", "1.0", "grpc")
    );
    assert_eq!(span.kind, 2, "server");
}

#[test]
fn difference_2_last_span_and_last_chunk_win() {
    let meta_span = |meta: &[(&str, &str)]| obj(vec![("meta", str_map(meta))]);
    let full = |n: &str| -> Vec<(String, String)> {
        [
            "env",
            "version",
            "_dd.hostname",
            "_dd.git.commit.sha",
            "_dd.apm_mode",
            "_dd.origin",
        ]
        .iter()
        .map(|k| (k.to_string(), format!("{k}-{n}")))
        .chain([
            ("_dd.p.dm".to_string(), format!("-{n}")),
            ("_dd.p.tid".to_string(), n.to_string()),
        ])
        .collect()
    };
    let as_refs = |pairs: &[(String, String)]| -> V { V::Map(pairs.iter().map(|(k, v)| (s(k), s(v))).collect()) };

    let decoded = decode(vec![
        vec![
            obj(vec![("meta", as_refs(&full("1")))]),
            obj(vec![("meta", as_refs(&full("2")))]),
        ],
        vec![obj(vec![("meta", as_refs(&full("3")))]), meta_span(&[("other", "x")])],
        // A later chunk without the fields leaves the payload's values alone.
        vec![meta_span(&[("other", "y")])],
    ]);

    let first = &decoded.traces[0];
    assert_eq!(first.origin, "_dd.origin-2");
    assert_eq!(first.sampling_mechanism, 2);
    assert_eq!(first.trace_id_high, 2);
    let second = &decoded.traces[1];
    assert_eq!(second.origin, "_dd.origin-3");
    assert_eq!(second.sampling_mechanism, 3);
    assert_eq!(second.trace_id_high, 3);
    assert_eq!(decoded.traces[2].origin, "");

    let payload = &decoded.traces[0].payload;
    assert_eq!(payload.env, "env-3");
    assert_eq!(payload.app_version, "version-3");
    assert_eq!(payload.hostname, "_dd.hostname-3");
    assert_eq!(
        decoded.payload_attributes.get("_dd.git.commit.sha"),
        Some(&string_attr("_dd.git.commit.sha-3"))
    );
    assert_eq!(
        decoded.payload_attributes.get("_dd.apm_mode"),
        Some(&string_attr("_dd.apm_mode-3"))
    );

    // Within a chunk, an empty value on a later span clears an earlier one, so the payload falls back to the
    // previous chunk's.
    let decoded = decode(vec![
        vec![meta_span(&[("env", "first")])],
        vec![meta_span(&[("env", "second")]), meta_span(&[("env", "")])],
    ]);
    assert_eq!(decoded.traces[0].payload.env, "first");
}

#[test]
fn difference_3_empty_chunks_are_kept_and_nil_spans_dropped() {
    // TestChunkUnmarshalMsgConvertedDropsNilSpans, plus an empty chunk.
    let decoded = decode(vec![vec![], vec![V::Nil, obj(vec![]), V::Nil], vec![V::Nil]]);
    let span_counts: Vec<_> = decoded.traces.iter().map(|trace| trace.spans().len()).collect();
    assert_eq!(span_counts, [0, 1, 0]);
    assert_eq!(decoded.traces[0].priority, None);

    // A `nil` chunk is not a list of spans.
    let error = decode_v04_payload(&encode_value(&V::Arr(vec![V::Nil]))).unwrap_err();
    assert!(
        matches!(
            error,
            DecodeError::Msgpack {
                context: "span list",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn difference_4_chunk_trace_id_comes_from_last_span() {
    let trace = &decode(vec![vec![
        obj(vec![("trace_id", V::Uint(1)), ("meta", str_map(&[("_dd.p.tid", "a")]))]),
        obj(vec![("trace_id", V::Uint(2))]),
        obj(vec![("span_id", V::Uint(3))]),
    ]])
    .traces[0];
    assert_eq!((trace.trace_id_high, trace.trace_id_low), (0xa, 2));

    // A `nil` trace ID after a non-zero one is an error only as the span's last field.
    let nil_last = obj(vec![("span_id", V::Uint(1)), ("trace_id", V::Nil)]);
    let error = decode_err(vec![vec![obj(vec![("trace_id", V::Uint(7))]), nil_last.clone()]]);
    assert!(
        matches!(
            error,
            DecodeError::Msgpack {
                context: "span trace_id",
                ..
            }
        ),
        "{error:?}"
    );

    let nil_first = obj(vec![("trace_id", V::Nil), ("span_id", V::Uint(1))]);
    let trace = &decode(vec![vec![obj(vec![("trace_id", V::Uint(7))]), nil_first]]).traces[0];
    assert_eq!(trace.trace_id_low, 7, "a nil trace ID leaves the earlier one");

    // Without an earlier trace ID, a trailing `nil` is fine.
    assert_eq!(decode_one(nil_last).trace_id_low, 0);
}

#[test]
fn difference_5_wire_order_decides_meta_and_metrics_clash() {
    let meta = ("meta", str_map(&[("clash", "text")]));
    let metrics = ("metrics", f64_map(&[("clash", 4.0)]));

    let trace = decode_one(obj(vec![meta.clone(), metrics.clone()]));
    assert_eq!(attr(&trace.spans()[0], "clash"), Some(&AttributeValue::Float(4.0)));

    let trace = decode_one(obj(vec![metrics, meta]));
    assert_eq!(attr(&trace.spans()[0], "clash"), Some(&string_attr("text")));
}

#[test]
fn difference_6_invalid_dm_is_ignored_and_float32_metrics_are_rejected() {
    let dm_span = |dm: &str| obj(vec![("meta", str_map(&[("_dd.p.dm", dm)]))]);
    let trace = &decode(vec![vec![dm_span("-4"), dm_span("not-a-number")]]).traces[0];
    assert_eq!(
        trace.sampling_mechanism, 4,
        "an invalid value keeps the earlier mechanism"
    );

    let error = decode_err(vec![vec![obj(vec![(
        "metrics",
        V::Map(vec![(s("_sampling_priority_v1"), V::F32(1.0))]),
    )])]]);
    assert!(
        matches!(
            error,
            DecodeError::Msgpack {
                context: "span metrics value",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn promoted_trace_id_high_parses_like_strconv() {
    let cases = [
        ("BABA", Some(0xBABA)),
        ("ffffffffffffffff", Some(u64::MAX)),
        ("0000000000000001", Some(1)),
        ("0xBABA", None),
        ("+1", None),
        ("-1", None),
        ("", None),
        ("zz", None),
        ("10000000000000000", None),
    ];
    for (tid, expected) in cases {
        let trace = &decode(vec![vec![
            obj(vec![("meta", str_map(&[("_dd.p.tid", "5")]))]),
            obj(vec![("meta", str_map(&[("_dd.p.tid", tid)]))]),
        ]])
        .traces[0];
        assert_eq!(trace.trace_id_high, expected.unwrap_or(5), "{tid:?}");
        assert_eq!(attr(&trace.spans()[1], "_dd.p.tid"), Some(&string_attr(tid)), "{tid:?}");
    }
}

#[test]
fn promoted_decision_maker_parses_like_strconv() {
    let cases = [
        ("-4", 4),
        ("4", 4),
        ("-0", 0),
        ("4294967295", u32::MAX),
        ("--4", 9),
        ("+4", 9),
        ("4294967296", 9),
        ("", 9),
    ];
    for (dm, expected) in cases {
        let trace = &decode(vec![vec![
            obj(vec![("meta", str_map(&[("_dd.p.dm", "-9")]))]),
            obj(vec![("meta", str_map(&[("_dd.p.dm", dm)]))]),
        ]])
        .traces[0];
        assert_eq!(trace.sampling_mechanism, expected, "{dm:?}");
    }
}

#[test]
fn span_kind_maps_to_the_otel_values() {
    let cases = [
        (Some("server"), 2),
        (Some("client"), 3),
        (Some("producer"), 4),
        (Some("consumer"), 5),
        (Some("internal"), 1),
        (Some("SERVER"), 1),
        (Some(""), 1),
        (None, 1),
    ];
    for (kind, expected) in cases {
        let meta = match kind {
            Some(kind) => str_map(&[("span.kind", kind)]),
            None => V::Nil,
        };
        let trace = decode_one(obj(vec![("meta", meta)]));
        assert_eq!(trace.spans()[0].kind, expected, "{kind:?}");
    }
}

#[test]
fn numbers_are_as_tolerant_as_the_reference() {
    // TestParseFloat64Bytes, through the metrics map.
    let metric_values = [
        (V::Raw(vec![0x2a]), 42.0),
        (V::Raw(vec![0xd1, 0xfb, 0x2e]), -1234.0),
        (V::Raw(vec![0xcd, 0x0a, 0x9b]), 2715.0),
        (
            V::Raw(vec![0xcb, 0x40, 0x09, 0x1e, 0xb8, 0x51, 0xeb, 0x85, 0x1f]),
            f64::from_bits(0x4009_1eb8_51eb_851f),
        ),
        (V::Raw(vec![0xcf, 0, 0, 0, 0, 0, 0, 0, 7]), 7.0),
        (V::Nil, 0.0),
    ];
    for (value, expected) in metric_values {
        let trace = decode_one(obj(vec![("metrics", V::Map(vec![(s("m"), value.clone())]))]));
        assert_eq!(
            attr(&trace.spans()[0], "m"),
            Some(&AttributeValue::Float(expected)),
            "{value:?}"
        );
    }

    let trace = decode_one(obj(vec![
        // A signed ID is reinterpreted as unsigned, as some tracers have no unsigned types.
        ("span_id", V::Int(-1)),
        ("parent_id", V::Raw(vec![0xd3, 0, 0, 0, 0, 0, 0, 0, 9])),
        ("start", V::Uint(1_700_000_000_000_000_000)),
        ("duration", V::Raw(vec![0xce, 0, 0, 1, 0])),
        ("error", V::Raw(vec![0xcc, 2])),
    ]));
    let span = &trace.spans()[0];
    assert_eq!(span.span_id(), u64::MAX);
    assert_eq!(span.parent_id(), 9);
    assert_eq!(span.start(), 1_700_000_000_000_000_000);
    assert_eq!(span.duration(), 256);
    assert_eq!(span.error(), 1, "any non-zero error code is an error");

    let trace = decode_one(obj(vec![
        ("service", V::Nil),
        ("span_id", V::Nil),
        ("start", V::Nil),
        ("error", V::Nil),
        ("meta", V::Nil),
        ("metrics", V::Nil),
    ]));
    let span = &trace.spans()[0];
    assert_eq!(
        (span.service(), span.span_id(), span.start(), span.error()),
        ("", 0, 0, 0)
    );

    let rejected = [
        ("start", V::Uint(u64::MAX), "span start"),
        ("error", V::Uint(u64::from(u32::MAX)), "span error"),
        ("error", V::Int(i64::from(i32::MAX) + 1), "span error"),
        ("span_id", V::F64(1.0), "span span_id"),
        ("span_id", s("1"), "span span_id"),
        ("service", V::Int(1), "span service"),
    ];
    for (field, value, context) in rejected {
        let error = decode_err(vec![vec![obj(vec![(field, value.clone())])]]);
        assert!(
            matches!(error, DecodeError::Msgpack { context: c, .. } if c == context),
            "{field} = {value:?}: {error:?}"
        );
    }
}

#[test]
fn strings_accept_bin_and_repair_invalid_utf8() {
    // TestParseStringBytesRef.
    let cases: [(V, &str); 5] = [
        (s("http.method"), "http.method"),
        (s(""), ""),
        (V::Nil, ""),
        (V::Bin(b"binary-value".to_vec()), "binary-value"),
        (V::Bin(b"bad\x80value".to_vec()), "bad\u{FFFD}value"),
    ];
    for (value, expected) in cases {
        let trace = decode_one(obj(vec![("resource", value.clone())]));
        assert_eq!(trace.spans()[0].resource(), expected, "{value:?}");
    }

    // Each byte of an invalid sequence becomes its own replacement character, as in the trace-agent.
    let truncated_emoji = b"a\xf0\x9f\x98b".to_vec();
    let mut raw = vec![0xa0 | truncated_emoji.len() as u8];
    raw.extend_from_slice(&truncated_emoji);
    let trace = decode_one(obj(vec![("name", V::Raw(raw))]));
    assert_eq!(trace.spans()[0].name(), "a\u{FFFD}\u{FFFD}\u{FFFD}b");
}

#[test]
fn long_strings_share_one_allocation_per_payload() {
    let long = "a-service-name-longer-than-the-inline-limit";
    let decoded = decode(vec![
        vec![obj(vec![("service", s(long))])],
        vec![obj(vec![("service", s(long))])],
    ]);
    let first = decoded.traces[0].spans()[0].service();
    let second = decoded.traces[1].spans()[0].service();
    assert_eq!(first, long);
    assert_eq!(first.as_ptr(), second.as_ptr());
}

#[test]
fn unknown_fields_and_trailing_bytes_are_skipped() {
    let mut bytes = payload(vec![vec![obj(vec![
        ("unknown", V::Map(vec![(s("nested"), V::Arr(vec![s("x"), V::Int(1)]))])),
        ("service", s("svc")),
    ])]]);
    bytes.extend_from_slice(&[0xc1, 0xff]);
    let decoded = decode_v04_payload(&bytes).unwrap();
    assert_eq!(decoded.traces[0].spans()[0].service(), "svc");
}

#[test]
fn short_input_is_distinguishable() {
    let full = payload(vec![vec![obj(reference_span(REFERENCE_META, REFERENCE_METRICS, 1))]]);
    let cases: &[(&str, &[u8])] = &[
        ("empty payload", &[]),
        ("trace list declares more chunks than bytes remain", &[0x92, 0x90]),
        ("truncated map key", &[0x91, 0x91, 0x81, 0xa7, b's', b'e']),
        ("truncated string value", &[0x91, 0x91, 0x81, 0xa1, b'x', 0xa3, b'a']),
        ("truncated uint64", &[0x91, 0x91, 0x81, 0xa1, b'x', 0xcf, 0, 0]),
        ("truncated full payload", &full[..full.len() - 3]),
    ];
    for (name, bytes) in cases {
        let error = decode_v04_payload(bytes).unwrap_err();
        assert!(matches!(error, DecodeError::ShortBytes { .. }), "{name}: {error:?}");
    }

    // As in the trace-agent, input that ends exactly where a number or string value should start is a type error,
    // not short input.
    let error = decode_v04_payload(&[0x91, 0x91, 0x81, 0xa7, b's', b'p', b'a', b'n', b'_', b'i', b'd']).unwrap_err();
    assert!(matches!(error, DecodeError::Msgpack { .. }), "{error:?}");

    // Every strict prefix of a valid payload fails to decode.
    for len in 0..full.len() {
        assert!(decode_v04_payload(&full[..len]).is_err(), "prefix of {len} bytes");
    }
}

#[test]
fn collection_counts_are_bounded() {
    // TestDecodersRejectCountsAboveLimit.
    let mut bytes = vec![0xdd];
    bytes.extend_from_slice(&500_001u32.to_be_bytes());
    bytes.resize(bytes.len() + 500_001, 0xc0);
    let error = decode_v04_payload(&bytes).unwrap_err();
    assert!(
        matches!(error, DecodeError::OversizeHeader { max: 500_000, .. }),
        "{error:?}"
    );

    // A meta map claiming more entries than two bytes each could back.
    let mut span = encode_value(&V::Map(vec![(s("meta"), V::Raw(vec![0x83]))]));
    span.extend_from_slice(&[0xa1, b'k', 0xc0]);
    let mut bytes = vec![0x91, 0x91];
    bytes.extend_from_slice(&span);
    let error = decode_v04_payload(&bytes).unwrap_err();
    assert!(
        matches!(error, DecodeError::ImplausibleHeaderCount { len: 3, .. }),
        "{error:?}"
    );
}

proptest! {
    #[test]
    fn property_test_arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        // Any outcome is fine as long as decoding returns. Each decoded chunk takes at least one byte.
        if let Ok(decoded) = decode_v04_payload(&bytes) {
            prop_assert!(decoded.traces.len() <= bytes.len());
        }
    }

    #[test]
    fn property_test_decodes_what_was_encoded(
        spans in prop::collection::vec(
            (
                "[a-z]{0,40}",
                any::<u64>(),
                any::<u64>(),
                any::<i64>(),
                prop::collection::btree_map("[a-z]{1,8}", "[ -~]{0,30}", 0..4),
                prop::collection::btree_map("[A-Z]{1,8}", -1e9f64..1e9, 0..4),
            ),
            0..6,
        ),
    ) {
        let encoded: Vec<V> = spans
            .iter()
            .map(|(service, span_id, trace_id, start, meta, metrics)| {
                obj(vec![
                    ("service", s(service)),
                    ("span_id", V::Uint(*span_id)),
                    ("trace_id", V::Uint(*trace_id)),
                    ("start", V::Int(*start)),
                    ("meta", V::Map(meta.iter().map(|(k, v)| (s(k), s(v))).collect())),
                    ("metrics", V::Map(metrics.iter().map(|(k, v)| (s(k), V::F64(*v))).collect())),
                ])
            })
            .collect();
        let decoded = decode(vec![encoded]);
        let trace = &decoded.traces[0];
        prop_assert_eq!(trace.spans().len(), spans.len());
        for (span, (service, span_id, _, start, meta, metrics)) in trace.spans().iter().zip(&spans) {
            prop_assert_eq!(span.service(), service.as_str());
            prop_assert_eq!(span.span_id(), *span_id);
            prop_assert_eq!(span.start(), *start as u64);
            // Every meta and metrics entry, plus `_dd.convertedv1`.
            prop_assert_eq!(span.attributes.len(), meta.len() + metrics.len() + 1);
            for (key, value) in meta {
                prop_assert_eq!(span.attributes.get(key.as_str()), Some(&string_attr(value)));
            }
            for (key, value) in metrics {
                prop_assert_eq!(span.attributes.get(key.as_str()), Some(&AttributeValue::Float(*value)));
            }
        }
        prop_assert_eq!(trace.trace_id_low, spans.last().map_or(0, |span| span.2));
    }
}

#[test]
fn span_links_match_reference() {
    // TestConvertedSpanLinks, with a `nil` link dropped as in TestSpanUnmarshalMsgConvertedDropsNilLinksAndEvents.
    let link = obj(vec![
        ("trace_id", V::Uint(0xAABB)),
        ("trace_id_high", V::Uint(0x1234)),
        ("span_id", V::Uint(1111)),
        ("attributes", str_map(&[("link.attr", "link.value")])),
        ("tracestate", s("test=state")),
        ("flags", V::Uint(1)),
    ]);
    let trace = decode_one(obj(vec![("span_links", V::Arr(vec![V::Nil, link, obj(vec![])]))]));
    let links = trace.spans()[0].span_links();
    assert_eq!(links.len(), 2);
    let link = &links[0];
    assert_eq!((link.trace_id_high(), link.trace_id()), (0x1234, 0xAABB));
    assert_eq!(link.span_id(), 1111);
    assert_eq!(link.tracestate(), "test=state");
    assert_eq!(link.flags(), 1);
    assert_eq!(link.attributes().len(), 1);
    assert_eq!(link.attributes().get("link.attr"), Some(&string_attr("link.value")));

    // `trace_id_high` and `flags` are strict unsigned reads, unlike the IDs.
    for (field, value) in [
        ("trace_id_high", V::Int(-1)),
        ("flags", V::Uint(u64::from(u32::MAX) + 1)),
    ] {
        let error = decode_err(vec![vec![obj(vec![(
            "span_links",
            V::Arr(vec![obj(vec![(field, value)])]),
        )])]]);
        assert!(matches!(error, DecodeError::Msgpack { .. }), "{field}: {error:?}");
    }
    // A `nil` list is an error, unlike a `nil` meta map.
    let error = decode_err(vec![vec![obj(vec![("span_links", V::Nil)])]]);
    assert!(
        matches!(
            error,
            DecodeError::Msgpack {
                context: "span links",
                ..
            }
        ),
        "{error:?}"
    );
}

fn any_value(fields: Vec<(&str, V)>) -> V {
    obj(fields)
}

#[test]
fn span_events_match_reference() {
    // TestConvertedSpanEvents.
    let array = any_value(vec![
        ("type", V::Int(4)),
        (
            "array_value",
            obj(vec![(
                "values",
                V::Arr(vec![
                    any_value(vec![("type", V::Int(0)), ("string_value", s("array.value"))]),
                    any_value(vec![("type", V::Int(2)), ("int_value", V::Int(100))]),
                    any_value(vec![("type", V::Int(1)), ("bool_value", V::Bool(true))]),
                    any_value(vec![("type", V::Int(3)), ("double_value", V::F64(2.5))]),
                ]),
            )]),
        ),
    ]);
    let event = obj(vec![
        ("time_unix_nano", V::Uint(171615)),
        ("name", s("span-name")),
        (
            "attributes",
            V::Map(vec![
                (
                    s("event.attr"),
                    any_value(vec![("type", V::Int(0)), ("string_value", s("event.value"))]),
                ),
                (
                    s("event.bool"),
                    any_value(vec![("type", V::Int(1)), ("bool_value", V::Bool(true))]),
                ),
                (
                    s("event.int"),
                    any_value(vec![("type", V::Int(2)), ("int_value", V::Int(42))]),
                ),
                (
                    s("event.double"),
                    any_value(vec![("type", V::Int(3)), ("double_value", V::F64(2.5))]),
                ),
                (s("event.array"), array),
            ]),
        ),
    ]);
    let trace = decode_one(obj(vec![("span_events", V::Arr(vec![V::Nil, event]))]));
    let events = trace.spans()[0].span_events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.time_unix_nano(), 171615);
    assert_eq!(event.name(), "span-name");
    let attributes = event.attributes();
    assert_eq!(attributes.len(), 5);
    assert_eq!(attributes.get("event.attr"), Some(&string_attr("event.value")));
    assert_eq!(attributes.get("event.bool"), Some(&AttributeValue::Bool(true)));
    assert_eq!(attributes.get("event.int"), Some(&AttributeValue::Int(42)));
    assert_eq!(attributes.get("event.double"), Some(&AttributeValue::Float(2.5)));
    assert_eq!(
        attributes.get("event.array"),
        Some(&AttributeValue::Array(vec![
            string_attr("array.value"),
            AttributeValue::Int(100),
            AttributeValue::Bool(true),
            AttributeValue::Float(2.5),
        ]))
    );
}

#[test]
fn span_event_attributes_drop_nil_and_unknown_values() {
    // TestSpanEventUnmarshalMsgConvertedDropsNilEntries, plus an unknown type and a `float32` double.
    let array = any_value(vec![
        ("type", V::Int(4)),
        (
            "array_value",
            obj(vec![(
                "values",
                V::Arr(vec![
                    V::Nil,
                    any_value(vec![("type", V::Int(0)), ("string_value", s("x"))]),
                    V::Nil,
                    any_value(vec![("type", V::Int(9))]),
                ]),
            )]),
        ),
    ]);
    let event = obj(vec![(
        "attributes",
        V::Map(vec![
            (
                s("keepStr"),
                any_value(vec![("type", V::Int(0)), ("string_value", s("keep"))]),
            ),
            (s("nilAttr"), V::Nil),
            (s("unknownType"), any_value(vec![("type", V::Int(9))])),
            (
                s("float32"),
                any_value(vec![("type", V::Int(3)), ("double_value", V::F32(0.5))]),
            ),
            (s("arr"), array),
        ]),
    )]);
    let trace = decode_one(obj(vec![("span_events", V::Arr(vec![event]))]));
    let attributes = trace.spans()[0].span_events()[0].attributes();
    assert_eq!(attributes.len(), 3, "{attributes:?}");
    assert_eq!(attributes.get("keepStr"), Some(&string_attr("keep")));
    assert_eq!(attributes.get("float32"), Some(&AttributeValue::Float(0.5)));
    assert_eq!(
        attributes.get("arr"),
        Some(&AttributeValue::Array(vec![string_attr("x")]))
    );

    // Nesting is bounded.
    let mut nested = any_value(vec![("type", V::Int(0))]);
    for _ in 0..70 {
        nested = any_value(vec![
            ("type", V::Int(4)),
            ("array_value", obj(vec![("values", V::Arr(vec![nested]))])),
        ]);
    }
    let event = obj(vec![("attributes", V::Map(vec![(s("deep"), nested)]))]);
    let error = decode_err(vec![vec![obj(vec![("span_events", V::Arr(vec![event]))])]]);
    assert!(matches!(error, DecodeError::DepthExceeded { max: 64 }), "{error:?}");
}

#[test]
fn meta_struct_maps_to_bytes_attributes() {
    let trace = decode_one(obj(vec![
        ("meta", str_map(&[("bts", "text")])),
        ("meta_struct", V::Map(vec![(s("bts"), V::Bin(b"bar".to_vec()))])),
    ]));
    assert_eq!(
        attr(&trace.spans()[0], "bts"),
        Some(&AttributeValue::Bytes(b"bar".to_vec()))
    );

    // Values are `bin` only, and a `nil` map is an error.
    for value in [V::Map(vec![(s("k"), s("v"))]), V::Nil] {
        let error = decode_err(vec![vec![obj(vec![("meta_struct", value.clone())])]]);
        assert!(matches!(error, DecodeError::Msgpack { .. }), "{value:?}: {error:?}");
    }
}

/// Tracks the live heap bytes allocated by the current thread, so a test can bound what a decode allocates.
pub(crate) mod alloc_tracking {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct Tracking;

    #[global_allocator]
    static ALLOCATOR: Tracking = Tracking;

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    fn record(delta: isize) {
        let _ = LIVE.try_with(|live| {
            let now = live.get() + delta;
            live.set(now);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }

    unsafe impl GlobalAlloc for Tracking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                record(layout.size() as isize);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            record(-(layout.size() as isize));
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new = unsafe { System.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                record(new_size as isize - layout.size() as isize);
            }
            new
        }
    }

    /// Runs `f` and returns its result with the peak bytes it held at once, including what the result keeps.
    pub fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let base = LIVE.with(Cell::get);
        PEAK.with(|peak| peak.set(base));
        let result = f();
        let peak = PEAK.with(Cell::get);
        (result, (peak - base).max(0) as usize)
    }
}

#[test]
fn nil_entries_do_not_keep_preallocated_capacity() {
    let mut nils = vec![0xdd];
    nils.extend_from_slice(&5001u32.to_be_bytes());
    nils.resize(nils.len() + 5000, 0xc0);
    nils.push(0x80);

    let mut strings = Strings::default();
    let mut r = nils.as_slice();
    let spans = decode_span_list(&mut r, &mut strings, &mut ChunkPromoted::default()).unwrap();
    assert_eq!(spans.len(), 1);
    assert!(spans.capacity() <= 4, "span capacity {}", spans.capacity());

    let mut links = Vec::new();
    decode_span_links(&mut nils.as_slice(), &mut strings, &mut links).unwrap();
    assert_eq!(links.len(), 1);
    assert!(links.capacity() <= 4, "link capacity {}", links.capacity());

    let mut events = Vec::new();
    decode_span_events(&mut nils.as_slice(), &mut strings, &mut events).unwrap();
    assert_eq!(events.len(), 1);
    assert!(events.capacity() <= 4, "event capacity {}", events.capacity());
}

#[test]
fn attribute_maps_of_repeated_keys_do_not_keep_preallocated_capacity() {
    // One span whose `meta` holds 1,024 `nil: nil` entries, which collapse into one `"": ""` attribute.
    let mut bytes = vec![0x91, 0x91, 0x81, 0xa4];
    bytes.extend_from_slice(b"meta");
    bytes.push(0xde);
    bytes.extend_from_slice(&1024u16.to_be_bytes());
    bytes.resize(bytes.len() + 2 * 1024, 0xc0);

    let decoded = decode_v04_payload(&bytes).unwrap();
    let attributes = &decoded.traces[0].spans()[0].attributes;
    // The `_dd.convertedv1` attribute is added after the map.
    assert_eq!(attributes.len(), 2);
    assert!(
        attributes.capacity() <= 16,
        "attribute capacity {}",
        attributes.capacity()
    );
}

#[test]
fn nil_heavy_payloads_allocate_in_proportion_to_their_size() {
    // 100 chunks of 5,000 `nil` spans: about 500 KB that decode to 100 empty traces.
    let bytes = payload((0..100).map(|_| vec![V::Nil; 5000]).collect());
    let (decoded, peak) = alloc_tracking::peak_during(|| decode_v04_payload(&bytes).unwrap());
    assert_eq!(decoded.traces.len(), 100);
    // One chunk's capped pre-allocation at a time, plus the traces themselves.
    let bound = read::MAX_PREALLOCATED_ELEMENTS * std::mem::size_of::<Span>() + 16 * bytes.len();
    assert!(
        peak <= bound,
        "peak {peak} bytes for a {}-byte payload (bound {bound})",
        bytes.len()
    );

    // A list header claiming far more spans than decode fails without reserving for the claim.
    let mut bytes = vec![0x91, 0xdd];
    bytes.extend_from_slice(&1_000_000u32.to_be_bytes());
    bytes.resize(bytes.len() + 1_000_000, 0xc1);
    let (result, peak) = alloc_tracking::peak_during(|| decode_v04_payload(&bytes));
    assert!(result.is_err());
    let bound = 2 * read::MAX_PREALLOCATED_ELEMENTS * std::mem::size_of::<Span>();
    assert!(peak <= bound, "peak {peak} bytes (bound {bound})");
}

#[test]
fn float_reads_check_the_length_first_as_msgp_does() {
    let f32_bytes = 0.5f32.to_be_bytes();
    let mut float32 = vec![0xca];
    float32.extend_from_slice(&f32_bytes);
    assert_eq!(read::v0_read_f64(&mut float32.as_slice(), "test").unwrap(), 0.5);

    let short: [&[u8]; 4] = [&[], &[0x01], &[0xcb, 0, 0, 0, 0, 0, 0, 0], &[0xca, 0, 0]];
    for bytes in short {
        let error = read::v0_read_f64(&mut &bytes[..], "test").unwrap_err();
        assert!(matches!(error, DecodeError::ShortBytes { .. }), "{bytes:?}: {error:?}");
    }

    // With nine bytes or more, a non-float marker is a type error.
    let error = read::v0_read_f64(&mut &[0x01; 9][..], "test").unwrap_err();
    assert!(matches!(error, DecodeError::Msgpack { .. }), "{error:?}");
}

fn any_value_strategy() -> impl Strategy<Value = V> {
    let leaf = prop_oneof![
        "[ -~]{0,30}".prop_map(|v| any_value(vec![("type", V::Int(0)), ("string_value", V::Str(v))])),
        any::<bool>().prop_map(|v| any_value(vec![("type", V::Int(1)), ("bool_value", V::Bool(v))])),
        any::<i64>().prop_map(|v| any_value(vec![("type", V::Int(2)), ("int_value", V::Int(v))])),
        any::<f64>().prop_map(|v| any_value(vec![("type", V::Int(3)), ("double_value", V::F64(v))])),
        Just(V::Nil),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop::collection::vec(inner, 0..4).prop_map(|values| {
            any_value(vec![
                ("type", V::Int(4)),
                ("array_value", obj(vec![("values", V::Arr(values))])),
            ])
        })
    })
}

fn string_map_strategy() -> impl Strategy<Value = V> {
    let key = prop_oneof![
        Just("env".to_string()),
        Just("version".to_string()),
        Just("span.kind".to_string()),
        Just("_dd.p.tid".to_string()),
        Just("_dd.p.dm".to_string()),
        Just("_dd.origin".to_string()),
        "[a-z.]{1,12}",
    ];
    prop::collection::vec((key, "[ -~]{0,40}"), 0..6)
        .prop_map(|entries| V::Map(entries.into_iter().map(|(k, v)| (V::Str(k), V::Str(v))).collect()))
}

fn link_strategy() -> impl Strategy<Value = V> {
    prop_oneof![
        1 => Just(V::Nil),
        4 => (any::<u64>(), any::<u64>(), any::<u64>(), string_map_strategy(), "[ -~]{0,20}", any::<u32>()).prop_map(
            |(trace_id, trace_id_high, span_id, attributes, tracestate, flags)| {
                obj(vec![
                    ("trace_id", V::Uint(trace_id)),
                    ("trace_id_high", V::Uint(trace_id_high)),
                    ("span_id", V::Uint(span_id)),
                    ("attributes", attributes),
                    ("tracestate", V::Str(tracestate)),
                    ("flags", V::Uint(u64::from(flags))),
                ])
            }
        ),
    ]
}

fn event_strategy() -> impl Strategy<Value = V> {
    prop_oneof![
        1 => Just(V::Nil),
        4 => (any::<u64>(), "[ -~]{0,20}", prop::collection::vec(("[a-z]{1,8}", any_value_strategy()), 0..4)).prop_map(
            |(time, name, attributes)| {
                obj(vec![
                    ("time_unix_nano", V::Uint(time)),
                    ("name", V::Str(name)),
                    ("attributes", V::Map(attributes.into_iter().map(|(k, v)| (V::Str(k), v)).collect())),
                ])
            }
        ),
    ]
}

fn span_strategy() -> impl Strategy<Value = V> {
    let ids = (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<i64>(),
        any::<i64>(),
        0i64..2,
    );
    let names = ("[a-z-]{0,30}", "[a-z.]{0,20}", "[ -~]{0,60}", "[a-z]{0,8}");
    let maps = (
        string_map_strategy(),
        prop::collection::vec(("[a-z_.]{1,20}", any::<f64>()), 0..4),
        prop::collection::vec(("[a-z]{1,8}", prop::collection::vec(any::<u8>(), 0..16)), 0..3),
    );
    let lists = (
        prop::collection::vec(link_strategy(), 0..3),
        prop::collection::vec(event_strategy(), 0..3),
    );
    prop_oneof![
        1 => Just(V::Nil),
        6 => (ids, names, maps, lists).prop_map(
            |((trace_id, span_id, parent_id, start, duration, error), (service, name, resource, span_type), (meta, metrics, meta_struct), (links, events))| {
                obj(vec![
                    ("service", V::Str(service)),
                    ("name", V::Str(name)),
                    ("resource", V::Str(resource)),
                    ("trace_id", V::Uint(trace_id)),
                    ("span_id", V::Uint(span_id)),
                    ("parent_id", V::Uint(parent_id)),
                    ("start", V::Int(start)),
                    ("duration", V::Int(duration)),
                    ("error", V::Int(error)),
                    ("meta", meta),
                    ("metrics", V::Map(metrics.into_iter().map(|(k, v)| (V::Str(k), V::F64(v))).collect())),
                    ("type", V::Str(span_type)),
                    ("meta_struct", V::Map(meta_struct.into_iter().map(|(k, v)| (V::Str(k), V::Bin(v))).collect())),
                    ("span_links", V::Arr(links)),
                    ("span_events", V::Arr(events)),
                ])
            }
        ),
    ]
}

proptest! {
    /// Mutated and truncated payloads decode or fail, without panicking.
    ///
    /// The allocation bound checked here is a loose sanity ceiling, not a regression guard: it is far above what a
    /// decode needs. `nil_heavy_payloads_allocate_in_proportion_to_their_size` and
    /// `nil_entries_do_not_keep_preallocated_capacity` guard the allocation behavior.
    #[test]
    fn property_test_mutated_full_payloads_never_panic(
        chunks in prop::collection::vec(prop::collection::vec(span_strategy(), 0..4), 0..4),
        truncate_at in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 0..6),
    ) {
        let bytes = payload(chunks.clone());
        let decoded = decode_v04_payload(&bytes);
        prop_assert!(decoded.is_ok(), "the unmutated payload must decode: {:?}", decoded.err());
        prop_assert_eq!(decoded.unwrap().traces.len(), chunks.len());

        let mut mutated = bytes;
        for (index, mask) in &flips {
            let i = index.index(mutated.len());
            mutated[i] ^= mask;
        }
        if truncate {
            mutated.truncate(truncate_at.index(mutated.len() + 1));
        }
        // Any outcome is fine. The peak is only checked against a loose ceiling, to catch a runaway allocation.
        let (_, peak) = alloc_tracking::peak_during(|| decode_v04_payload(&mutated));
        let bound = 2 * read::MAX_PREALLOCATED_ELEMENTS * std::mem::size_of::<Span>() + 2048 * mutated.len();
        prop_assert!(peak <= bound, "peak {} bytes for {} bytes (bound {})", peak, mutated.len(), bound);
    }
}

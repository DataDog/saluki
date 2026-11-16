//! Tests for the classic tracer payload conversion.
//!
//! These port the reference's `converter_test.go` for what a JSON body can't express: chunk and payload fields, chunk
//! and payload tags, and span event values of every type. The span-level behaviors are also covered end to end by the
//! JSON decoder's fixtures, which come from the reference itself.

use datadog_protos::traces::{
    attribute_any_value::AttributeAnyValueType, attribute_array_value::AttributeArrayValueType, AttributeAnyValue,
    AttributeArray, AttributeArrayValue, Span as ClassicSpan, SpanEvent as ClassicSpanEvent, TraceChunk, TracerPayload,
};
use protobuf::{EnumOrUnknown, MessageField};
use saluki_core::data_model::event::trace::AttributeValue;

use super::{convert_tracer_payload, parse_decision_maker, parse_uint};

fn span(meta: &[(&str, &str)]) -> ClassicSpan {
    ClassicSpan {
        service: "test-service".into(),
        name: "test-span".into(),
        resource: "test-resource".into(),
        traceID: 123,
        spanID: 456,
        meta: meta.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..ClassicSpan::default()
    }
}

fn chunk(spans: Vec<ClassicSpan>) -> TraceChunk {
    TraceChunk {
        spans,
        ..TraceChunk::default()
    }
}

fn payload(chunks: Vec<TraceChunk>) -> TracerPayload {
    TracerPayload {
        chunks,
        ..TracerPayload::default()
    }
}

fn string(value: &str) -> AttributeValue {
    AttributeValue::String(value.into())
}

// `TestConvertToIdx_ChunkPriorityUsedWhenSpanHasNone` and `TestConvertToIdx_SpanPriorityTakesPrecedence`.
#[test]
fn the_chunk_priority_is_a_fallback_for_the_spans() {
    let mut fallback = chunk(vec![span(&[])]);
    fallback.priority = 2;
    let mut overridden = chunk(vec![span(&[])]);
    overridden.priority = 2;
    overridden.spans[0].metrics.insert("_sampling_priority_v1".into(), 1.0);
    let mut none = chunk(vec![span(&[])]);
    none.priority = -128;

    let decoded = convert_tracer_payload(payload(vec![fallback, overridden, none, chunk(vec![span(&[])])]), "");
    let priorities: Vec<_> = decoded.traces.iter().map(|trace| trace.priority).collect();
    // A chunk built without a priority has priority 0, which isn't "none".
    assert_eq!(priorities, [Some(2), Some(1), None, Some(0)]);
}

// `TestConvertToIdx_DecisionMakerFromChunkTags` and `TestConvertToIdx_SpanDecisionMakerTakesPrecedence`.
#[test]
fn the_chunk_decision_maker_tag_is_a_fallback_for_the_spans() {
    let mut from_tag = chunk(vec![span(&[])]);
    from_tag.tags.insert("_dd.p.dm".into(), "-3".into());
    let mut from_span = chunk(vec![span(&[("_dd.p.dm", "-4")])]);
    from_span.tags.insert("_dd.p.dm".into(), "-3".into());
    let mut invalid = chunk(vec![span(&[])]);
    invalid.tags.insert("_dd.p.dm".into(), "abc".into());

    let decoded = convert_tracer_payload(payload(vec![from_tag, from_span, invalid]), "");
    let mechanisms: Vec<_> = decoded.traces.iter().map(|trace| trace.sampling_mechanism).collect();
    assert_eq!(mechanisms, [3, 4, 0]);
    // The tag stays a chunk attribute.
    assert_eq!(decoded.traces[0].attributes.get("_dd.p.dm"), Some(&string("-3")));
}

// `TestConvertToIdx_ChunkOriginAndDroppedTracePreserved`.
#[test]
fn chunk_origin_and_dropped_trace_are_kept_and_a_span_origin_wins() {
    let mut kept = chunk(vec![span(&[])]);
    kept.origin = "lambda".into();
    kept.droppedTrace = true;
    let mut overridden = chunk(vec![span(&[]), span(&[("_dd.origin", "rum")])]);
    overridden.origin = "lambda".into();

    let decoded = convert_tracer_payload(payload(vec![kept, overridden]), "");
    assert_eq!(decoded.traces[0].origin, "lambda");
    assert!(decoded.traces[0].dropped_trace);
    assert_eq!(decoded.traces[1].origin, "rum");
    assert!(!decoded.traces[1].dropped_trace);
}

// `TestConvertToIdx_PayloadFieldsOverrideSpanFields` and `TestConvertToIdx_PayloadMetadataFieldsSet`.
#[test]
fn payload_fields_win_over_the_promoted_ones() {
    let spans = vec![span(&[
        ("_dd.hostname", "span-hostname"),
        ("env", "span-env"),
        ("version", "span-version"),
    ])];
    let tracer_payload = TracerPayload {
        hostname: "payload-hostname".into(),
        env: "payload-env".into(),
        appVersion: "payload-version".into(),
        containerID: "container-123".into(),
        languageName: "go".into(),
        languageVersion: "1.20".into(),
        tracerVersion: "v1.2.3".into(),
        runtimeID: "runtime-456".into(),
        ..payload(vec![chunk(spans.clone())])
    };
    let fields = &convert_tracer_payload(tracer_payload, "").traces[0].payload;
    assert_eq!(fields.hostname, "payload-hostname");
    assert_eq!(fields.env, "payload-env");
    assert_eq!(fields.app_version, "payload-version");
    assert_eq!(fields.container_id, "container-123");
    assert_eq!(fields.language_name, "go");
    assert_eq!(fields.language_version, "1.20");
    assert_eq!(fields.tracer_version, "v1.2.3");
    assert_eq!(fields.runtime_id, "runtime-456");

    let fields = &convert_tracer_payload(payload(vec![chunk(spans)]), "").traces[0].payload;
    assert_eq!(fields.hostname, "span-hostname");
    assert_eq!(fields.env, "span-env");
    assert_eq!(fields.app_version, "span-version");
}

// `TestConvertToIdx_ChunkAttributesConverted` and `TestConvertToIdx_GitCommitShaExtracted`.
#[test]
fn chunk_and_payload_tags_become_attributes_and_promoted_ones_win() {
    let mut with_tags = chunk(vec![span(&[
        ("_dd.git.commit.sha", "abc123"),
        ("_dd.apm_mode", "edge"),
    ])]);
    with_tags.tags.insert("chunk.tag".into(), "chunk.value".into());
    let tracer_payload = TracerPayload {
        tags: [
            ("payload.tag".to_string(), "payload.value".to_string()),
            ("_dd.git.commit.sha".to_string(), "from-payload".to_string()),
        ]
        .into_iter()
        .collect(),
        ..payload(vec![with_tags])
    };

    let decoded = convert_tracer_payload(tracer_payload, "");
    assert_eq!(
        decoded.traces[0].attributes.get("chunk.tag"),
        Some(&string("chunk.value"))
    );
    assert_eq!(
        decoded.payload_attributes.get("payload.tag"),
        Some(&string("payload.value"))
    );
    assert_eq!(
        decoded.payload_attributes.get("_dd.git.commit.sha"),
        Some(&string("abc123"))
    );
    assert_eq!(decoded.payload_attributes.get("_dd.apm_mode"), Some(&string("edge")));
}

// `TestConvertToIdx_NilChunksSkipped`, `TestConvertToIdx_EmptySpansChunksSkipped` and
// `TestConvertToIdx_DropsUnconvertibleChunks`.
#[test]
fn chunks_without_spans_are_dropped() {
    let decoded = convert_tracer_payload(payload(vec![chunk(vec![]), chunk(vec![span(&[])]), chunk(vec![])]), "");
    assert_eq!(decoded.traces.len(), 1);
    assert_eq!(decoded.traces[0].trace_id_low, 123);
}

// `TestConvertToIdx_ConvertedV1AttributeSet`.
#[test]
fn the_converted_from_attribute_is_only_set_when_named() {
    let decoded = convert_tracer_payload(payload(vec![chunk(vec![span(&[])])]), "");
    assert!(!decoded.traces[0].spans()[0].attributes.contains_key("_dd.convertedv1"));
    let decoded = convert_tracer_payload(payload(vec![chunk(vec![span(&[("_dd.convertedv1", "x")])])]), "v07-pb");
    assert_eq!(
        decoded.traces[0].spans()[0].attributes.get("_dd.convertedv1"),
        Some(&string("v07-pb"))
    );
}

fn any_value(value_type: i32) -> AttributeAnyValue {
    AttributeAnyValue {
        type_: EnumOrUnknown::from_i32(value_type),
        ..AttributeAnyValue::default()
    }
}

fn array_value(value_type: i32) -> AttributeArrayValue {
    AttributeArrayValue {
        type_: EnumOrUnknown::from_i32(value_type),
        ..AttributeArrayValue::default()
    }
}

// `TestConvertToIdx_SpanEventAttributeTypes`, `TestConvertToIdx_ArrayValueAllTypes` and
// `TestConvertArrayValue_NilAndNilElement`.
#[test]
fn span_event_attributes_convert_by_type_and_unknown_types_are_dropped() {
    let array = AttributeArray {
        values: vec![
            AttributeArrayValue {
                string_value: "s".into(),
                ..array_value(AttributeArrayValueType::STRING_VALUE as i32)
            },
            AttributeArrayValue {
                bool_value: true,
                ..array_value(AttributeArrayValueType::BOOL_VALUE as i32)
            },
            AttributeArrayValue {
                int_value: 7,
                ..array_value(AttributeArrayValueType::INT_VALUE as i32)
            },
            AttributeArrayValue {
                double_value: 1.5,
                ..array_value(AttributeArrayValueType::DOUBLE_VALUE as i32)
            },
            array_value(9),
        ],
        ..AttributeArray::default()
    };
    let attributes = [
        (
            "string",
            AttributeAnyValue {
                string_value: "v".into(),
                ..any_value(AttributeAnyValueType::STRING_VALUE as i32)
            },
        ),
        (
            "bool",
            AttributeAnyValue {
                bool_value: true,
                ..any_value(AttributeAnyValueType::BOOL_VALUE as i32)
            },
        ),
        (
            "int",
            AttributeAnyValue {
                int_value: -3,
                ..any_value(AttributeAnyValueType::INT_VALUE as i32)
            },
        ),
        (
            "double",
            AttributeAnyValue {
                double_value: 2.5,
                ..any_value(AttributeAnyValueType::DOUBLE_VALUE as i32)
            },
        ),
        (
            "array",
            AttributeAnyValue {
                array_value: MessageField::some(array),
                ..any_value(AttributeAnyValueType::ARRAY_VALUE as i32)
            },
        ),
        ("no_array", any_value(AttributeAnyValueType::ARRAY_VALUE as i32)),
        ("unknown", any_value(5)),
    ];
    let mut classic = span(&[]);
    classic.spanEvents = vec![ClassicSpanEvent {
        time_unix_nano: 42,
        name: "event".into(),
        attributes: attributes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ..ClassicSpanEvent::default()
    }];

    let decoded = convert_tracer_payload(payload(vec![chunk(vec![classic])]), "");
    let event = &decoded.traces[0].spans()[0].span_events()[0];
    assert_eq!((event.time_unix_nano(), event.name()), (42, "event"));
    let attributes = event.attributes();
    assert_eq!(attributes.len(), 6);
    assert_eq!(attributes.get("string"), Some(&string("v")));
    assert_eq!(attributes.get("bool"), Some(&AttributeValue::Bool(true)));
    assert_eq!(attributes.get("int"), Some(&AttributeValue::Int(-3)));
    assert_eq!(attributes.get("double"), Some(&AttributeValue::Float(2.5)));
    assert_eq!(
        attributes.get("array"),
        Some(&AttributeValue::Array(vec![
            string("s"),
            AttributeValue::Bool(true),
            AttributeValue::Int(7),
            AttributeValue::Float(1.5),
        ]))
    );
    assert_eq!(attributes.get("no_array"), Some(&AttributeValue::Array(vec![])));
}

#[test]
fn integers_parse_as_strconv_parse_uint() {
    assert_eq!(parse_uint(b"ff", 16, 64), Ok(255));
    assert_eq!(parse_uint(b"FF", 16, 64), Ok(255));
    assert_eq!(parse_uint(b"ffffffffffffffff", 16, 64), Ok(u64::MAX));
    assert_eq!(parse_uint(b"10000000000000000", 16, 64), Err(u64::MAX));
    assert_eq!(parse_uint(b"", 16, 64), Err(0));
    assert_eq!(parse_uint(b"+1", 10, 32), Err(0));
    assert_eq!(parse_uint(b"0x1", 16, 64), Err(0));
    assert_eq!(parse_uint(b"1_0", 10, 32), Err(0));
    assert_eq!(parse_uint(b"g", 16, 64), Err(0));
    assert_eq!(parse_uint(b"4294967295", 10, 32), Ok(u64::from(u32::MAX)));
    assert_eq!(parse_uint(b"4294967296", 10, 32), Err(u64::from(u32::MAX)));
    // The overflow is found before the invalid character, so it wins.
    assert_eq!(parse_uint(b"99999999999x", 10, 32), Err(u64::from(u32::MAX)));
    assert_eq!(parse_uint(b"1x", 10, 32), Err(0));

    // `ConvertToIdx` keeps whatever `ParseUint` returns, error or not.
    assert_eq!(parse_decision_maker("-4"), 4);
    assert_eq!(parse_decision_maker("--4"), 0);
    assert_eq!(parse_decision_maker("-99999999999"), u32::MAX);
}

#[test]
fn non_ascii_trace_id_high_bits_read_as_zero() {
    // A 32-byte `otel.trace_id` is cut at byte 16, here inside `é`, as Go slices it: the half then doesn't parse.
    let cases = [
        // 32 bytes, with `é` across bytes 15 and 16.
        ("otel.trace_id", "000000000000000é000000000000000"),
        ("otel.trace_id", "00000000000000é0000000000000000"),
        ("otel.trace_id", "é"),
        ("_dd.p.tid", "é"),
        ("_dd.p.tid", "\u{212a}"),
    ];
    for (key, value) in cases {
        let decoded = convert_tracer_payload(payload(vec![chunk(vec![span(&[(key, value)])])]), "");
        let trace = &decoded.traces[0];
        assert_eq!((trace.trace_id_high, trace.trace_id_low), (0, 123), "{key}: {value}");
    }
}

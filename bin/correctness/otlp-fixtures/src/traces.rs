//! OTLP traces fixtures.

use otlp_protos::opentelemetry::proto::{
    collector::trace::v1::ExportTraceServiceRequest,
    resource::v1::Resource,
    trace::v1::{span::SpanKind, ResourceSpans, ScopeSpans, Span},
};
use prost::Message as _;

use crate::common::{kv, string};

/// 2023-11-14T22:13:20Z, in nanoseconds.
const BASE_TIME_NANOS: u64 = 1_700_000_000_000_000_000;

/// The start of each span, relative to the previous one, in nanoseconds.
const SPAN_STEP_NANOS: u64 = 10_000_000;

/// The duration of each span, in nanoseconds.
const SPAN_DURATION_NANOS: u64 = 5_000_000;

/// The ID of the trace for `seed`: the seed as a big-endian integer.
fn trace_id(seed: u32) -> Vec<u8> {
    u128::from(seed).to_be_bytes().to_vec()
}

/// The ID of span `number`, counted from 1, in the trace for `seed`: the seed in the high 32 bits and the number in the
/// low 32 bits, as a big-endian integer.
fn span_id(seed: u32, number: u32) -> Vec<u8> {
    ((u64::from(seed) << 32) | u64::from(number)).to_be_bytes().to_vec()
}

/// Builds span `number`, counted from 1, in the trace for `seed`. Span 1 is the root, and every other span is its child.
fn span(seed: u32, number: u32) -> Span {
    let start = BASE_TIME_NANOS + u64::from(number - 1) * SPAN_STEP_NANOS;
    let kind = if number == 1 {
        SpanKind::Server
    } else {
        SpanKind::Internal
    };
    Span {
        trace_id: trace_id(seed),
        span_id: span_id(seed, number),
        parent_span_id: if number == 1 { Vec::new() } else { span_id(seed, 1) },
        name: format!("span-{}", number),
        kind: kind as i32,
        start_time_unix_nano: start,
        end_time_unix_nano: start + SPAN_DURATION_NANOS,
        ..Default::default()
    }
}

/// One trace of `span_count` spans from `service`.
///
/// The trace ID and span IDs derive from `seed`, so give each fixture that can reach the same destination its own seed.
/// The seed must not be zero, because an all-zero trace ID is invalid. The timestamps are fixed, so the same arguments
/// always give the same bytes.
pub fn trace_fixture(seed: u32, service: &str, span_count: u32) -> Vec<u8> {
    assert!(seed != 0, "the trace seed must not be zero");
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", string(service))],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: (1..=span_count).map(|number| span(seed, number)).collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::any_value::Value;

    use super::*;

    #[test]
    fn trace_fixture_round_trips() {
        let request = ExportTraceServiceRequest::decode(trace_fixture(7, "fixture-service", 3).as_slice()).unwrap();

        assert_eq!(request.resource_spans.len(), 1);
        let resource_spans = &request.resource_spans[0];

        let attributes = &resource_spans.resource.as_ref().unwrap().attributes;
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].key, "service.name");
        assert_eq!(
            attributes[0].value.as_ref().unwrap().value,
            Some(Value::StringValue("fixture-service".to_string()))
        );

        assert_eq!(resource_spans.scope_spans.len(), 1);
        let spans = &resource_spans.scope_spans[0].spans;
        let span_ids: Vec<&[u8]> = spans.iter().map(|span| span.span_id.as_slice()).collect();
        let expected: [&[u8]; 3] = [
            &[0, 0, 0, 7, 0, 0, 0, 1],
            &[0, 0, 0, 7, 0, 0, 0, 2],
            &[0, 0, 0, 7, 0, 0, 0, 3],
        ];
        assert_eq!(span_ids, expected);

        let expected_trace_id = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
        for span in spans {
            assert_eq!(span.trace_id, expected_trace_id);
        }
    }

    #[test]
    fn trace_fixture_links_children_to_the_root_span() {
        let request = ExportTraceServiceRequest::decode(trace_fixture(7, "fixture-service", 3).as_slice()).unwrap();
        let spans = &request.resource_spans[0].scope_spans[0].spans;

        assert!(spans[0].parent_span_id.is_empty());
        assert_eq!(spans[1].parent_span_id, spans[0].span_id);
        assert_eq!(spans[2].parent_span_id, spans[0].span_id);
    }

    #[test]
    fn trace_fixture_has_fixed_timestamps() {
        let request = ExportTraceServiceRequest::decode(trace_fixture(7, "fixture-service", 3).as_slice()).unwrap();
        let spans = &request.resource_spans[0].scope_spans[0].spans;

        let starts: Vec<u64> = spans.iter().map(|span| span.start_time_unix_nano).collect();
        assert_eq!(
            starts,
            [
                1_700_000_000_000_000_000,
                1_700_000_000_010_000_000,
                1_700_000_000_020_000_000
            ]
        );
        for span in spans {
            assert_eq!(span.end_time_unix_nano - span.start_time_unix_nano, 5_000_000);
        }
    }

    #[test]
    fn trace_fixture_ids_differ_by_seed() {
        let first = ExportTraceServiceRequest::decode(trace_fixture(1, "fixture-service", 3).as_slice()).unwrap();
        let second = ExportTraceServiceRequest::decode(trace_fixture(2, "fixture-service", 3).as_slice()).unwrap();
        let first = &first.resource_spans[0].scope_spans[0].spans;
        let second = &second.resource_spans[0].scope_spans[0].spans;

        for (a, b) in first.iter().zip(second) {
            assert_ne!(a.trace_id, b.trace_id);
            assert_ne!(a.span_id, b.span_id);
        }
    }

    #[test]
    #[should_panic(expected = "must not be zero")]
    fn trace_fixture_refuses_a_zero_seed() {
        trace_fixture(0, "fixture-service", 3);
    }

    #[test]
    fn trace_fixture_is_deterministic() {
        assert_eq!(
            trace_fixture(7, "fixture-service", 3),
            trace_fixture(7, "fixture-service", 3)
        );
    }
}

//! OTLP traces fixtures.

use otlp_protos::opentelemetry::proto::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::KeyValue,
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

/// Builds one trace of `span_count` spans from `service`.
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

/// How a probe span relates to its parent.
enum Parent {
    /// The span has no parent.
    Root,

    /// The parent is span `n` of the same trace, counted from 1.
    Span(u32),
}

/// A span of a probe fixture: what the trace builders need to build it.
struct ProbeSpan {
    /// The `service.name` of the resource that holds the span. It names one of the fixture's resources.
    service: &'static str,

    /// The OTLP name of the span.
    name: &'static str,
    kind: SpanKind,
    parent: Parent,
}

impl ProbeSpan {
    /// Builds a span with no parent.
    fn new(service: &'static str, name: &'static str, kind: SpanKind) -> Self {
        Self {
            service,
            name,
            kind,
            parent: Parent::Root,
        }
    }

    /// Makes the span a child of span `number` of its trace, counted from 1.
    fn child_of(mut self, number: u32) -> Self {
        self.parent = Parent::Span(number);
        self
    }

    /// Builds span `number`, counted from 1, in the trace for `seed`.
    fn build(&self, seed: u32, number: u32) -> Span {
        let start = BASE_TIME_NANOS + u64::from(number - 1) * SPAN_STEP_NANOS;
        Span {
            trace_id: trace_id(seed),
            span_id: span_id(seed, number),
            parent_span_id: match self.parent {
                Parent::Root => Vec::new(),
                Parent::Span(parent) => span_id(seed, parent),
            },
            name: self.name.to_string(),
            kind: self.kind as i32,
            start_time_unix_nano: start,
            end_time_unix_nano: start + SPAN_DURATION_NANOS,
            ..Default::default()
        }
    }
}

/// A resource of a probe fixture.
struct ProbeResource {
    service: &'static str,
    attributes: Vec<KeyValue>,
}

impl ProbeResource {
    /// Builds a resource with only the `service.name` attribute.
    fn new(service: &'static str) -> Self {
        Self {
            service,
            attributes: vec![kv("service.name", string(service))],
        }
    }
}

/// Builds a request from probe traces.
///
/// Each resource has one `ResourceSpans` that holds the spans of its service, so a trace that crosses services spans more
/// than one of them. Trace `n`, counted from 1, has the seed `seed_base + n`.
fn probe_request(resources: &[ProbeResource], seed_base: u32, traces: &[Vec<ProbeSpan>]) -> Vec<u8> {
    let mut spans: Vec<Vec<Span>> = resources.iter().map(|_| Vec::new()).collect();
    for (seed, trace) in (seed_base + 1..).zip(traces) {
        for (number, spec) in (1..).zip(trace) {
            let index = resources
                .iter()
                .position(|resource| resource.service == spec.service)
                .unwrap_or_else(|| panic!("no resource for the service {}", spec.service));
            spans[index].push(spec.build(seed, number));
        }
    }

    ExportTraceServiceRequest {
        resource_spans: resources
            .iter()
            .zip(spans)
            .map(|(resource, spans)| ResourceSpans {
                resource: Some(Resource {
                    attributes: resource.attributes.clone(),
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    spans,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
    }
    .encode_to_vec()
}

/// The seed of the traces of `defaults_fixture`: trace `n`, counted from 1, has the seed `DEFAULTS_SEED + n`.
const DEFAULTS_SEED: u32 = 0x0900;

/// Spans that leave top-level marking to the defaults: a root, a client, a span below a service boundary, an orphan,
/// and a root in another service.
///
/// - Trace 1 crosses services: a `SERVER` root and its `CLIENT` child in `probe-tl-a`, and an `INTERNAL` grandchild in
///   `probe-tl-b`.
/// - Trace 2 is an `INTERNAL` span in `probe-tl-a` whose parent, span 9, is never sent.
/// - Trace 3 is a `SERVER` root in `probe-ssi`.
pub fn defaults_fixture() -> Vec<u8> {
    let traces = vec![
        vec![
            ProbeSpan::new("probe-tl-a", "tl.root", SpanKind::Server),
            ProbeSpan::new("probe-tl-a", "tl.client", SpanKind::Client).child_of(1),
            ProbeSpan::new("probe-tl-b", "tl.b.internal", SpanKind::Internal).child_of(2),
        ],
        vec![ProbeSpan::new("probe-tl-a", "tl.orphan", SpanKind::Internal).child_of(9)],
        vec![ProbeSpan::new("probe-ssi", "ssi.root", SpanKind::Server)],
    ];
    probe_request(
        &[
            ProbeResource::new("probe-tl-a"),
            ProbeResource::new("probe-tl-b"),
            ProbeResource::new("probe-ssi"),
        ],
        DEFAULTS_SEED,
        &traces,
    )
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

    /// Returns each span of `defaults_fixture` as its service, name, span ID and parent span ID.
    fn defaults_spans() -> Vec<(String, String, Vec<u8>, Vec<u8>)> {
        let request = ExportTraceServiceRequest::decode(defaults_fixture().as_slice()).unwrap();
        request
            .resource_spans
            .iter()
            .flat_map(|resource_spans| {
                let service = match &resource_spans.resource.as_ref().unwrap().attributes[0].value {
                    Some(value) => match &value.value {
                        Some(Value::StringValue(service)) => service.clone(),
                        other => panic!("unexpected service {:?}", other),
                    },
                    None => panic!("no service"),
                };
                resource_spans.scope_spans[0].spans.iter().map(move |span| {
                    (
                        service.clone(),
                        span.name.clone(),
                        span.span_id.clone(),
                        span.parent_span_id.clone(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn defaults_fixture_groups_spans_by_service() {
        let spans = defaults_spans();
        let names = spans
            .iter()
            .map(|(service, name, _, _)| (service.as_str(), name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("probe-tl-a", "tl.root"),
                ("probe-tl-a", "tl.client"),
                ("probe-tl-a", "tl.orphan"),
                ("probe-tl-b", "tl.b.internal"),
                ("probe-ssi", "ssi.root"),
            ]
        );
    }

    #[test]
    fn defaults_fixture_links_spans_across_services_and_leaves_one_orphan() {
        let spans = defaults_spans();
        let id_of = |name: &str| spans.iter().find(|span| span.1 == name).unwrap().2.clone();
        let parent_of = |name: &str| spans.iter().find(|span| span.1 == name).unwrap().3.clone();

        assert!(parent_of("tl.root").is_empty());
        assert!(parent_of("ssi.root").is_empty());
        assert_eq!(parent_of("tl.client"), id_of("tl.root"));
        assert_eq!(parent_of("tl.b.internal"), id_of("tl.client"));

        let orphan_parent = parent_of("tl.orphan");
        assert!(!orphan_parent.is_empty());
        assert!(spans.iter().all(|span| span.2 != orphan_parent));
    }

    #[test]
    fn defaults_fixture_span_ids_are_unique() {
        let spans = defaults_spans();
        let ids = spans
            .iter()
            .map(|span| span.2.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), spans.len());
    }

    #[test]
    fn trace_fixture_is_deterministic() {
        assert_eq!(
            trace_fixture(7, "fixture-service", 3),
            trace_fixture(7, "fixture-service", 3)
        );
    }
}

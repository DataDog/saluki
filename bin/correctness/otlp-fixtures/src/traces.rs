//! OTLP traces fixtures.

use otlp_protos::opentelemetry::proto::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{AnyValue, KeyValue},
    resource::v1::Resource,
    trace::v1::{
        span::{Event, Link, SpanKind},
        status::StatusCode,
        ResourceSpans, ScopeSpans, Span, Status,
    },
};
use prost::Message as _;

use crate::common::{double, int, kv, string};

mod conversion;

pub use self::conversion::conversion_fixture;

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

    /// The span names itself as its parent.
    Itself,
}

/// A span of a probe fixture: what the trace builders need to build it.
struct ProbeSpan {
    /// The `service.name` of the resource that holds the span. It names one of the fixture's resources.
    service: &'static str,

    /// The OTLP name of the span.
    name: &'static str,

    /// The number of the OTLP span kind.
    kind: i32,
    parent: Parent,
    attributes: Vec<KeyValue>,
    trace_state: &'static str,
    status: Option<Status>,
    events: Vec<Event>,
    links: Vec<Link>,

    /// The high and low halves of the trace ID. A span without them has the ID of its trace.
    trace_id: Option<(u64, u64)>,

    /// Whether the span ID is zero, which is not valid.
    zero_span_id: bool,

    /// The start of the span, in nanoseconds. A span without one starts at the base time plus one step for each span
    /// before it in its trace.
    start: Option<u64>,

    /// The duration of the span, in nanoseconds.
    duration: u64,
}

impl ProbeSpan {
    /// Builds a span with no parent, no attributes and the default duration.
    fn new(service: &'static str, name: &'static str, kind: SpanKind) -> Self {
        Self {
            service,
            name,
            kind: kind as i32,
            parent: Parent::Root,
            attributes: Vec::new(),
            trace_state: "",
            status: None,
            events: Vec::new(),
            links: Vec::new(),
            trace_id: None,
            zero_span_id: false,
            start: None,
            duration: SPAN_DURATION_NANOS,
        }
    }

    /// Gives the span the span kind with number `kind`, which can be one that OTLP does not define.
    fn with_kind_number(mut self, kind: i32) -> Self {
        self.kind = kind;
        self
    }

    /// Makes the span a child of span `number` of its trace, counted from 1.
    fn child_of(mut self, number: u32) -> Self {
        self.parent = Parent::Span(number);
        self
    }

    fn with_parent(mut self, parent: Parent) -> Self {
        self.parent = parent;
        self
    }

    fn with_attribute(mut self, key: &str, value: AnyValue) -> Self {
        self.attributes.push(kv(key, value));
        self
    }

    /// Adds a string attribute for each of `attributes`.
    fn with_strings(self, attributes: &[(&str, &str)]) -> Self {
        attributes
            .iter()
            .fold(self, |span, (key, value)| span.with_attribute(key, string(value)))
    }

    /// Gives the span the status `ERROR`.
    fn failed(self) -> Self {
        self.with_status(StatusCode::Error, "probe error")
    }

    /// Gives the span a status with `code` and `message`.
    fn with_status(mut self, code: StatusCode, message: &str) -> Self {
        self.status = Some(Status {
            message: message.to_string(),
            code: code as i32,
        });
        self
    }

    fn with_event(mut self, event: Event) -> Self {
        self.events.push(event);
        self
    }

    fn with_link(mut self, link: Link) -> Self {
        self.links.push(link);
        self
    }

    /// Gives the span the trace ID with the high half `high` and the low half `low`, not the ID of its trace.
    fn with_trace_id(mut self, high: u64, low: u64) -> Self {
        self.trace_id = Some((high, low));
        self
    }

    /// Gives the span the span ID zero.
    fn with_zero_span_id(mut self) -> Self {
        self.zero_span_id = true;
        self
    }

    fn with_trace_state(mut self, trace_state: &'static str) -> Self {
        self.trace_state = trace_state;
        self
    }

    /// Starts the span at `start` nanoseconds.
    fn starting_at(mut self, start: u64) -> Self {
        self.start = Some(start);
        self
    }

    /// Builds span `number`, counted from 1, in the trace for `seed`.
    fn build(&self, seed: u32, number: u32) -> Span {
        let start = self
            .start
            .unwrap_or(BASE_TIME_NANOS + u64::from(number - 1) * SPAN_STEP_NANOS);
        Span {
            trace_id: match self.trace_id {
                Some((high, low)) => ((u128::from(high) << 64) | u128::from(low)).to_be_bytes().to_vec(),
                None => trace_id(seed),
            },
            span_id: if self.zero_span_id {
                vec![0; 8]
            } else {
                span_id(seed, number)
            },
            parent_span_id: match self.parent {
                Parent::Root => Vec::new(),
                Parent::Span(parent) => span_id(seed, parent),
                Parent::Itself => span_id(seed, number),
            },
            trace_state: self.trace_state.to_string(),
            name: self.name.to_string(),
            kind: self.kind,
            start_time_unix_nano: start,
            end_time_unix_nano: start + self.duration,
            attributes: self.attributes.clone(),
            events: self.events.clone(),
            links: self.links.clone(),
            status: self.status.clone(),
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

    /// Builds a resource with no attributes. Its spans name it as their `service`, which is not a `service.name`.
    fn without_service_name(id: &'static str) -> Self {
        Self {
            service: id,
            attributes: Vec::new(),
        }
    }

    fn with_attribute(mut self, key: &str, value: AnyValue) -> Self {
        self.attributes.push(kv(key, value));
        self
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

/// 2100-01-01T00:00:00Z, in nanoseconds.
const YEAR_2100_NANOS: u64 = 4_102_444_800_000_000_000;

/// The seed of the traces of `stats_fixture`: trace `n`, counted from 1, has the seed `STATS_SEED + n`.
const STATS_SEED: u32 = 0x0a00;

/// The service that most of the spans of `stats_fixture` belong to.
const STATS_SERVICE: &str = "probe-stats";

/// The service of the resource that says a client computed its stats.
const CLIENT_COMPUTED_SERVICE: &str = "probe-stats-client";

/// The service of the resource that marks every span measured.
const MEASURED_SERVICE: &str = "probe-stats-measured";

/// The fewest microseconds in the duration of a span of `stats_fixture`.
const MIN_PRIME_MICROS: u64 = 2_000;

/// A span in `STATS_SERVICE`, with the operation name `name` and the same OTLP name, so that the span has its own stats
/// group.
fn stats_span(name: &'static str, kind: SpanKind) -> ProbeSpan {
    ProbeSpan::new(STATS_SERVICE, name, kind).with_attribute("operation.name", string(name))
}

/// A `SERVER` span in `STATS_SERVICE` for a request that got `status_code`.
fn http_span(name: &'static str, status_code: i64) -> ProbeSpan {
    stats_span(name, SpanKind::Server)
        .with_attribute("http.request.method", string("GET"))
        .with_attribute("http.route", string("/status"))
        .with_attribute("http.response.status_code", int(status_code))
}

/// A `CLIENT` span in `STATS_SERVICE` that calls a gRPC method and got `status_code`.
fn grpc_span(name: &'static str, status_code: AnyValue) -> ProbeSpan {
    stats_span(name, SpanKind::Client)
        .with_attribute("rpc.system", string("grpc"))
        .with_attribute("rpc.service", string("probe.Users"))
        .with_attribute("rpc.method", string("GetUser"))
        .with_attribute("rpc.grpc.status_code", status_code)
}

/// A `CLIENT` span in `STATS_SERVICE`, with `peer_attributes` as its attributes.
fn peer_span(name: &'static str, peer_attributes: &[(&str, &str)]) -> ProbeSpan {
    peer_attributes
        .iter()
        .fold(stats_span(name, SpanKind::Client), |span, (key, value)| {
            span.with_attribute(key, string(value))
        })
}

/// The traces of `stats_fixture`.
///
/// Each stats dimension has its own operation name, `otlpprobe.tstat.<dimension>`, so that a difference names the
/// dimension that causes it. A trace with one span is a root.
fn stats_traces() -> Vec<Vec<ProbeSpan>> {
    let mut traces = Vec::new();

    // HTTP status code: one group for each of 200, 404 and 500. Three spans share the 200 group.
    for status_code in [200, 200, 200, 404, 500] {
        let span = http_span("otlpprobe.tstat.http_status", status_code);
        traces.push(vec![if status_code == 500 { span.failed() } else { span }]);
    }

    // HTTP method and endpoint split no group: one group holds all three spans, whatever their method and route.
    for (method, route) in [("GET", "/a"), ("POST", "/b"), ("PUT", "/c")] {
        traces.push(vec![stats_span("otlpprobe.tstat.http_split", SpanKind::Server)
            .with_attribute("resource.name", string("otlpprobe.tstat.http_split"))
            .with_attribute("http.request.method", string(method))
            .with_attribute("http.route", string(route))]);
    }

    // gRPC status code: an integer and a status name. Two groups: 14 for two spans, and 4 for the name.
    for status_code in [int(14), int(14), string("DeadlineExceeded")] {
        traces.push(vec![grpc_span("otlpprobe.tstat.grpc", status_code)]);
    }

    // Database type: `db.type` is a source of a peer tag, and neither side has a stats dimension for it.
    traces.push(vec![
        stats_span("otlpprobe.tstat.db_type", SpanKind::Client).with_attribute("db.type", string("sql"))
    ]);

    // Span kind: one root for each kind, so each has a group.
    for kind in [
        SpanKind::Unspecified,
        SpanKind::Internal,
        SpanKind::Server,
        SpanKind::Client,
        SpanKind::Producer,
        SpanKind::Consumer,
    ] {
        traces.push(vec![stats_span("otlpprobe.tstat.kind", kind)]);
    }

    // Which spans count. Every span is a child of the `SERVER` root of a trace, in the same service.
    // - `internal_child` is neither top-level nor measured, so it has no group.
    // - `partial` is a snapshot of a long span, so it has no group, and `partial_control` is the same span without
    //   the snapshot marker.
    // - `measured_string` and `measured_int` set `_dd.measured` as a string and as an integer.
    // - `top_level_attribute` sets `_dd.top_level`.
    traces.push(vec![
        stats_span("otlpprobe.tstat.children", SpanKind::Server),
        stats_span("otlpprobe.tstat.internal_child", SpanKind::Internal).child_of(1),
        stats_span("otlpprobe.tstat.partial", SpanKind::Client)
            .child_of(1)
            .with_attribute("_dd.partial_version", int(0)),
        stats_span("otlpprobe.tstat.partial_control", SpanKind::Client).child_of(1),
        stats_span("otlpprobe.tstat.measured_string", SpanKind::Internal)
            .child_of(1)
            .with_attribute("_dd.measured", string("1")),
        stats_span("otlpprobe.tstat.measured_int", SpanKind::Internal)
            .child_of(1)
            .with_attribute("_dd.measured", int(1)),
        stats_span("otlpprobe.tstat.top_level_attribute", SpanKind::Internal)
            .child_of(1)
            .with_attribute("_dd.top_level", int(1)),
    ]);

    // `_dd.measured` on the resource, not the span. The resource marks every span in it measured.
    traces.push(vec![
        ProbeSpan::new(
            MEASURED_SERVICE,
            "otlpprobe.tstat.measured_resource_root",
            SpanKind::Server,
        )
        .with_attribute("operation.name", string("otlpprobe.tstat.measured_resource_root")),
        ProbeSpan::new(
            MEASURED_SERVICE,
            "otlpprobe.tstat.measured_resource",
            SpanKind::Internal,
        )
        .with_attribute("operation.name", string("otlpprobe.tstat.measured_resource"))
        .child_of(1),
    ]);

    // Synthetics: the origin of the trace marks its groups.
    traces.push(vec![stats_span("otlpprobe.tstat.synthetics", SpanKind::Server)
        .with_attribute("_dd.origin", string("synthetics-browser"))]);
    traces.push(vec![stats_span("otlpprobe.tstat.synthetics_off", SpanKind::Server)]);
    // The origin is on the root, and the span sent first has none.
    traces.push(vec![
        stats_span("otlpprobe.tstat.synthetics_first", SpanKind::Internal).child_of(2),
        stats_span("otlpprobe.tstat.synthetics_root", SpanKind::Server)
            .with_attribute("_dd.origin", string("synthetics-browser")),
    ]);

    // `is_trace_root`: the same name and resource on a root and on a `SERVER` child, so two groups.
    traces.push(vec![
        stats_span("otlpprobe.tstat.is_trace_root", SpanKind::Server),
        stats_span("otlpprobe.tstat.is_trace_root", SpanKind::Server).child_of(1),
    ]);

    // The span is its own parent.
    traces.push(vec![
        stats_span("otlpprobe.tstat.self_parent", SpanKind::Server).with_parent(Parent::Itself)
    ]);

    // Peer tags: in range, then the edges. A `CLIENT` span carries them, and a `SERVER` span does not.
    traces.push(vec![peer_span(
        "otlpprobe.tstat.peer",
        &[("peer.service", "probe-peer"), ("peer.hostname", "probe-peer.example")],
    )]);
    // An address becomes `blocked-ip-address`, and a loopback address stays.
    for hostname in ["10.1.2.3:5432", "127.0.0.1"] {
        traces.push(vec![peer_span(
            "otlpprobe.tstat.peer_address",
            &[("peer.hostname", hostname)],
        )]);
    }
    traces.push(vec![peer_span(
        "otlpprobe.tstat.peer_service",
        &[("peer.service", "My Peer!!")],
    )]);
    // Two keys, where one is the other with a suffix.
    traces.push(vec![peer_span(
        "otlpprobe.tstat.peer_order",
        &[
            ("messaging.destination", "probe-queue"),
            ("messaging.destination.name", "probe-queue"),
        ],
    )]);
    traces.push(vec![stats_span(
        "otlpprobe.tstat.peer_base_service",
        SpanKind::Internal,
    )
    .with_attribute("_dd.base_service", string("probe-base"))]);
    traces.push(vec![stats_span("otlpprobe.tstat.peer_server", SpanKind::Server)
        .with_attribute("peer.service", string("probe-peer"))]);

    // Weights: the sample rate of the root multiplies the counts of every group of the trace.
    traces.push(vec![
        stats_span("otlpprobe.tstat.weight", SpanKind::Server).with_attribute("_sample_rate", double(0.25)),
        stats_span("otlpprobe.tstat.weight_child", SpanKind::Client).child_of(1),
    ]);
    traces.push(vec![
        stats_span("otlpprobe.tstat.weight_tracestate", SpanKind::Server).with_trace_state("ot=th:8")
    ]);

    // The `datadog.*` attributes override the fields of the span.
    traces.push(vec![stats_span("otlpprobe.tstat.override", SpanKind::Server)
        .with_attribute("datadog.resource", string("otlpprobe.override.resource"))]);

    // The span ends in the year 2100.
    traces.push(vec![
        stats_span("otlpprobe.tstat.future", SpanKind::Server).starting_at(YEAR_2100_NANOS)
    ]);

    // The resource says that a client computed the stats of its spans.
    traces.push(vec![
        ProbeSpan::new(
            CLIENT_COMPUTED_SERVICE,
            "otlpprobe.tstat.client_computed",
            SpanKind::Server,
        )
        .with_attribute("operation.name", string("otlpprobe.tstat.client_computed")),
        ProbeSpan::new(
            CLIENT_COMPUTED_SERVICE,
            "otlpprobe.tstat.client_computed_child",
            SpanKind::Client,
        )
        .with_attribute("operation.name", string("otlpprobe.tstat.client_computed_child"))
        .child_of(1),
    ]);

    // Every span gets its own duration, a prime number of microseconds, so that the duration of a group names the spans
    // in it.
    let primes = (MIN_PRIME_MICROS..).filter(|&n| (2..).take_while(|d| d * d <= n).all(|d| n % d != 0));
    for (span, micros) in traces.iter_mut().flatten().zip(primes) {
        span.duration = micros * 1_000;
    }
    traces
}

/// Spans of the services `probe-stats`, `probe-stats-measured` and `probe-stats-client`, built so that each stats
/// dimension has its own operation name.
///
/// The `probe-stats-client` resource says that the client computed the stats of its spans. The `probe-stats-measured`
/// resource marks its spans measured.
pub fn stats_fixture() -> Vec<u8> {
    probe_request(
        &[
            ProbeResource::new(STATS_SERVICE),
            ProbeResource::new(MEASURED_SERVICE).with_attribute("_dd.measured", int(1)),
            ProbeResource::new(CLIENT_COMPUTED_SERVICE).with_attribute("_dd.stats_computed", string("true")),
        ],
        STATS_SEED,
        &stats_traces(),
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

    /// Returns each span of `stats_fixture`, with the resource that holds it.
    fn stats_spans() -> Vec<(Resource, Span)> {
        let request = ExportTraceServiceRequest::decode(stats_fixture().as_slice()).unwrap();
        request
            .resource_spans
            .iter()
            .flat_map(|resource_spans| {
                let resource = resource_spans.resource.clone().unwrap();
                resource_spans.scope_spans[0]
                    .spans
                    .iter()
                    .map(move |span| (resource.clone(), span.clone()))
            })
            .collect()
    }

    fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a Value> {
        attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .and_then(|attribute| attribute.value.as_ref())
            .and_then(|value| value.value.as_ref())
    }

    fn service_of(resource: &Resource) -> String {
        match attribute(&resource.attributes, "service.name") {
            Some(Value::StringValue(service)) => service.clone(),
            other => panic!("unexpected service {:?}", other),
        }
    }

    fn stats_span_named(name: &str) -> (Resource, Span) {
        let mut matching = stats_spans().into_iter().filter(|(_, span)| {
            attribute(&span.attributes, "operation.name") == Some(&Value::StringValue(name.into()))
        });
        let found = matching.next().unwrap_or_else(|| panic!("no span named {}", name));
        assert!(matching.next().is_none(), "more than one span named {}", name);
        found
    }

    #[test]
    fn stats_fixture_counts_match_the_case() {
        // The `otlp-traces-stats` case expects these counts of each side.
        let spans = stats_spans();

        let traces = spans
            .iter()
            .map(|(_, span)| span.trace_id.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(traces.len(), 37);
        let in_service = |service: &str| {
            spans
                .iter()
                .filter(|(resource, _)| service_of(resource) == service)
                .count()
        };
        assert_eq!(in_service(STATS_SERVICE), 44);
        assert_eq!(in_service(MEASURED_SERVICE), 2);
        assert_eq!(in_service(CLIENT_COMPUTED_SERVICE), 2);
    }

    #[test]
    fn stats_fixture_span_ids_are_unique() {
        let spans = stats_spans();

        let ids = spans
            .iter()
            .map(|(_, span)| span.span_id.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), spans.len());
    }

    #[test]
    fn stats_fixture_gives_each_span_its_own_prime_duration_in_microseconds() {
        let spans = stats_spans();

        let mut micros = spans
            .iter()
            .map(|(_, span)| {
                let nanos = span.end_time_unix_nano - span.start_time_unix_nano;
                assert_eq!(nanos % 1_000, 0);
                nanos / 1_000
            })
            .collect::<Vec<_>>();
        for &value in &micros {
            assert!(value >= MIN_PRIME_MICROS);
            assert!(
                (2..).take_while(|d| d * d <= value).all(|d| value % d != 0),
                "{value} is not prime"
            );
        }
        micros.sort_unstable();
        micros.dedup();
        assert_eq!(micros.len(), spans.len());
    }

    #[test]
    fn stats_fixture_has_the_edge_spans() {
        let (_, own_parent) = stats_span_named("otlpprobe.tstat.self_parent");
        assert_eq!(own_parent.parent_span_id, own_parent.span_id);

        let (_, future) = stats_span_named("otlpprobe.tstat.future");
        assert!(future.start_time_unix_nano >= YEAR_2100_NANOS);

        let (_, tracestate) = stats_span_named("otlpprobe.tstat.weight_tracestate");
        assert_eq!(tracestate.trace_state, "ot=th:8");

        let (_, failed) = stats_spans()
            .into_iter()
            .find(|(_, span)| attribute(&span.attributes, "http.response.status_code") == Some(&Value::IntValue(500)))
            .unwrap();
        assert_eq!(failed.status.unwrap().code, StatusCode::Error as i32);
    }

    #[test]
    fn stats_fixture_marks_two_resources() {
        let spans = stats_spans();
        let resource_of = |service: &str| {
            spans
                .iter()
                .map(|(resource, _)| resource)
                .find(|resource| service_of(resource) == service)
                .unwrap()
        };

        assert_eq!(
            attribute(&resource_of(CLIENT_COMPUTED_SERVICE).attributes, "_dd.stats_computed"),
            Some(&Value::StringValue("true".to_string()))
        );
        assert_eq!(
            attribute(&resource_of(MEASURED_SERVICE).attributes, "_dd.measured"),
            Some(&Value::IntValue(1))
        );
        assert_eq!(
            attribute(&resource_of(STATS_SERVICE).attributes, "_dd.stats_computed"),
            None
        );
    }
}

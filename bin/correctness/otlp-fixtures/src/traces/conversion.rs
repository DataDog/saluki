//! The fixture of the `otlp-traces-conversion` case: one probe for each way that a span converts.

use otlp_protos::opentelemetry::proto::{
    common::v1::{AnyValue, KeyValue},
    trace::v1::{
        span::{Event, Link, SpanKind},
        status::StatusCode,
    },
};

use super::{probe_request, ProbeResource, ProbeSpan, BASE_TIME_NANOS};
use crate::common::{array, boolean, bytes, double, empty, int, kv, map, string};

/// The seed of the traces of `conversion_fixture`: trace `n`, counted from 1, has the seed `CONVERSION_SEED + n`.
const CONVERSION_SEED: u32 = 0x0b00;

/// The service of the resource that most of the probes belong to.
const SERVICE: &str = "probe-conv";

/// The service of the resource with a `deployment.environment.name`, a `service.version` and SDK attributes.
const META_SERVICE: &str = "probe-conv-meta";

/// The service of the resource with the same attributes as `META_SERVICE`, and a span with an environment and a version
/// of its own.
const META_SPAN_SERVICE: &str = "probe-conv-meta-span";

/// The service of the resource with no environment or version, and a span that has them.
const SPAN_ENV_SERVICE: &str = "probe-conv-span-env";

/// The service of the resource with a span that has `datadog.env` and `datadog.version`.
const DATADOG_ENV_SERVICE: &str = "probe-conv-dd-env";

/// The service of the resource with a `host.name`.
const HOST_SERVICE: &str = "probe-conv-host";

/// The service of the resource with a `datadog.host.name`.
const DATADOG_HOST_SERVICE: &str = "probe-conv-dd-host";

/// The service of the resource that a Fargate task holds.
const FARGATE_SERVICE: &str = "probe-conv-fargate";

/// The service of the resource with `datadog.*` attributes.
const DATADOG_RESOURCE_SERVICE: &str = "probe-conv-dd-resource";

/// The id of the resource with no `service.name`.
const UNNAMED_RESOURCE: &str = "probe-conv-unnamed";

/// The service of the spans with trace IDs that share their low half.
const HIGH_SERVICE: &str = "probe-conv-high";

/// The service of the spans that both sides drop for a zero trace ID or span ID, and of their controls.
const ZERO_SERVICE: &str = "probe-conv-zero";

/// The service that a `service.name` span attribute gives a span of `SERVICE`.
const SPAN_ATTRIBUTE_SERVICE: &str = "probe-conv-span-service";

/// The service that a `datadog.service` span attribute gives a span of `SERVICE`.
const DATADOG_SPAN_SERVICE: &str = "probe-conv-dd-span-service";

/// The low half of the trace IDs of `high_traces`, and the first of the low halves of its other traces.
const HIGH_LOW_HALF: u64 = 0x0b51;

/// The length of an attribute value that the limit of 25000 bytes cuts.
const LONG_VALUE_LEN: usize = 25_001;

/// The length of an attribute key that the limit of 200 bytes cuts.
const LONG_KEY_LEN: usize = 201;

/// The time of an event or link of `ProbeSpan`, in nanoseconds: `step` milliseconds after the base time.
fn at(step: u64) -> u64 {
    BASE_TIME_NANOS + step * 1_000_000
}

/// A span in `SERVICE` that is the root of its trace.
fn probe(name: &'static str, kind: SpanKind) -> ProbeSpan {
    ProbeSpan::new(SERVICE, name, kind)
}

/// A span in `SERVICE`, named `name`, with the string attributes `attributes`.
fn named(name: &'static str, kind: SpanKind, attributes: &[(&str, &str)]) -> Vec<ProbeSpan> {
    vec![probe(name, kind).with_strings(attributes)]
}

/// A `SERVER` span in `SERVICE` for a request that got the status code `status_code`, set under the key `key`.
///
/// The route is `/conv/status/<variant>`, so that the resource, which an HTTP span derives from its method and route,
/// names the probe.
fn status_span(name: &'static str, key: &str, status_code: AnyValue) -> Vec<ProbeSpan> {
    vec![probe(name, SpanKind::Server)
        .with_strings(&[("http.request.method", "GET"), ("http.route", &status_route(name))])
        .with_attribute(key, status_code)]
}

/// The route of the status probe `name`.
fn status_route(name: &str) -> String {
    format!("/conv/status/{}", name.rsplit('.').next().unwrap())
}

fn event(name: &str, time: u64, attributes: Vec<KeyValue>) -> Event {
    Event {
        time_unix_nano: time,
        name: name.to_string(),
        attributes,
        dropped_attributes_count: 0,
    }
}

fn link(trace_id: (u64, u64), span_id: u64, trace_state: &str, flags: u32, attributes: Vec<KeyValue>) -> Link {
    Link {
        trace_id: ((u128::from(trace_id.0) << 64) | u128::from(trace_id.1))
            .to_be_bytes()
            .to_vec(),
        span_id: span_id.to_be_bytes().to_vec(),
        trace_state: trace_state.to_string(),
        attributes,
        dropped_attributes_count: 0,
        flags,
    }
}

/// The traces whose names and resources derive from the span kind and from the semantic conventions.
///
/// Each span has the OTLP name `otlpprobe.conv.naming.<variant>`, which is the resource of a span that no convention
/// names. A convention that sets a resource has a route, an operation or a statement of its own, so that the resource
/// names the probe.
fn naming_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        // HTTP: the method and, for a server, the route. A client has the URL template instead.
        named(
            "otlpprobe.conv.naming.http_server",
            SpanKind::Server,
            &[("http.request.method", "GET"), ("http.route", "/conv/http-server")],
        ),
        named(
            "otlpprobe.conv.naming.http_client_template",
            SpanKind::Client,
            &[("http.request.method", "POST"), ("url.template", "/conv/{id}/client")],
        ),
        named(
            "otlpprobe.conv.naming.http_client_plain",
            SpanKind::Client,
            &[("http.request.method", "GET"), ("http.route", "/conv/http-client")],
        ),
        named(
            "otlpprobe.conv.naming.http_other_method",
            SpanKind::Server,
            &[("http.request.method", "_OTHER"), ("http.route", "/conv/other-method")],
        ),
        named(
            "otlpprobe.conv.naming.http_legacy_method",
            SpanKind::Server,
            &[("http.method", "PUT"), ("http.route", "/conv/legacy-method")],
        ),
        // Messaging.
        named(
            "otlpprobe.conv.naming.messaging",
            SpanKind::Producer,
            &[
                ("messaging.system", "kafka"),
                ("messaging.operation", "publish"),
                ("messaging.destination.name", "conv-topic"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.messaging_no_destination",
            SpanKind::Consumer,
            &[("messaging.system", "kafka"), ("messaging.operation", "receive")],
        ),
        named(
            "otlpprobe.conv.naming.messaging_internal",
            SpanKind::Internal,
            &[("messaging.system", "kafka"), ("messaging.operation", "process")],
        ),
        // Function as a service.
        named(
            "otlpprobe.conv.naming.faas_client",
            SpanKind::Client,
            &[("faas.invoked_provider", "aws"), ("faas.invoked_name", "conv-function")],
        ),
        named(
            "otlpprobe.conv.naming.faas_server",
            SpanKind::Server,
            &[("faas.trigger", "pubsub")],
        ),
        // RPC.
        named(
            "otlpprobe.conv.naming.aws_api",
            SpanKind::Client,
            &[
                ("rpc.system", "aws-api"),
                ("rpc.service", "S3"),
                ("rpc.method", "GetObject"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.grpc_server",
            SpanKind::Server,
            &[
                ("rpc.system", "grpc"),
                ("rpc.service", "conv.Users"),
                ("rpc.method", "GetUser"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.grpc_client",
            SpanKind::Client,
            &[("rpc.system", "grpc"), ("rpc.method", "Ping")],
        ),
        // GraphQL.
        named(
            "otlpprobe.conv.naming.graphql",
            SpanKind::Server,
            &[
                ("graphql.operation.type", "query"),
                ("graphql.operation.name", "GetConv"),
            ],
        ),
        // Databases: the legacy keys, the keys of semantic conventions 1.30, which name the system and the statement
        // differently, and the precedence of a statement over a query text.
        named(
            "otlpprobe.conv.naming.db_legacy",
            SpanKind::Client,
            &[
                ("db.system", "postgresql"),
                ("db.statement", "SELECT id FROM conv_legacy"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.db_modern",
            SpanKind::Client,
            &[
                ("db.system.name", "postgresql"),
                ("db.query.text", "SELECT id FROM conv_modern"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.db_statement_first",
            SpanKind::Client,
            &[
                ("db.system", "postgresql"),
                ("db.statement", "SELECT id FROM conv_statement"),
                ("db.query.text", "SELECT id FROM conv_query_text"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.db_namespace",
            SpanKind::Client,
            &[
                ("db.system", "mysql"),
                ("db.statement", "SELECT id FROM conv_namespace"),
                ("db.namespace", "conv_db"),
            ],
        ),
        // Without a convention, the span kind and the network protocol name the operation.
        named(
            "otlpprobe.conv.naming.protocol_server",
            SpanKind::Server,
            &[("network.protocol.name", "amqp")],
        ),
        named(
            "otlpprobe.conv.naming.protocol_client",
            SpanKind::Client,
            &[("network.protocol.name", "amqp")],
        ),
        named("otlpprobe.conv.naming.kind_server", SpanKind::Server, &[]),
        named("otlpprobe.conv.naming.kind_client", SpanKind::Client, &[]),
        named("otlpprobe.conv.naming.kind_producer", SpanKind::Producer, &[]),
        named("otlpprobe.conv.naming.kind_consumer", SpanKind::Consumer, &[]),
        named("otlpprobe.conv.naming.kind_internal", SpanKind::Internal, &[]),
        named("otlpprobe.conv.naming.kind_unspecified", SpanKind::Unspecified, &[]),
        vec![probe("otlpprobe.conv.naming.kind_unknown", SpanKind::Unspecified).with_kind_number(99)],
        // The Datadog conventions override the derived fields. The first span sets all of them, and the second sets
        // two of them on an HTTP server span.
        named(
            "otlpprobe.conv.naming.override_all",
            SpanKind::Server,
            &[
                ("operation.name", "conv.operation"),
                ("resource.name", "conv resource"),
                ("span.type", "conv-type"),
                ("http.request.method", "GET"),
                ("http.route", "/conv/override-all"),
            ],
        ),
        named(
            "otlpprobe.conv.naming.override_http",
            SpanKind::Server,
            &[
                ("operation.name", "conv.http.operation"),
                ("http.request.method", "GET"),
                ("http.route", "/conv/override-http"),
            ],
        ),
        // A `service.name` span attribute wins over the one of the resource.
        named(
            "otlpprobe.conv.naming.service_attribute",
            SpanKind::Internal,
            &[("service.name", SPAN_ATTRIBUTE_SERVICE)],
        ),
    ]
}

/// The traces whose attributes have each type that OTLP can hold.
fn typed_attribute_traces() -> Vec<Vec<ProbeSpan>> {
    let probe = |name| probe(name, SpanKind::Internal);
    vec![
        vec![probe("otlpprobe.conv.typed.numbers")
            .with_attribute("conv.int", int(42))
            .with_attribute("conv.negative", int(-7))
            .with_attribute("conv.int_max", int(i64::MAX))
            .with_attribute("conv.double", double(1.5))],
        vec![probe("otlpprobe.conv.typed.bools")
            .with_attribute("conv.true", boolean(true))
            .with_attribute("conv.false", boolean(false))],
        vec![probe("otlpprobe.conv.typed.bytes").with_attribute("conv.bytes", bytes(&[0, 1, 2, 255]))],
        vec![probe("otlpprobe.conv.typed.arrays")
            .with_attribute("conv.strings", array(vec![string("a"), string("b")]))
            .with_attribute("conv.mixed", array(vec![int(1), string("x"), boolean(true)]))
            .with_attribute("conv.empty_array", array(Vec::new()))],
        vec![probe("otlpprobe.conv.typed.kvlist").with_attribute(
            "conv.map",
            map(vec![kv("a", int(1)), kv("b", map(vec![kv("c", string("d"))]))]),
        )],
        vec![probe("otlpprobe.conv.typed.empty")
            .with_attribute("conv.empty_string", string(""))
            .with_attribute("conv.no_value", empty())],
    ]
}

/// The traces with an HTTP status code in each form that a tracer sends.
fn status_code_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        status_span("otlpprobe.conv.status.int", "http.response.status_code", int(200)),
        status_span(
            "otlpprobe.conv.status.string",
            "http.response.status_code",
            string("404"),
        ),
        status_span("otlpprobe.conv.status.legacy_int", "http.status_code", int(503)),
        status_span("otlpprobe.conv.status.legacy_string", "http.status_code", string("201")),
        status_span(
            "otlpprobe.conv.status.invalid_string",
            "http.status_code",
            string("abc"),
        ),
        status_span("otlpprobe.conv.status.out_of_range", "http.status_code", string("700")),
        status_span("otlpprobe.conv.status.negative", "http.response.status_code", int(-1)),
        // Both keys: the legacy key wins.
        vec![probe("otlpprobe.conv.status.both_keys", SpanKind::Server)
            .with_strings(&[("http.request.method", "GET"), ("http.route", "/conv/status/both_keys")])
            .with_attribute("http.status_code", int(500))
            .with_attribute("http.response.status_code", int(200))],
    ]
}

/// The traces with each form of span status and error attributes.
fn error_traces() -> Vec<Vec<ProbeSpan>> {
    let probe = |name| probe(name, SpanKind::Internal);
    vec![
        vec![probe("otlpprobe.conv.error.message").with_status(StatusCode::Error, "conv failure")],
        vec![probe("otlpprobe.conv.error.no_message").with_status(StatusCode::Error, "")],
        vec![probe("otlpprobe.conv.error.ok_with_message").with_status(StatusCode::Ok, "all fine")],
        vec![probe("otlpprobe.conv.error.unset_with_message").with_status(StatusCode::Unset, "no status")],
        // An error with no message takes the one of its HTTP status code.
        vec![probe("otlpprobe.conv.error.http_status")
            .with_status(StatusCode::Error, "")
            .with_attribute("http.response.status_code", int(503))],
        // The Agent derives the message from a status code in `meta` only, and a string code is kept there.
        vec![probe("otlpprobe.conv.error.http_status_string")
            .with_status(StatusCode::Error, "")
            .with_attribute("http.response.status_code", string("503"))],
        vec![probe("otlpprobe.conv.error.exception_event")
            .with_status(StatusCode::Error, "")
            .with_event(event(
                "exception",
                at(1),
                vec![
                    kv("exception.message", string("conv exception")),
                    kv("exception.type", string("ConvError")),
                    kv("exception.stacktrace", string("conv.stack")),
                ],
            ))],
        // The attributes win over the status and the exception.
        vec![probe("otlpprobe.conv.error.attributes")
            .with_status(StatusCode::Error, "conv status message")
            .with_strings(&[
                ("error.msg", "conv attribute message"),
                ("error.type", "ConvAttributeError"),
                ("error.stack", "conv.attribute.stack"),
            ])],
        // Only one of the three attributes.
        vec![probe("otlpprobe.conv.error.partial_attributes")
            .with_status(StatusCode::Error, "conv partial message")
            .with_strings(&[("error.type", "ConvPartialError")])],
    ]
}

/// The traces with the sampling, analytics, peer and truncation attributes.
fn attribute_traces() -> Vec<Vec<ProbeSpan>> {
    let probe = |name| probe(name, SpanKind::Internal);
    vec![
        vec![probe("otlpprobe.conv.attribute.tracestate").with_trace_state("vendor=conv,ot=th:8")],
        vec![probe("otlpprobe.conv.attribute.sample_rate").with_attribute("_sample_rate", double(0.5))],
        // The attribute wins over the tracestate.
        vec![probe("otlpprobe.conv.attribute.sample_rate_and_tracestate")
            .with_attribute("_sample_rate", double(0.25))
            .with_trace_state("ot=th:8")],
        vec![probe("otlpprobe.conv.attribute.analytics_bool").with_attribute("analytics.event", boolean(true))],
        vec![probe("otlpprobe.conv.attribute.analytics_string").with_attribute("analytics.event", string("false"))],
        vec![probe("otlpprobe.conv.attribute.peer_service").with_strings(&[("peer.service", "Conv Peer!!")])],
        vec![probe("otlpprobe.conv.attribute.long_value")
            .with_attribute("conv.long_value", string(&"v".repeat(LONG_VALUE_LEN)))],
        vec![probe("otlpprobe.conv.attribute.long_key").with_attribute(&"k".repeat(LONG_KEY_LEN), string("conv"))],
    ]
}

/// The traces with span events and span links.
fn event_and_link_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        vec![probe("otlpprobe.conv.event.attributes", SpanKind::Internal)
            .with_event(event(
                "conv.event",
                at(2),
                vec![
                    kv("conv.string", string("s")),
                    kv("conv.int", int(7)),
                    kv("conv.double", double(2.5)),
                    kv("conv.bool", boolean(true)),
                    kv("conv.bytes", bytes(&[1, 2, 3])),
                    kv("conv.strings", array(vec![string("a"), string("b")])),
                    kv("conv.mixed", array(vec![int(1), string("x")])),
                    kv("conv.map", map(vec![kv("a", int(1))])),
                    kv("conv.no_value", empty()),
                ],
            ))
            .with_event(event("conv.event.plain", at(1), Vec::new()))
            .with_event(event(
                "exception",
                at(3),
                vec![kv("exception.message", string("conv event exception"))],
            ))],
        vec![probe("otlpprobe.conv.link.attributes", SpanKind::Internal)
            .with_link(link(
                (0x0b61, 0x0b62),
                0x0b63,
                "vendor=conv",
                1,
                vec![
                    kv("conv.string", string("s")),
                    kv("conv.int", int(7)),
                    kv("conv.strings", array(vec![string("a"), string("b")])),
                    kv("conv.map", map(vec![kv("a", string("b"))])),
                ],
            ))
            .with_link(link((0, 0x0b64), 0x0b65, "", 0, Vec::new()))
            // A name that the Agent does not normalize.
            .with_link(link(
                (0, 0x0b66),
                0x0b67,
                "",
                0,
                vec![kv("link.name", string("Conv Link Name!"))],
            ))],
    ]
}

/// The traces whose spans carry `datadog.*` attributes.
fn datadog_attribute_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        named(
            "otlpprobe.conv.datadog.fields",
            SpanKind::Internal,
            &[
                ("datadog.service", DATADOG_SPAN_SERVICE),
                ("datadog.name", "conv.datadog.operation"),
                ("datadog.resource", "conv datadog resource"),
                ("datadog.type", "conv-datadog-type"),
            ],
        ),
        vec![probe("otlpprobe.conv.datadog.error", SpanKind::Internal)
            .with_attribute("datadog.error", int(1))
            .with_strings(&[
                ("datadog.error.msg", "conv datadog message"),
                ("datadog.error.type", "ConvDatadogError"),
                ("datadog.error.stack", "conv.datadog.stack"),
            ])],
        vec![probe("otlpprobe.conv.datadog.kind_and_status", SpanKind::Server)
            .with_strings(&[
                ("datadog.span.kind", "client"),
                ("datadog.http_status_code", "418"),
                ("http.request.method", "GET"),
                ("http.route", "/conv/datadog-status"),
            ])
            .with_attribute("http.response.status_code", int(200))],
    ]
}

/// The traces of the spans that the resources of the fixture give their metadata.
///
/// Each resource has one `ResourceSpans`, so one payload, and one trace. The core agent fills a payload that has no
/// environment or version from the spans of its first trace, so a resource with two traces that set them would have
/// the metadata of either.
fn resource_traces() -> Vec<Vec<ProbeSpan>> {
    let internal = |service, name| vec![ProbeSpan::new(service, name, SpanKind::Internal)];
    vec![
        internal(META_SERVICE, "otlpprobe.conv.resource.from_resource"),
        // The same resource, with an environment and a version on the span as well.
        vec![ProbeSpan::new(
            META_SPAN_SERVICE,
            "otlpprobe.conv.resource.from_span",
            SpanKind::Internal,
        )
        .with_strings(&[
            ("deployment.environment", "conv-span-env"),
            ("service.version", "9.9.9"),
        ])],
        // A resource with neither.
        vec![ProbeSpan::new(
            SPAN_ENV_SERVICE,
            "otlpprobe.conv.resource.span_only",
            SpanKind::Internal,
        )
        .with_strings(&[
            ("deployment.environment", "conv-only-span-env"),
            ("service.version", "8.8.8"),
        ])],
        vec![ProbeSpan::new(
            DATADOG_ENV_SERVICE,
            "otlpprobe.conv.resource.datadog_env_version",
            SpanKind::Internal,
        )
        .with_strings(&[
            ("datadog.env", "conv-datadog-env"),
            ("datadog.version", "conv-datadog-version"),
            ("deployment.environment.name", "conv-otel-env"),
            ("service.version", "conv-otel-version"),
        ])],
        internal(HOST_SERVICE, "otlpprobe.conv.resource.host_name"),
        internal(DATADOG_HOST_SERVICE, "otlpprobe.conv.resource.datadog_host_name"),
        internal(FARGATE_SERVICE, "otlpprobe.conv.resource.fargate"),
        internal(UNNAMED_RESOURCE, "otlpprobe.conv.resource.no_service_name"),
        internal(DATADOG_RESOURCE_SERVICE, "otlpprobe.conv.resource.datadog_attributes"),
    ]
}

/// The trace with an `INTERNAL` root and a child of each other span kind.
///
/// A root is top level whatever its kind, so only the children show how the kind sets the top-level and the measured
/// marks.
fn kind_traces() -> Vec<Vec<ProbeSpan>> {
    let child = |name, kind| probe(name, kind).child_of(1);
    vec![vec![
        probe("otlpprobe.conv.kind.root", SpanKind::Internal),
        child("otlpprobe.conv.kind.server_child", SpanKind::Server),
        child("otlpprobe.conv.kind.client_child", SpanKind::Client),
        child("otlpprobe.conv.kind.producer_child", SpanKind::Producer),
        child("otlpprobe.conv.kind.consumer_child", SpanKind::Consumer),
    ]]
}

/// The traces whose spans have trace IDs that share their low half.
///
/// A trace groups spans by the low half of their ID. The two spans of the first and second traces have the same low
/// half and different high halves, and the spans of the third trace have the same ID.
fn high_traces() -> Vec<Vec<ProbeSpan>> {
    let span = |name| ProbeSpan::new(HIGH_SERVICE, name, SpanKind::Internal);
    vec![
        vec![
            span("otlpprobe.conv.high.mixed_a").with_trace_id(1, HIGH_LOW_HALF),
            span("otlpprobe.conv.high.mixed_b")
                .with_trace_id(2, HIGH_LOW_HALF)
                .child_of(1),
        ],
        vec![
            span("otlpprobe.conv.high.zero_half_a").with_trace_id(0, HIGH_LOW_HALF + 1),
            span("otlpprobe.conv.high.zero_half_b")
                .with_trace_id(5, HIGH_LOW_HALF + 1)
                .child_of(1),
        ],
        vec![
            span("otlpprobe.conv.high.same_a").with_trace_id(3, HIGH_LOW_HALF + 2),
            span("otlpprobe.conv.high.same_b")
                .with_trace_id(3, HIGH_LOW_HALF + 2)
                .child_of(1),
        ],
    ]
}

/// The traces that both sides drop for an ID of zero, each followed by a control that both keep.
fn zero_traces() -> Vec<Vec<ProbeSpan>> {
    let span = |name| ProbeSpan::new(ZERO_SERVICE, name, SpanKind::Internal);
    vec![
        vec![span("otlpprobe.conv.zero.trace_id").with_trace_id(0, 0)],
        vec![span("otlpprobe.conv.zero.low_half").with_trace_id(7, 0)],
        vec![
            span("otlpprobe.conv.zero.span_id_root"),
            span("otlpprobe.conv.zero.span_id_child")
                .with_zero_span_id()
                .child_of(1),
        ],
        vec![span("otlpprobe.conv.zero.control")],
    ]
}

/// A resource with an environment, a version, a container and an SDK that has an upper-case language and a `+` in its
/// version.
fn meta_resource(service: &'static str) -> ProbeResource {
    ProbeResource::new(service)
        .with_attribute("deployment.environment.name", string("conv-resource-env"))
        .with_attribute("service.version", string("1.2.3"))
        .with_attribute("telemetry.sdk.language", string("JAVA"))
        .with_attribute("telemetry.sdk.version", string("1.2+build.5"))
        .with_attribute("container.id", string("conv-container-id"))
}

/// The resources of the fixture, one for each payload.
fn resources() -> Vec<ProbeResource> {
    vec![
        ProbeResource::new(SERVICE),
        meta_resource(META_SERVICE),
        meta_resource(META_SPAN_SERVICE),
        ProbeResource::new(SPAN_ENV_SERVICE),
        ProbeResource::new(DATADOG_ENV_SERVICE),
        ProbeResource::new(HOST_SERVICE).with_attribute("host.name", string("conv-host-name")),
        ProbeResource::new(DATADOG_HOST_SERVICE).with_attribute("datadog.host.name", string("conv-datadog-host")),
        ProbeResource::new(FARGATE_SERVICE)
            .with_attribute("cloud.provider", string("aws"))
            .with_attribute("cloud.platform", string("aws_ecs"))
            .with_attribute("aws.ecs.launchtype", string("fargate"))
            .with_attribute(
                "aws.ecs.task.arn",
                string("arn:aws:ecs:us-east-1:123456789012:task/conv-cluster/0123456789abcdef"),
            ),
        ProbeResource::without_service_name(UNNAMED_RESOURCE),
        ProbeResource::new(DATADOG_RESOURCE_SERVICE)
            .with_attribute("datadog.name", string("conv.datadog.resource.operation"))
            .with_attribute("datadog.resource", string("conv datadog resource attribute"))
            .with_attribute("datadog.env", string("conv-datadog-resource-env"))
            .with_attribute("datadog.version", string("conv-datadog-resource-version"))
            .with_attribute("datadog.host.name", string("conv-datadog-resource-host")),
        ProbeResource::new(HIGH_SERVICE),
        ProbeResource::new(ZERO_SERVICE),
    ]
}

/// Spans of the services `probe-conv` and `probe-conv-*`, built so that each way that a span converts has a span of
/// its own.
///
/// A span is named `otlpprobe.conv.<family>.<variant>`. Every span is the root of its trace, except those of
/// `kind_traces` and `high_traces` and the zero span ID probe, which have a child.
pub fn conversion_fixture() -> Vec<u8> {
    let traces: Vec<Vec<ProbeSpan>> = [
        naming_traces(),
        typed_attribute_traces(),
        status_code_traces(),
        error_traces(),
        attribute_traces(),
        event_and_link_traces(),
        datadog_attribute_traces(),
        resource_traces(),
        kind_traces(),
        high_traces(),
        zero_traces(),
    ]
    .into_iter()
    .flatten()
    .collect();
    probe_request(&resources(), CONVERSION_SEED, &traces)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use otlp_protos::opentelemetry::proto::{
        collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value, trace::v1::Span,
    };
    use prost::Message as _;

    use super::*;

    /// Returns each span of `conversion_fixture` and the `service.name` of its resource, or `None` for a resource that
    /// has none.
    fn spans() -> Vec<(Option<String>, Span)> {
        let request = ExportTraceServiceRequest::decode(conversion_fixture().as_slice()).unwrap();
        request
            .resource_spans
            .iter()
            .flat_map(|resource_spans| {
                let service = resource_spans
                    .resource
                    .as_ref()
                    .unwrap()
                    .attributes
                    .iter()
                    .find(|attribute| attribute.key == "service.name")
                    .and_then(|attribute| match &attribute.value.as_ref()?.value {
                        Some(Value::StringValue(service)) => Some(service.clone()),
                        _ => None,
                    });
                resource_spans.scope_spans[0]
                    .spans
                    .iter()
                    .map(move |span| (service.clone(), span.clone()))
            })
            .collect()
    }

    /// The span of `conversion_fixture` named `name`.
    fn span_named(name: &str) -> Span {
        let mut found = spans()
            .into_iter()
            .map(|(_, span)| span)
            .filter(|span| span.name == name);
        let span = found.next().unwrap_or_else(|| panic!("no span named '{name}'"));
        assert!(found.next().is_none(), "more than one span named '{name}'");
        span
    }

    /// The high and the low half of the trace ID of `span`.
    fn halves(span: &Span) -> (u64, u64) {
        (
            u64::from_be_bytes(span.trace_id[..8].try_into().unwrap()),
            u64::from_be_bytes(span.trace_id[8..].try_into().unwrap()),
        )
    }

    #[test]
    fn every_span_has_a_distinct_name_of_the_conversion_family() {
        let spans = spans();
        assert_eq!(spans.len(), 91);

        let names: HashSet<&str> = spans.iter().map(|(_, span)| span.name.as_str()).collect();
        assert_eq!(names.len(), spans.len(), "a span name is not unique");
        assert!(names.iter().all(|name| name.starts_with("otlpprobe.conv.")));
    }

    #[test]
    fn every_service_belongs_to_the_conversion_family() {
        for (service, span) in spans() {
            match service {
                Some(service) => assert!(service.starts_with(SERVICE), "{} is in '{service}'", span.name),
                None => assert_eq!(span.name, "otlpprobe.conv.resource.no_service_name"),
            }
        }
    }

    #[test]
    fn the_high_traces_share_a_low_half_and_differ_in_their_high_half() {
        let (high_a, low_a) = halves(&span_named("otlpprobe.conv.high.mixed_a"));
        let (high_b, low_b) = halves(&span_named("otlpprobe.conv.high.mixed_b"));
        assert_eq!(low_a, low_b);
        assert_ne!(high_a, high_b);

        let (high_a, low_a) = halves(&span_named("otlpprobe.conv.high.zero_half_a"));
        let (high_b, low_b) = halves(&span_named("otlpprobe.conv.high.zero_half_b"));
        assert_eq!(low_a, low_b);
        assert_eq!(high_a, 0);
        assert_ne!(high_b, 0);

        let (high_a, low_a) = halves(&span_named("otlpprobe.conv.high.same_a"));
        let (high_b, low_b) = halves(&span_named("otlpprobe.conv.high.same_b"));
        assert_eq!((high_a, low_a), (high_b, low_b));
    }

    #[test]
    fn the_zero_probes_have_a_zero_id_and_the_control_has_none() {
        assert_eq!(halves(&span_named("otlpprobe.conv.zero.trace_id")), (0, 0));
        assert_eq!(halves(&span_named("otlpprobe.conv.zero.low_half")).1, 0);
        assert_eq!(span_named("otlpprobe.conv.zero.span_id_child").span_id, vec![0; 8]);

        let control = span_named("otlpprobe.conv.zero.control");
        assert_ne!(halves(&control).1, 0);
        assert_ne!(control.span_id, vec![0; 8]);
    }

    #[test]
    fn the_kind_trace_has_an_internal_root_and_a_child_of_each_other_kind() {
        let root = span_named("otlpprobe.conv.kind.root");
        assert_eq!(root.kind, SpanKind::Internal as i32);
        assert!(root.parent_span_id.is_empty());

        for (variant, kind) in [
            ("server", SpanKind::Server),
            ("client", SpanKind::Client),
            ("producer", SpanKind::Producer),
            ("consumer", SpanKind::Consumer),
        ] {
            let child = span_named(&format!("otlpprobe.conv.kind.{variant}_child"));
            assert_eq!(child.kind, kind as i32, "{variant}");
            assert_eq!(child.trace_id, root.trace_id, "{variant}");
            assert_eq!(child.parent_span_id, root.span_id, "{variant}");
        }
    }

    #[test]
    fn each_status_probe_has_a_route_of_its_own() {
        let routes: HashSet<String> = spans()
            .into_iter()
            .filter(|(_, span)| span.name.starts_with("otlpprobe.conv.status."))
            .map(|(_, span)| {
                span.attributes
                    .iter()
                    .find(|attribute| attribute.key == "http.route")
                    .and_then(|attribute| match &attribute.value.as_ref()?.value {
                        Some(Value::StringValue(route)) => Some(route.clone()),
                        _ => None,
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(routes.len(), 8);
    }
}

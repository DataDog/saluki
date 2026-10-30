//! The golden vectors: OTLP requests and the Agent's wire output for them.
//!
//! Ported from the Datadog Agent at commit `e0c8d8d93befb6864f13e2d88acbe13fb2d380b7`:
//!
//! - `TestTranslator` and `TestDeriveStatus` in
//!   <https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go>
//! - `TestLogsExporter` in
//!   <https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/comp/otelcol/otlp/components/exporter/logsagentexporter/logs_exporter_test.go>
//!
//! The Agent's tests stop at the translator's `HTTPLogItem` or at the exporter's message. Each target here is that
//! expected value carried through the rest of the Agent's pipeline by [`agent_wire_log`], so that it can be compared
//! with what ADP puts on the wire.
//!
//! The targets differ from the Agent's tests in two ways, both because ADP runs as the Agent's embedded OTLP receiver:
//!
//! - The translator tests build their translator with the OTel source `test`, so their tags end in
//!   `otel_source:test`. The embedded receiver uses `datadog_agent`.
//! - `exporter/ddtags-different-tags` configures the OTel source `datadog_exporter` and an empty log source name. The
//!   targets use the embedded receiver's `datadog_agent` and `otlp_log_ingestion`.
//!
//! `TestTranslatorWithRUMRouting` isn't ported: the logs agent exporter calls `MapLogs`, which doesn't route RUM events.

use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
};
use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use serde_json::{json, Map, Value};

use super::{AGENT_HOSTNAME, ENCODE_TIME_MS};

/// The log source name of the Agent's OTLP logs, used as `ddsource` unless `datadog.log.source` is set.
const AGENT_LOG_SOURCE: &str = "otlp_log_ingestion";

/// A golden vector: one OTLP request, and the logs the Agent sends for it.
pub struct GoldenVector {
    /// The vector's name, which the gap manifest and the current fixtures refer to.
    pub name: &'static str,

    /// The OTLP request to send.
    pub request: ExportLogsServiceRequest,

    /// The logs the Agent sends to the intake for the request, in order, as JSON objects.
    ///
    /// A `message` that is a JSON object stands for that object serialized as a string.
    pub target: Vec<Value>,
}

/// The fields of the `datadogV2.HTTPLogItem` that the Agent's translator returns.
#[derive(Default)]
struct HttpLogItem {
    ddtags: &'static str,
    hostname: Option<&'static str>,
    service: Option<&'static str>,
    message: &'static str,
    additional_properties: Value,
}

/// Returns the log that the Agent sends to the intake for a translated `item`.
///
/// This follows the rest of the Agent's pipeline after the translator:
///
/// - The logs agent exporter (`logs_exporter.go`) moves `ddtags` to the message origin, takes the status from the
///   `status` property (`info` when it's missing or empty), and the source from the `datadog.log.source` property (the
///   log source name when it's missing or empty). The message content is the rest of the item, serialized by
///   `HTTPLogItem.MarshalJSON`, where the additional properties overwrite the top-level fields.
/// - The logs processor (`processor.go`) uses the Agent's hostname when the log has none.
/// - The JSON encoder (`json.go`) writes every envelope field, with an encode-time timestamp in milliseconds.
fn agent_wire_log(item: HttpLogItem) -> Value {
    let mut content = Map::new();
    if let Some(hostname) = item.hostname {
        content.insert("hostname".to_string(), json!(hostname));
    }
    content.insert("message".to_string(), json!(item.message));
    if let Some(service) = item.service {
        content.insert("service".to_string(), json!(service));
    }
    match item.additional_properties {
        Value::Object(properties) => content.extend(properties),
        Value::Null => {}
        other => panic!("additional properties must be a JSON object, got {}", other),
    }

    let non_empty_property = |key: &str| {
        content
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let status = non_empty_property("status").unwrap_or("info").to_string();
    let ddsource = non_empty_property("datadog.log.source")
        .unwrap_or(AGENT_LOG_SOURCE)
        .to_string();

    json!({
        "message": content,
        "status": status,
        "timestamp": ENCODE_TIME_MS,
        "hostname": item.hostname.unwrap_or(AGENT_HOSTNAME),
        "service": item.service.unwrap_or(""),
        "ddsource": ddsource,
        "ddtags": item.ddtags,
    })
}

fn string(value: &str) -> AnyValue {
    AnyValue {
        value: Some(OtlpValue::StringValue(value.to_string())),
    }
}

fn int(value: i64) -> AnyValue {
    AnyValue {
        value: Some(OtlpValue::IntValue(value)),
    }
}

fn map<const N: usize>(entries: [(&str, AnyValue); N]) -> AnyValue {
    AnyValue {
        value: Some(OtlpValue::KvlistValue(KeyValueList {
            values: entries.into_iter().map(|(key, value)| key_value(key, value)).collect(),
        })),
    }
}

fn array<const N: usize>(values: [AnyValue; N]) -> AnyValue {
    AnyValue {
        value: Some(OtlpValue::ArrayValue(ArrayValue { values: values.into() })),
    }
}

fn strings<const N: usize>(values: [&str; N]) -> AnyValue {
    array(values.map(string))
}

fn key_value(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(value),
    }
}

/// Builds a log record, the way the Agent's tests fill in a `plog.LogRecord`.
#[derive(Default)]
struct Record(LogRecord);

impl Record {
    fn attr(mut self, key: &str, value: AnyValue) -> Self {
        self.0.attributes.push(key_value(key, value));
        self
    }

    fn severity_number(mut self, severity_number: i32) -> Self {
        self.0.severity_number = severity_number;
        self
    }

    fn severity_text(mut self, severity_text: &str) -> Self {
        self.0.severity_text = severity_text.to_string();
        self
    }

    fn body(mut self, body: &str) -> Self {
        self.0.body = Some(string(body));
        self
    }

    fn trace_id(mut self, trace_id: [u8; 16]) -> Self {
        self.0.trace_id = trace_id.to_vec();
        self
    }

    fn span_id(mut self, span_id: [u8; 8]) -> Self {
        self.0.span_id = span_id.to_vec();
        self
    }

    fn timestamp(mut self, time_unix_nano: u64) -> Self {
        self.0.time_unix_nano = time_unix_nano;
        self
    }

    fn observed_timestamp(mut self, observed_time_unix_nano: u64) -> Self {
        self.0.observed_time_unix_nano = observed_time_unix_nano;
        self
    }

    fn event_name(mut self, event_name: &str) -> Self {
        self.0.event_name = event_name.to_string();
        self
    }

    fn dropped_attributes_count(mut self, count: u32) -> Self {
        self.0.dropped_attributes_count = count;
        self
    }
}

fn record() -> Record {
    Record::default()
}

fn resource<const N: usize>(attributes: [(&str, AnyValue); N]) -> Resource {
    Resource {
        attributes: attributes
            .into_iter()
            .map(|(key, value)| key_value(key, value))
            .collect(),
        ..Default::default()
    }
}

fn scope<const N: usize>(name: &str, version: &str, attributes: [(&str, AnyValue); N]) -> InstrumentationScope {
    InstrumentationScope {
        name: name.to_string(),
        version: version.to_string(),
        attributes: attributes
            .into_iter()
            .map(|(key, value)| key_value(key, value))
            .collect(),
        ..Default::default()
    }
}

/// Builds a request with one resource and one scope, as `TestTranslator` does.
fn request(resource: Resource, scope: InstrumentationScope, records: Vec<Record>) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource),
            scope_logs: vec![ScopeLogs {
                scope: Some(scope),
                log_records: records.into_iter().map(|record| record.0).collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn no_scope() -> InstrumentationScope {
    InstrumentationScope::default()
}

/// The tags of a log whose resource has no attributes that map to tags.
const OTEL_SOURCE_TAG: &str = "otel_source:datadog_agent";

// `TestTranslator` IDs: the span ID is the second half of the trace ID.
const TRACE_ID: [u8; 16] = [
    0x08, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00,
];
const SPAN_ID: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00];
const TRACE_ID_HEX: &str = "0802030405060708000000000a000000";
const SPAN_ID_HEX: &str = "000000000a000000";
const DD_TRACE_ID: &str = "167772160";
const DD_SPAN_ID: &str = "167772160";

/// Returns the vectors ported from `TestTranslator`.
fn translator_vectors() -> Vec<GoldenVector> {
    let traced = || {
        record()
            .attr("app", string("test"))
            .span_id(SPAN_ID)
            .trace_id(TRACE_ID)
            .attr("service.name", string("otlp_col"))
    };

    vec![
        GoldenVector {
            name: "translator/basic",
            request: request(
                resource([]),
                no_scope(),
                vec![record().attr("app", string("test")).severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/resource",
            request: request(
                resource([("service.name", string("otlp_col"))]),
                no_scope(),
                vec![record().attr("app", string("test")).severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: "service:otlp_col,otel_source:datadog_agent",
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/append-tags",
            request: request(
                resource([("service.name", string("otlp_col"))]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("ddtags", string("foo:bar"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: "service:otlp_col,foo:bar,otel_source:datadog_agent",
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/service",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("service.name", string("otlp_col"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/trace",
            request: request(resource([]), no_scope(), vec![traced().severity_number(5)]),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/trace-from-attributes",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("spanid", string("2e26da881214cd7c"))
                    .attr("traceid", string("437ab4d83468c540bb0f3398a39faa59"))
                    .attr("service.name", string("otlp_col"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "otel.trace_id": "437ab4d83468c540bb0f3398a39faa59",
                    "dd.span_id": "3325585652813450620",
                    "dd.trace_id": "13479048940416379481",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/trace-from-attributes-underscore",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("span_id", string("2e26da881214cd7c"))
                    .attr("trace_id", string("740112b325075be8c80a48de336ebc67"))
                    .attr("service.name", string("otlp_col"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "otel.trace_id": "740112b325075be8c80a48de336ebc67",
                    "dd.span_id": "3325585652813450620",
                    "dd.trace_id": "14414413676535528551",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/trace-from-attributes-decode-error",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("spanid", string("2e26da881214cd7c"))
                    .attr("traceid", string("invalidtraceid"))
                    .attr("service.name", string("otlp_col"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "dd.span_id": "3325585652813450620",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/trace-from-attributes-size-error",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .attr("app", string("test"))
                    .attr("spanid", string("2023675201651514964"))
                    .attr(
                        "traceid",
                        string("eb068afe5e53704f3b0dc3d3e1e397cb760549a7b58547db4f1dee845d9101f8db1ccf8fdd0976a9112f"),
                    )
                    .attr("service.name", string("otlp_col"))
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/severity-text",
            request: request(
                resource([]),
                no_scope(),
                vec![traced().severity_text("alert").severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "alert",
                    "otel.severity_text": "alert",
                    "otel.severity_number": "5",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/body",
            request: request(
                resource([]),
                no_scope(),
                vec![traced().severity_number(13).body("This is log")],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "message": "This is log",
                    "app": "test",
                    "status": "warn",
                    "otel.severity_number": "13",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/log-level",
            request: request(
                resource([]),
                no_scope(),
                vec![traced().attr("level", string("error")).body("This is log")],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                service: Some("otlp_col"),
                additional_properties: json!({
                    "message": "This is log",
                    "app": "test",
                    "status": "error",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/resource-attributes",
            request: request(
                resource([("service.name", string("otlp_col")), ("key", string("val"))]),
                no_scope(),
                vec![record().attr("app", string("test")).severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: "service:otlp_col,otel_source:datadog_agent",
                service: Some("otlp_col"),
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "key": "val",
                    "service.name": "otlp_col",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/reserved-resource-attributes",
            request: request(
                resource([("hostname", string("example_host")), ("service", string("otlp_col"))]),
                no_scope(),
                vec![record().attr("app", string("test")).severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: "service:otlp_col,otel_source:datadog_agent",
                additional_properties: json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.service": "otlp_col",
                    "otel.hostname": "example_host",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/nestings",
            request: request(
                resource([]),
                no_scope(),
                vec![record().attr(
                    "root",
                    map([
                        ("nest1", map([("nest2", string("val"))])),
                        ("nest12", map([("nest22", map([("nest3", string("val2"))]))])),
                        ("nest13", string("val3")),
                    ]),
                )],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "root.nest1.nest2": "val",
                    "root.nest12.nest22.nest3": "val2",
                    "root.nest13": "val3",
                    "status": "",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/nil-map",
            request: request(resource([]), no_scope(), vec![record()]),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({"status": ""}),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/too-many-nestings",
            request: request(
                resource([]),
                no_scope(),
                vec![record().attr(
                    "nest1",
                    map([(
                        "nest2",
                        map([(
                            "nest3",
                            map([(
                                "nest4",
                                map([(
                                    "nest5",
                                    map([
                                        (
                                            "nest6",
                                            map([(
                                                "nest7",
                                                map([(
                                                    "nest8",
                                                    map([(
                                                        "nest9",
                                                        map([(
                                                            "nest10",
                                                            map([("nest11", map([("nest12", string("ok"))]))]),
                                                        )]),
                                                    )]),
                                                )]),
                                            )]),
                                        ),
                                        ("nest14", map([("nest15", string("ok2"))])),
                                    ]),
                                )]),
                            )]),
                        )]),
                    )]),
                )],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "nest1.nest2.nest3.nest4.nest5.nest6.nest7.nest8.nest9.nest10": "{\"nest11\":{\"nest12\":\"ok\"}}",
                    "nest1.nest2.nest3.nest4.nest5.nest14.nest15": "ok2",
                    "status": "",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/timestamp",
            request: request(
                resource([]),
                no_scope(),
                vec![record().timestamp(1_700_499_303_397_000_000).severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "@timestamp": "2023-11-20T16:55:03.397Z",
                    "otel.timestamp": "1700499303397000000",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/observed-timestamp",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .observed_timestamp(1_700_499_303_397_000_000)
                    .severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "@timestamp": "2023-11-20T16:55:03.397Z",
                    "otel.timestamp": "1700499303397000000",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/event-name",
            request: request(
                resource([]),
                no_scope(),
                vec![record().event_name("app.screen.click").severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.event_name": "app.screen.click",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/scope-attributes",
            request: request(
                resource([]),
                scope(
                    "",
                    "",
                    [
                        ("otelcol.component.id", string("otlp")),
                        ("otelcol.component.kind", string("Receiver")),
                    ],
                ),
                vec![record().body("hello world").severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "hello world",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otelcol.component.id": "otlp",
                    "otelcol.component.kind": "Receiver",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/scope-name",
            request: request(
                resource([]),
                scope("go.opentelemetry.io/contrib/bridges/otelslog", "", []),
                vec![record().body("hello world").severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "hello world",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "go.opentelemetry.io/contrib/bridges/otelslog",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/scope-version",
            request: request(
                resource([]),
                scope("go.opentelemetry.io/contrib/bridges/otelslog", "v1.2.3", []),
                vec![record().body("hello world").severity_number(5)],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "hello world",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "go.opentelemetry.io/contrib/bridges/otelslog",
                    "otel.scope.version": "v1.2.3",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/library-aliases",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .body("hello world")
                    .severity_number(5)
                    .attr("otel.library.name", string("my-library"))
                    .attr("otel.library.version", string("v0.1.0"))],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "hello world",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "my-library",
                    "otel.scope.version": "v0.1.0",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/explicit-scope-name",
            request: request(
                resource([]),
                scope("scope-metadata-name", "", []),
                vec![record()
                    .body("hello world")
                    .severity_number(5)
                    .attr("otel.scope.name", string("explicit-scope-name"))
                    .attr("otel.library.name", string("library-alias-name"))],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "hello world",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "explicit-scope-name",
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/array-of-strings",
            request: request(
                resource([]),
                no_scope(),
                vec![record()
                    .body("test array attribute")
                    .severity_number(5)
                    .attr("test_array", strings(["value1", "value2", "value3"]))],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "test array attribute",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "test_array": ["value1", "value2", "value3"],
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/array-of-maps",
            request: request(
                resource([]),
                no_scope(),
                vec![record().body("test array with maps").severity_number(5).attr(
                    "array_with_maps",
                    array([
                        map([("item_name", string("item1")), ("value", int(100))]),
                        map([("item_name", string("item2")), ("value", int(200))]),
                    ]),
                )],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "test array with maps",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "array_with_maps": [
                        {"item_name": "item1", "value": 100},
                        {"item_name": "item2", "value": 200},
                    ],
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/multiple-nesting",
            request: request(
                resource([]),
                no_scope(),
                vec![record().body("test multiple nesting").severity_number(5).attr(
                    "multiple_nesting",
                    array([
                        map([
                            ("month", string("January")),
                            ("categories", strings(["meetings", "personal", "holidays"])),
                            (
                                "events",
                                array([
                                    map([
                                        ("title", string("Team Meeting")),
                                        ("attendees", strings(["a", "b", "c"])),
                                    ]),
                                    map([("title", string("Project Review")), ("attendees", strings(["d", "e"]))]),
                                ]),
                            ),
                        ]),
                        map([
                            ("month", string("February")),
                            ("categories", strings(["conferences", "workshops"])),
                            (
                                "events",
                                array([map([
                                    ("title", string("Tech Conference")),
                                    ("attendees", strings(["f", "g", "h"])),
                                ])]),
                            ),
                        ]),
                    ]),
                )],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "test multiple nesting",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "multiple_nesting": [
                        {
                            "month": "January",
                            "categories": ["meetings", "personal", "holidays"],
                            "events": [
                                {"title": "Team Meeting", "attendees": ["a", "b", "c"]},
                                {"title": "Project Review", "attendees": ["d", "e"]},
                            ],
                        },
                        {
                            "month": "February",
                            "categories": ["conferences", "workshops"],
                            "events": [
                                {"title": "Tech Conference", "attendees": ["f", "g", "h"]},
                            ],
                        },
                    ],
                }),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "translator/slice-of-slices-of-maps",
            request: request(
                resource([]),
                no_scope(),
                vec![record().body("test slice of slices of maps").severity_number(5).attr(
                    "matrix",
                    array([0, 1, 2].map(|row| {
                        array([0, 1].map(|col| {
                            let value = ["a", "b", "c", "d", "e", "f"][(row * 2 + col) as usize];
                            map([("row", int(row)), ("col", int(col)), ("value", string(value))])
                        }))
                    })),
                )],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                message: "test slice of slices of maps",
                additional_properties: json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "matrix": [
                        [{"row": 0, "col": 0, "value": "a"}, {"row": 0, "col": 1, "value": "b"}],
                        [{"row": 1, "col": 0, "value": "c"}, {"row": 1, "col": 1, "value": "d"}],
                        [{"row": 2, "col": 0, "value": "e"}, {"row": 2, "col": 1, "value": "f"}],
                    ],
                }),
                ..Default::default()
            })],
        },
    ]
}

/// Returns the vectors ported from `TestDeriveStatus`, as logs with only a severity number.
fn derive_status_vectors() -> Vec<GoldenVector> {
    [
        ("derive-status/trace3", 3, "trace"),
        ("derive-status/trace4", 4, "trace"),
        ("derive-status/debug5", 5, "debug"),
        ("derive-status/debug7", 7, "debug"),
        ("derive-status/debug8", 8, "debug"),
        ("derive-status/info9", 9, "info"),
        ("derive-status/info12", 12, "info"),
        ("derive-status/warn13", 13, "warn"),
        ("derive-status/warn16", 16, "warn"),
        ("derive-status/error17", 17, "error"),
        ("derive-status/error20", 20, "error"),
        ("derive-status/fatal21", 21, "fatal"),
        ("derive-status/fatal24", 24, "fatal"),
        ("derive-status/undefined", 50, "error"),
    ]
    .into_iter()
    .map(|(name, severity_number, status)| GoldenVector {
        name,
        request: request(
            resource([]),
            no_scope(),
            vec![record().severity_number(severity_number)],
        ),
        target: vec![agent_wire_log(HttpLogItem {
            ddtags: OTEL_SOURCE_TAG,
            additional_properties: json!({
                "status": status,
                "otel.severity_number": severity_number.to_string(),
            }),
            ..Default::default()
        })],
    })
    .collect()
}

// `TestLogsExporter` uses the records of `comp/otelcol/otlp/testutil`.
const TEST_LOG_TIME_UNIX_NANO: u64 = 1_581_452_773_000_000_789;
const TEST_LOG_TIME: &str = "2020-02-11T20:26:13.000Z";
const LOG_ONE_SPAN_ID: [u8; 8] = [0x01, 0x02, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00];
const LOG_ONE_TRACE_ID: [u8; 16] = [
    0x08, 0x04, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The record of `testutil.fillLogOne`.
fn log_one() -> Record {
    record()
        .timestamp(TEST_LOG_TIME_UNIX_NANO)
        .dropped_attributes_count(1)
        .severity_number(9)
        .severity_text("Info")
        .span_id(LOG_ONE_SPAN_ID)
        .trace_id(LOG_ONE_TRACE_ID)
        .attr("app", string("server"))
        .attr("instance_num", int(1))
        .body("This is a log message")
}

/// The record of `testutil.fillLogTwo`.
fn log_two() -> Record {
    record()
        .timestamp(TEST_LOG_TIME_UNIX_NANO)
        .dropped_attributes_count(1)
        .severity_number(9)
        .severity_text("Info")
        .attr("customer", string("acme"))
        .attr("env", string("dev"))
        .body("something happened")
}

/// The resource of `testutil.GenerateLogsOneLogRecord`, with `extra` attributes after its own.
fn testutil_resource<const N: usize>(extra: [(&str, AnyValue); N]) -> Resource {
    let mut resource = resource([("resource-attr", string("resource-attr-val-1"))]);
    resource
        .attributes
        .extend(extra.into_iter().map(|(key, value)| key_value(key, value)));
    resource
}

/// The content properties of `fillLogOne`, merged with `overrides`.
fn log_one_properties(overrides: Value) -> Value {
    let mut properties = json!({
        "message": "This is a log message",
        "app": "server",
        "instance_num": 1,
        "@timestamp": TEST_LOG_TIME,
        "status": "Info",
        "dd.span_id": "72624976397598720",
        "dd.trace_id": "0",
        "otel.severity_text": "Info",
        "otel.severity_number": "9",
        "otel.span_id": "0102040800000000",
        "otel.trace_id": "08040201000000000000000000000000",
        "otel.timestamp": TEST_LOG_TIME_UNIX_NANO.to_string(),
        "resource-attr": "resource-attr-val-1",
    });
    if let (Value::Object(properties), Value::Object(overrides)) = (&mut properties, overrides) {
        properties.extend(overrides);
    }
    properties
}

/// The content properties of `fillLogTwo`.
fn log_two_properties() -> Value {
    json!({
        "message": "something happened",
        "env": "dev",
        "customer": "acme",
        "@timestamp": TEST_LOG_TIME,
        "status": "Info",
        "otel.severity_text": "Info",
        "otel.severity_number": "9",
        "otel.timestamp": TEST_LOG_TIME_UNIX_NANO.to_string(),
        "resource-attr": "resource-attr-val-1",
    })
}

/// Returns the vectors ported from `TestLogsExporter`.
///
/// The exporter test checks the message content with its `message` key, so the content is given as properties.
fn exporter_vectors() -> Vec<GoldenVector> {
    vec![
        GoldenVector {
            name: "exporter/message",
            request: request(testutil_resource([]), no_scope(), vec![log_one()]),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: log_one_properties(json!({})),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "exporter/message-attribute",
            request: request(
                testutil_resource([]),
                no_scope(),
                vec![log_one()
                    .attr("message", string("hello"))
                    .attr("datadog.log.source", string("custom_source"))
                    .attr("host.name", string("test-host"))],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                hostname: Some("test-host"),
                additional_properties: log_one_properties(json!({
                    "message": "hello",
                    "datadog.log.source": "custom_source",
                    "host.name": "test-host",
                })),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "exporter/resource-attribute-source",
            request: request(
                testutil_resource([("datadog.log.source", string("custom_source_rattr"))]),
                no_scope(),
                vec![log_one()],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: log_one_properties(json!({
                    "datadog.log.source": "custom_source_rattr",
                })),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "exporter/status",
            request: request(
                testutil_resource([]),
                no_scope(),
                vec![log_one().severity_text("Fatal")],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: OTEL_SOURCE_TAG,
                additional_properties: log_one_properties(json!({
                    "status": "Fatal",
                    "otel.severity_text": "Fatal",
                })),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "exporter/ddtags",
            request: request(
                testutil_resource([]),
                no_scope(),
                vec![log_one().attr("ddtags", string("tag1:true"))],
            ),
            target: vec![agent_wire_log(HttpLogItem {
                ddtags: "tag1:true,otel_source:datadog_agent",
                additional_properties: log_one_properties(json!({})),
                ..Default::default()
            })],
        },
        GoldenVector {
            name: "exporter/ddtags-same-tags",
            request: request(
                testutil_resource([]),
                no_scope(),
                vec![
                    log_one().attr("ddtags", string("tag1:true")),
                    log_two().attr("ddtags", string("tag1:true")),
                ],
            ),
            target: vec![
                agent_wire_log(HttpLogItem {
                    ddtags: "tag1:true,otel_source:datadog_agent",
                    additional_properties: log_one_properties(json!({})),
                    ..Default::default()
                }),
                agent_wire_log(HttpLogItem {
                    ddtags: "tag1:true,otel_source:datadog_agent",
                    additional_properties: log_two_properties(),
                    ..Default::default()
                }),
            ],
        },
        GoldenVector {
            name: "exporter/ddtags-different-tags",
            request: request(
                testutil_resource([]),
                no_scope(),
                vec![
                    log_one().attr("ddtags", string("tag1:true")),
                    log_two().attr("ddtags", string("tag2:true")),
                ],
            ),
            target: vec![
                agent_wire_log(HttpLogItem {
                    ddtags: "tag1:true,otel_source:datadog_agent",
                    additional_properties: log_one_properties(json!({})),
                    ..Default::default()
                }),
                agent_wire_log(HttpLogItem {
                    ddtags: "tag2:true,otel_source:datadog_agent",
                    additional_properties: log_two_properties(),
                    ..Default::default()
                }),
            ],
        },
    ]
}

/// Returns every golden vector.
pub fn golden_vectors() -> Vec<GoldenVector> {
    let mut vectors = translator_vectors();
    vectors.extend(derive_status_vectors());
    vectors.extend(exporter_vectors());
    vectors
}

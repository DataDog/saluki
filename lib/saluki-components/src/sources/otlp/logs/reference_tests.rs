//! OTLP logs tests ported from the reference implementation.
//!
//! The reference tests compare the JSON of the `datadogV2.HTTPLogItem` that the reference translator returns for one
//! log record. Each case here compares the same JSON for the translated log, with the log's status as the item's
//! `status` property, merged with the log's other fields as the logs encoder merges them. The reference tests build their translator with the OTel source `test`, so their tags end in
//! `otel_source:test`. This translator always tags logs with `otel_source:datadog_agent`, so the expected tags here
//! use that instead.
//!
//! Cases that don't match yet are in ignored `target_*` tests, one per gap. A change that closes a gap moves its cases
//! into `translator_cases` or `exporter_cases`, and removes the target test.
//!
//! The exporter cases check the message content, the source, and the tags of each log that the exporter sends, which
//! here are the rendered content, the source, and the tags of the translated log.

// Ported from `TestTranslator` and `TestDeriveStatus` in
// https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go
//
// `TestTranslatorWithRUMRouting` isn't ported: the logs exporter doesn't route RUM events.
//
// `TestLogsExporter` in
// https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/comp/otelcol/otlp/components/exporter/logsagentexporter/logs_exporter_test.go
// is ported too. Its case with different tags configures the OTel source `datadog_exporter` and an empty log source
// name. Here it uses the translator's own OTel source `datadog_agent` and log source `otlp_log_ingestion`.

use std::collections::BTreeSet;

use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
};
use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_core::data_model::event::log::Log;
use saluki_env::workload::providers::NoopWorkloadProvider;
use serde_json::{json, Map, Value};

use super::render::ContentRenderer;
use super::translator::ResourceLogsIter;
use crate::common::otlp::origin::OtlpOriginTagResolver;

/// A translated log, as the reference tests compare it: the JSON of the `datadogV2.HTTPLogItem`.
///
/// The JSON has the item's `message`, `status`, `hostname`, and `service`, overwritten by its additional properties,
/// as the logs encoder writes them. The tags are a set: the reference tests compare them as a string, but the logs
/// encoder removes duplicate tags, and their order doesn't matter to the intake.
#[derive(Debug, PartialEq)]
struct Item {
    ddtags: BTreeSet<String>,
    json: Map<String, Value>,
}

/// Returns the item with the given tags and JSON. The JSON's `message` is empty unless it's given.
fn item(ddtags: &str, json: Value) -> Item {
    let Value::Object(mut json) = json else {
        panic!("the item JSON must be an object");
    };
    json.entry("message").or_insert_with(|| json!(""));
    Item {
        ddtags: ddtags.split(',').map(str::to_string).collect(),
        json,
    }
}

/// A reference test case: one log record, and the item the reference translator returns for it.
struct Case {
    name: &'static str,
    resource: Resource,
    scope: InstrumentationScope,
    record: LogRecord,
    expected: Item,
}

fn case(name: &'static str, record: Record, expected: Item) -> Case {
    Case {
        name,
        resource: Resource::default(),
        scope: InstrumentationScope::default(),
        record: record.0,
        expected,
    }
}

impl Case {
    fn resource<const N: usize>(mut self, attributes: [(&str, AnyValue); N]) -> Self {
        self.resource.attributes = key_values(attributes);
        self
    }

    fn scope<const N: usize>(mut self, name: &str, version: &str, attributes: [(&str, AnyValue); N]) -> Self {
        self.scope = InstrumentationScope {
            name: name.to_string(),
            version: version.to_string(),
            attributes: key_values(attributes),
            ..Default::default()
        };
        self
    }
}

/// Translates log records with one resource and one scope, as the reference tests do.
fn translate_records(resource: &Resource, scope: &InstrumentationScope, records: Vec<LogRecord>) -> Vec<Log> {
    let resource_logs = ResourceLogs {
        resource: Some(resource.clone()),
        scope_logs: vec![ScopeLogs {
            scope: Some(scope.clone()),
            log_records: records,
            ..Default::default()
        }],
        ..Default::default()
    };
    let resolver = OtlpOriginTagResolver::new(std::sync::Arc::new(NoopWorkloadProvider));
    ResourceLogsIter::new(resource_logs, &resolver).collect()
}

fn tags_of(log: &Log) -> BTreeSet<String> {
    log.tags().into_iter().map(|tag| tag.as_str().to_string()).collect()
}

/// Translates the case's log record, and returns the fields of the translated log that the reference tests check.
fn translate(case: &Case) -> Item {
    let mut logs = translate_records(&case.resource, &case.scope, vec![case.record.clone()]);
    assert_eq!(logs.len(), 1, "{}: expected one log", case.name);
    let log = logs.remove(0);

    // The fields are merged in the logs encoder's order: the additional properties come last, and overwrite the
    // fields before them, including the status.
    let mut json = Map::new();
    json.insert("message".to_string(), json!(log.message()));
    // The reference translator always sets the `status` property, empty when it derives no status.
    json.insert("status".to_string(), json!(log.status()));
    if !log.hostname().is_empty() {
        json.insert("hostname".to_string(), json!(log.hostname()));
    }
    if !log.service().is_empty() {
        json.insert("service".to_string(), json!(log.service()));
    }
    json.extend(
        log.additional_properties()
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone())),
    );
    Item {
        ddtags: tags_of(&log),
        json,
    }
}

fn check(cases: Vec<Case>) {
    for case in &cases {
        assert_eq!(translate(case), case.expected, "{}", case.name);
    }
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
            values: key_values(entries),
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

fn key_values<const N: usize>(entries: [(&str, AnyValue); N]) -> Vec<KeyValue> {
    entries
        .into_iter()
        .map(|(key, value)| KeyValue {
            key: key.to_string(),
            value: Some(value),
        })
        .collect()
}

/// Builds a log record, the way the reference tests fill in a `plog.LogRecord`.
#[derive(Default)]
struct Record(LogRecord);

impl Record {
    fn attr(mut self, key: &str, value: AnyValue) -> Self {
        self.0.attributes.extend(key_values([(key, value)]));
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

/// A record with an `app` attribute, the `TestTranslator` IDs, and a `service.name` attribute.
fn traced() -> Record {
    record()
        .attr("app", string("test"))
        .span_id(SPAN_ID)
        .trace_id(TRACE_ID)
        .attr("service.name", string("otlp_col"))
}

/// Returns every case ported from `TestTranslator`.
fn translator_cases() -> Vec<Case> {
    vec![
        case(
            "basic",
            record().attr("app", string("test")).severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({"app": "test", "status": "debug", "otel.severity_number": "5"}),
            ),
        ),
        case(
            "resource",
            record().attr("app", string("test")).severity_number(5),
            item(
                "service:otlp_col,otel_source:datadog_agent",
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
            ),
        )
        .resource([("service.name", string("otlp_col"))]),
        case(
            "append tags",
            record()
                .attr("app", string("test"))
                .attr("ddtags", string("foo:bar"))
                .severity_number(5),
            item(
                "service:otlp_col,foo:bar,otel_source:datadog_agent",
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
            ),
        )
        .resource([("service.name", string("otlp_col"))]),
        case(
            "service",
            record()
                .attr("app", string("test"))
                .attr("service.name", string("otlp_col"))
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "trace",
            traced().severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "trace from attributes",
            record()
                .attr("app", string("test"))
                .attr("spanid", string("2e26da881214cd7c"))
                .attr("traceid", string("437ab4d83468c540bb0f3398a39faa59"))
                .attr("service.name", string("otlp_col"))
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "otel.trace_id": "437ab4d83468c540bb0f3398a39faa59",
                    "dd.span_id": "3325585652813450620",
                    "dd.trace_id": "13479048940416379481",
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "trace from attributes (underscore)",
            record()
                .attr("app", string("test"))
                .attr("span_id", string("2e26da881214cd7c"))
                .attr("trace_id", string("740112b325075be8c80a48de336ebc67"))
                .attr("service.name", string("otlp_col"))
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "otel.trace_id": "740112b325075be8c80a48de336ebc67",
                    "dd.span_id": "3325585652813450620",
                    "dd.trace_id": "14414413676535528551",
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "trace from attributes decode error",
            record()
                .attr("app", string("test"))
                .attr("spanid", string("2e26da881214cd7c"))
                .attr("traceid", string("invalidtraceid"))
                .attr("service.name", string("otlp_col"))
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.span_id": "2e26da881214cd7c",
                    "dd.span_id": "3325585652813450620",
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "trace from attributes size error",
            record()
                .attr("app", string("test"))
                .attr("spanid", string("2023675201651514964"))
                .attr(
                    "traceid",
                    string("eb068afe5e53704f3b0dc3d3e1e397cb760549a7b58547db4f1dee845d9101f8db1ccf8fdd0976a9112f"),
                )
                .attr("service.name", string("otlp_col"))
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "SeverityText",
            traced().severity_text("alert").severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
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
            ),
        ),
        case(
            "body",
            traced().severity_number(13).body("This is log"),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
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
            ),
        ),
        case(
            "log-level",
            traced().attr("level", string("error")).body("This is log"),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "service": "otlp_col",
                    "message": "This is log",
                    "app": "test",
                    "status": "error",
                    "otel.span_id": SPAN_ID_HEX,
                    "otel.trace_id": TRACE_ID_HEX,
                    "dd.span_id": DD_SPAN_ID,
                    "dd.trace_id": DD_TRACE_ID,
                    "service.name": "otlp_col",
                }),
            ),
        ),
        case(
            "resource attributes in additional properties",
            record().attr("app", string("test")).severity_number(5),
            item(
                "service:otlp_col,otel_source:datadog_agent",
                json!({
                    "service": "otlp_col",
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "key": "val",
                    "service.name": "otlp_col",
                }),
            ),
        )
        .resource([("service.name", string("otlp_col")), ("key", string("val"))]),
        case(
            "DD hostname and service are not overridden by resource attributes",
            record().attr("app", string("test")).severity_number(5),
            item(
                "service:otlp_col,otel_source:datadog_agent",
                json!({
                    "app": "test",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.service": "otlp_col",
                    "otel.hostname": "example_host",
                }),
            ),
        )
        .resource([("hostname", string("example_host")), ("service", string("otlp_col"))]),
        case(
            "Nestings",
            record().attr(
                "root",
                map([
                    ("nest1", map([("nest2", string("val"))])),
                    ("nest12", map([("nest22", map([("nest3", string("val2"))]))])),
                    ("nest13", string("val3")),
                ]),
            ),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "root.nest1.nest2": "val",
                    "root.nest12.nest22.nest3": "val2",
                    "root.nest13": "val3",
                    "status": "",
                }),
            ),
        ),
        case("Nil Map", record(), item(OTEL_SOURCE_TAG, json!({"status": ""}))),
        case(
            "Too many nestings",
            record().attr(
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
            ),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "nest1.nest2.nest3.nest4.nest5.nest6.nest7.nest8.nest9.nest10": "{\"nest11\":{\"nest12\":\"ok\"}}",
                    "nest1.nest2.nest3.nest4.nest5.nest14.nest15": "ok2",
                    "status": "",
                }),
            ),
        ),
        case(
            "Timestamps are formatted properly",
            record().timestamp(1_700_499_303_397_000_000).severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "@timestamp": "2023-11-20T16:55:03.397Z",
                    "otel.timestamp": "1700499303397000000",
                }),
            ),
        ),
        case(
            "scope attributes",
            record().body("hello world").severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "hello world",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otelcol.component.id": "otlp",
                    "otelcol.component.kind": "Receiver",
                }),
            ),
        )
        .scope(
            "",
            "",
            [
                ("otelcol.component.id", string("otlp")),
                ("otelcol.component.kind", string("Receiver")),
            ],
        ),
        case(
            "ObservedTimestamp used as fallback when Timestamp is zero",
            record()
                .observed_timestamp(1_700_499_303_397_000_000)
                .severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "status": "debug",
                    "otel.severity_number": "5",
                    "@timestamp": "2023-11-20T16:55:03.397Z",
                    "otel.timestamp": "1700499303397000000",
                }),
            ),
        ),
    ]
}

/// Returns every case ported from `TestDeriveStatus`, as logs with only a severity number.
fn derive_status_cases() -> Vec<Case> {
    [
        ("trace3", 3, "trace"),
        ("trace4", 4, "trace"),
        ("debug5", 5, "debug"),
        ("debug7", 7, "debug"),
        ("debug8", 8, "debug"),
        ("info9", 9, "info"),
        ("info12", 12, "info"),
        ("warn13", 13, "warn"),
        ("warn16", 16, "warn"),
        ("error17", 17, "error"),
        ("error20", 20, "error"),
        ("fatal21", 21, "fatal"),
        ("fatal24", 24, "fatal"),
        ("undefined", 50, "error"),
    ]
    .into_iter()
    .map(|(name, severity_number, status)| {
        case(
            name,
            record().severity_number(severity_number),
            item(
                OTEL_SOURCE_TAG,
                json!({"status": status, "otel.severity_number": severity_number.to_string()}),
            ),
        )
    })
    .collect()
}

/// The `TestTranslator` case of a log with an event name.
fn event_name_cases() -> Vec<Case> {
    vec![case(
        "EventName is mapped to otel.event_name",
        record().event_name("app.screen.click").severity_number(5),
        item(
            OTEL_SOURCE_TAG,
            json!({
                "status": "debug",
                "otel.severity_number": "5",
                "otel.event_name": "app.screen.click",
            }),
        ),
    )]
}

/// The `TestTranslator` cases of scope names and versions, and of the `otel.library.*` aliases for them.
fn scope_metadata_cases() -> Vec<Case> {
    vec![
        case(
            "scope name",
            record().body("hello world").severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "hello world",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "go.opentelemetry.io/contrib/bridges/otelslog",
                }),
            ),
        )
        .scope("go.opentelemetry.io/contrib/bridges/otelslog", "", []),
        case(
            "scope version",
            record().body("hello world").severity_number(5),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "hello world",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "go.opentelemetry.io/contrib/bridges/otelslog",
                    "otel.scope.version": "v1.2.3",
                }),
            ),
        )
        .scope("go.opentelemetry.io/contrib/bridges/otelslog", "v1.2.3", []),
        case(
            "library name/version aliases remapped to scope",
            record()
                .body("hello world")
                .severity_number(5)
                .attr("otel.library.name", string("my-library"))
                .attr("otel.library.version", string("v0.1.0")),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "hello world",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "my-library",
                    "otel.scope.version": "v0.1.0",
                }),
            ),
        ),
        case(
            "explicit otel.scope.name attribute takes precedence over library alias and scope metadata",
            record()
                .body("hello world")
                .severity_number(5)
                .attr("otel.scope.name", string("explicit-scope-name"))
                .attr("otel.library.name", string("library-alias-name")),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "hello world",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "otel.scope.name": "explicit-scope-name",
                }),
            ),
        )
        .scope("scope-metadata-name", "", []),
    ]
}

/// The `TestTranslator` cases of array attributes, which the reference translator keeps as JSON arrays.
fn array_attributes_cases() -> Vec<Case> {
    vec![
        case(
            "array attribute with strings",
            record()
                .body("test array attribute")
                .severity_number(5)
                .attr("test_array", strings(["value1", "value2", "value3"])),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "test array attribute",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "test_array": ["value1", "value2", "value3"],
                }),
            ),
        ),
        case(
            "array attribute with name-value maps",
            record().body("test array with maps").severity_number(5).attr(
                "array_with_maps",
                array([
                    map([("item_name", string("item1")), ("value", int(100))]),
                    map([("item_name", string("item2")), ("value", int(200))]),
                ]),
            ),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "test array with maps",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "array_with_maps": [
                        {"item_name": "item1", "value": 100},
                        {"item_name": "item2", "value": 200},
                    ],
                }),
            ),
        ),
        case(
            "multiple nesting",
            record().body("test multiple nesting").severity_number(5).attr(
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
            ),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "test multiple nesting",
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
            ),
        ),
        case(
            "slice of slices of maps",
            record().body("test slice of slices of maps").severity_number(5).attr(
                "matrix",
                array([0, 1, 2].map(|row| {
                    array([0, 1].map(|col| {
                        let value = ["a", "b", "c", "d", "e", "f"][(row * 2 + col) as usize];
                        map([("row", int(row)), ("col", int(col)), ("value", string(value))])
                    }))
                })),
            ),
            item(
                OTEL_SOURCE_TAG,
                json!({
                    "message": "test slice of slices of maps",
                    "status": "debug",
                    "otel.severity_number": "5",
                    "matrix": [
                        [{"row": 0, "col": 0, "value": "a"}, {"row": 0, "col": 1, "value": "b"}],
                        [{"row": 1, "col": 0, "value": "c"}, {"row": 1, "col": 1, "value": "d"}],
                        [{"row": 2, "col": 0, "value": "e"}, {"row": 2, "col": 1, "value": "f"}],
                    ],
                }),
            ),
        ),
    ]
}

/// Cases of resource and scope attributes named `status`, which the reference translator replaces with the derived
/// status. These aren't ported: `TestTranslator` has no such case.
fn status_attribute_override_cases() -> Vec<Case> {
    vec![
        case(
            "resource status attribute is replaced by the derived status",
            record().severity_number(5),
            item(OTEL_SOURCE_TAG, json!({"status": "debug", "otel.severity_number": "5"})),
        )
        .resource([("status", string("resource-status"))]),
        case(
            "scope status attribute is replaced by the derived status",
            record().severity_number(5),
            item(OTEL_SOURCE_TAG, json!({"status": "debug", "otel.severity_number": "5"})),
        )
        .scope("", "", [("status", string("scope-status"))]),
        case(
            "resource status attribute is dropped when no status is derived",
            record(),
            item(OTEL_SOURCE_TAG, json!({"status": ""})),
        )
        .resource([("status", string("resource-status"))]),
    ]
}

#[test]
fn translator() {
    let mut cases = translator_cases();
    cases.extend(derive_status_cases());
    cases.extend(status_attribute_override_cases());
    check(cases);
}

#[test]
#[ignore = "target for `gap/event-name`"]
fn target_event_name() {
    check(event_name_cases());
}

#[test]
#[ignore = "target for `gap/scope-metadata`"]
fn target_scope_metadata() {
    check(scope_metadata_cases());
}

#[test]
#[ignore = "target for `gap/array-attributes`"]
fn target_array_attributes() {
    check(array_attributes_cases());
}

// `TestLogsExporter` uses the records of `comp/otelcol/otlp/testutil`.
const TEST_LOG_TIME_UNIX_NANO: u64 = 1_581_452_773_000_000_789;
const TEST_LOG_TIME: &str = "2020-02-11T20:26:13.000Z";
const LOG_ONE_SPAN_ID: [u8; 8] = [0x01, 0x02, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00];
const LOG_ONE_TRACE_ID: [u8; 16] = [
    0x08, 0x04, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The log source name of OTLP logs, used unless `datadog.log.source` is set.
const DEFAULT_LOG_SOURCE: &str = "otlp_log_ingestion";

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
    let mut attributes = key_values([("resource-attr", string("resource-attr-val-1"))]);
    attributes.extend(key_values(extra));
    Resource {
        attributes,
        ..Default::default()
    }
}

/// The content of `fillLogOne`, merged with `overrides`.
fn log_one_content(overrides: Value) -> Value {
    let mut content = json!({
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
    if let (Value::Object(content), Value::Object(overrides)) = (&mut content, overrides) {
        content.extend(overrides);
    }
    content
}

/// The content of `fillLogTwo`.
fn log_two_content() -> Value {
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

/// A log that the exporter sends, as the reference test checks it.
#[derive(Debug, PartialEq)]
struct Sent {
    content: Value,
    source: String,
    tags: BTreeSet<String>,
}

fn sent(content: Value, source: &str, tags: &str) -> Sent {
    Sent {
        content,
        source: source.to_string(),
        tags: tags.split(',').map(str::to_string).collect(),
    }
}

/// A `TestLogsExporter` case: the records of one resource, and the logs that the exporter sends for them.
struct ExporterCase {
    name: &'static str,
    resource: Resource,
    records: Vec<Record>,
    expected: Vec<Sent>,
}

fn check_exporter(cases: Vec<ExporterCase>) {
    let mut renderer = ContentRenderer::default();
    for case in cases {
        let records = case.records.into_iter().map(|record| record.0).collect();
        let sent = translate_records(&case.resource, &InstrumentationScope::default(), records)
            .iter()
            .map(|log| {
                let content = renderer.render(log).expect("content should render");
                Sent {
                    content: serde_json::from_str(&content).expect("content should be JSON"),
                    source: log.source().as_deref().unwrap_or_default().to_string(),
                    tags: tags_of(log),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(sent, case.expected, "{}", case.name);
    }
}

/// Returns the `TestLogsExporter` cases that the translator and renderer match.
fn exporter_cases() -> Vec<ExporterCase> {
    vec![
        ExporterCase {
            name: "message",
            resource: testutil_resource([]),
            records: vec![log_one()],
            expected: vec![sent(log_one_content(json!({})), DEFAULT_LOG_SOURCE, OTEL_SOURCE_TAG)],
        },
        ExporterCase {
            name: "status",
            resource: testutil_resource([]),
            records: vec![log_one().severity_text("Fatal")],
            expected: vec![sent(
                log_one_content(json!({"status": "Fatal", "otel.severity_text": "Fatal"})),
                DEFAULT_LOG_SOURCE,
                OTEL_SOURCE_TAG,
            )],
        },
        ExporterCase {
            name: "ddtags",
            resource: testutil_resource([]),
            records: vec![log_one().attr("ddtags", string("tag1:true"))],
            expected: vec![sent(
                log_one_content(json!({})),
                DEFAULT_LOG_SOURCE,
                "tag1:true,otel_source:datadog_agent",
            )],
        },
        ExporterCase {
            name: "ddtags-same-tags",
            resource: testutil_resource([]),
            records: vec![
                log_one().attr("ddtags", string("tag1:true")),
                log_two().attr("ddtags", string("tag1:true")),
            ],
            expected: vec![
                sent(
                    log_one_content(json!({})),
                    DEFAULT_LOG_SOURCE,
                    "tag1:true,otel_source:datadog_agent",
                ),
                sent(
                    log_two_content(),
                    DEFAULT_LOG_SOURCE,
                    "tag1:true,otel_source:datadog_agent",
                ),
            ],
        },
        ExporterCase {
            name: "ddtags-different-tags",
            resource: testutil_resource([]),
            records: vec![
                log_one().attr("ddtags", string("tag1:true")),
                log_two().attr("ddtags", string("tag2:true")),
            ],
            expected: vec![
                sent(
                    log_one_content(json!({})),
                    DEFAULT_LOG_SOURCE,
                    "tag1:true,otel_source:datadog_agent",
                ),
                sent(
                    log_two_content(),
                    DEFAULT_LOG_SOURCE,
                    "tag2:true,otel_source:datadog_agent",
                ),
            ],
        },
    ]
}

/// The `TestLogsExporter` cases of a `datadog.log.source` attribute, which the reference exporter uses as the source.
fn log_source_attribute_cases() -> Vec<ExporterCase> {
    vec![
        ExporterCase {
            name: "message-attribute",
            resource: testutil_resource([]),
            records: vec![log_one()
                .attr("message", string("hello"))
                .attr("datadog.log.source", string("custom_source"))
                .attr("host.name", string("test-host"))],
            expected: vec![sent(
                log_one_content(json!({
                    "message": "hello",
                    "datadog.log.source": "custom_source",
                    "host.name": "test-host",
                    "hostname": "test-host",
                })),
                "custom_source",
                OTEL_SOURCE_TAG,
            )],
        },
        ExporterCase {
            name: "resource-attribute-source",
            resource: testutil_resource([("datadog.log.source", string("custom_source_rattr"))]),
            records: vec![log_one()],
            expected: vec![sent(
                log_one_content(json!({"datadog.log.source": "custom_source_rattr"})),
                "custom_source_rattr",
                OTEL_SOURCE_TAG,
            )],
        },
    ]
}

#[test]
fn exporter() {
    check_exporter(exporter_cases());
}

#[test]
#[ignore = "target for `gap/log-source-attribute`"]
fn target_log_source_attribute() {
    check_exporter(log_source_attribute_cases());
}

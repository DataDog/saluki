//! Translator tests ported from the reference implementation.
//!
//! Ported from `TestTranslator` and `TestDeriveStatus` in
//! <https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go>.
//!
//! Those tests compare the JSON of the `datadogV2.HTTPLogItem` that the reference translator returns for one log
//! record. Each case here compares the same JSON for the translated log, with the log's status as the item's `status`
//! property. The reference tests build their translator with the OTel source `test`, so their tags end in
//! `otel_source:test`. This translator always tags logs with `otel_source:datadog_agent`, so the expected tags here
//! use that instead.
//!
//! Cases that the translator doesn't match yet are in ignored `target_*` tests, one per gap. A change that closes a
//! gap moves its cases into `translator_cases`, and removes the target test.
//!
//! `TestTranslatorWithRUMRouting` isn't ported: the logs exporter doesn't route RUM events.

use std::collections::BTreeSet;

use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
};
use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_core::data_model::event::Event;
use saluki_env::workload::providers::NoopWorkloadProvider;
use serde_json::{json, Map, Value};

use super::translator::OtlpLogsTranslator;
use crate::common::otlp::origin::OtlpOriginTagResolver;

/// A translated log, as the reference tests compare it: the JSON of the `datadogV2.HTTPLogItem`.
///
/// The JSON has the item's `message`, `hostname`, and `service`, overwritten by its additional properties, as
/// `HTTPLogItem.MarshalJSON` writes them. The tags are a set: the reference tests compare them as a string, but their
/// order doesn't matter to the intake.
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

/// Translates the case's log record, and returns the fields of the translated log that the reference tests check.
fn translate(case: &Case) -> Item {
    let resource_logs = ResourceLogs {
        resource: Some(case.resource.clone()),
        scope_logs: vec![ScopeLogs {
            scope: Some(case.scope.clone()),
            log_records: vec![case.record.clone()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let resolver = OtlpOriginTagResolver::new(std::sync::Arc::new(NoopWorkloadProvider));
    let mut events = OtlpLogsTranslator::from_resource_logs(resource_logs, &resolver).collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{}: expected one log", case.name);
    let Event::Log(log) = events.remove(0) else {
        panic!("{}: expected a log event", case.name);
    };

    let mut json = Map::new();
    json.insert("message".to_string(), json!(log.message()));
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
    // The reference translator always sets the `status` property, empty when it derives no status.
    json.insert("status".to_string(), json!(log.status()));
    Item {
        ddtags: log.tags().into_iter().map(|tag| tag.as_str().to_string()).collect(),
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
            "append-tags",
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
            "trace-from-attributes",
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
            "trace-from-attributes-underscore",
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
            "trace-from-attributes-decode-error",
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
            "trace-from-attributes-size-error",
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
            "severity-text",
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
            "resource-attributes",
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
            "reserved-resource-attributes",
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
            "nestings",
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
        case("nil-map", record(), item(OTEL_SOURCE_TAG, json!({"status": ""}))),
        case(
            "too-many-nestings",
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
            "timestamp",
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
            "scope-attributes",
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

/// The `TestTranslator` case of a log with only an observed timestamp, which the reference translator uses as
/// the log's timestamp.
fn observed_timestamp_cases() -> Vec<Case> {
    vec![case(
        "observed-timestamp",
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
    )]
}

/// The `TestTranslator` case of a log with an event name.
fn event_name_cases() -> Vec<Case> {
    vec![case(
        "event-name",
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
            "scope-name",
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
            "scope-version",
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
            "library-aliases",
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
            "explicit-scope-name",
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
            "array-of-strings",
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
            "array-of-maps",
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
            "multiple-nesting",
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
            "slice-of-slices-of-maps",
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

#[test]
fn translator() {
    let mut cases = translator_cases();
    cases.extend(derive_status_cases());
    check(cases);
}

#[test]
#[ignore = "target for `gap/observed-timestamp`"]
fn target_observed_timestamp() {
    check(observed_timestamp_cases());
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

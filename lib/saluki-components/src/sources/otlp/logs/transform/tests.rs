use otlp_common::{any_value::Value, AnyValue, KeyValue, KeyValueList};
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_core::data_model::tags::TagSet;
use serde_json::json;

use super::*;

fn string(value: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(value.to_string())),
    }
}

fn key_value(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(value),
    }
}

fn transform_attributes(attributes: Vec<KeyValue>) -> Log {
    let lr = LogRecord {
        attributes,
        ..Default::default()
    };
    transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default())
}

#[test]
fn ordinary_attribute_keys_keep_their_spelling() {
    let log = transform_attributes(vec![
        key_value("RequestID", string("abc")),
        // Only the lowercase spellings are special to the rendered content, so these are ordinary attributes.
        key_value("Hostname", string("attribute-host")),
        key_value("Service", string("attribute-service")),
        key_value(
            "nested_Map",
            AnyValue {
                value: Some(Value::KvlistValue(KeyValueList {
                    values: vec![key_value("InnerKey", string("inner"))],
                })),
            },
        ),
    ]);

    let props = log.additional_properties();
    assert_eq!(props.get("RequestID"), Some(&json!("abc")));
    assert_eq!(props.get("nested_Map.InnerKey"), Some(&json!("inner")));
    assert_eq!(props.get("Hostname"), Some(&json!("attribute-host")));
    assert_eq!(props.get("Service"), Some(&json!("attribute-service")));
    assert!(!props.contains_key("requestid"));
    assert!(!props.contains_key("nested_map.InnerKey"));
    assert!(!props.contains_key("hostname"));
    assert!(!props.contains_key("service"));
}

#[test]
fn reserved_attribute_keys_match_in_any_case() {
    let log = transform_attributes(vec![
        key_value("MESSAGE", string("from attribute")),
        key_value("Level", string("warn")),
        key_value("TraceID", string("0802030405060708000000000a000000")),
        key_value("Span_ID", string("000000000a000000")),
        key_value("DDTags", string("team:a,env:prod")),
    ]);

    assert_eq!(log.message(), "from attribute");
    assert_eq!(log.status(), "warn");
    assert!(log.tags().has_tag("team:a"));
    assert!(log.tags().has_tag("env:prod"));

    let props = log.additional_properties();
    assert_eq!(props.get("dd.trace_id"), Some(&json!("167772160")));
    assert_eq!(
        props.get("otel.trace_id"),
        Some(&json!("0802030405060708000000000a000000"))
    );
    assert_eq!(props.get("dd.span_id"), Some(&json!("167772160")));
    assert_eq!(props.get("otel.span_id"), Some(&json!("000000000a000000")));
    // Reserved keys are consumed, so none of them appear as attributes under any spelling.
    let mut keys = props.keys().map(|key| key.as_ref()).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, ["dd.span_id", "dd.trace_id", "otel.span_id", "otel.trace_id"]);
}

#[test]
fn duplicate_reserved_keys_resolve_as_the_reference() {
    const TRACE_A: &str = "0802030405060708000000000a000000";
    const TRACE_B: &str = "000000000000000000000000000000FF";
    const SPAN_A: &str = "000000000a000000";
    const SPAN_B: &str = "00000000000000FF";

    struct Case {
        name: &'static str,
        attributes: &'static [(&'static str, Option<&'static str>)],
        message: &'static str,
        status: &'static str,
        tags: &'static [&'static str],
        trace_id: Option<&'static str>,
        span_id: Option<&'static str>,
    }

    // Every record has the body `body`, the severity text `notice`, and the resource tag `res:tag`.
    let cases = [
        Case {
            name: "the last message wins",
            attributes: &[
                ("message", Some("first")),
                ("MESSAGE", Some("second")),
                ("Log", Some("third")),
            ],
            message: "third",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "an empty last message falls back to the body",
            attributes: &[("msg", Some("first")), ("Message", Some(""))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "a last message without a value falls back to the body",
            attributes: &[("message", Some("first")), ("MESSAGE", None)],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "the last status wins",
            attributes: &[
                ("status", Some("info")),
                ("Level", Some("warn")),
                ("SYSLOG.SEVERITY", Some("error")),
            ],
            message: "body",
            status: "error",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "an empty last status falls back to the severity text",
            attributes: &[("severity", Some("error")), ("LEVEL", Some(""))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "a last status without a value falls back to the severity text",
            attributes: &[("status", Some("warn")), ("Level", None)],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "the last ddtags wins after the resource tags",
            attributes: &[("ddtags", Some("a:1,b:2")), ("DDTags", Some("c:3"))],
            message: "body",
            status: "notice",
            tags: &["c:3", "res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "an empty last ddtags leaves only the resource tags",
            attributes: &[("ddtags", Some("a:1")), ("DDTAGS", Some(""))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "a last ddtags without a value leaves only the resource tags",
            attributes: &[("ddtags", Some("a:1")), ("DDTags", None)],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: None,
        },
        Case {
            name: "the first trace ID wins",
            attributes: &[("traceid", Some(TRACE_A)), ("Trace_Id", Some(TRACE_B))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: Some(TRACE_A),
            span_id: None,
        },
        Case {
            name: "a trace ID that doesn't decode is skipped",
            attributes: &[("trace_id", Some("not-hex")), ("OtelTraceID", Some(TRACE_B))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: Some(TRACE_B),
            span_id: None,
        },
        Case {
            name: "the first span ID wins",
            attributes: &[("spanid", Some(SPAN_A)), ("SPAN_ID", Some(SPAN_B))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: Some(SPAN_A),
        },
        Case {
            name: "a span ID that doesn't decode is skipped",
            attributes: &[("span_id", Some("0a")), ("ContextMap.SpanID", Some(SPAN_B))],
            message: "body",
            status: "notice",
            tags: &["res:tag"],
            trace_id: None,
            span_id: Some(SPAN_B),
        },
    ];

    for case in cases {
        let lr = LogRecord {
            attributes: case
                .attributes
                .iter()
                .map(|(key, value)| KeyValue {
                    key: key.to_string(),
                    value: value.map(string),
                })
                .collect(),
            body: Some(string("body")),
            severity_text: "notice".to_string(),
            ..Default::default()
        };
        let mut tags = TagSet::default();
        tags.insert_tag("res:tag");
        let log = transform_log_record(lr, &Resource::default(), None, None, None, tags);

        assert_eq!(log.message(), case.message, "{}: message", case.name);
        assert_eq!(log.status(), case.status, "{}: status", case.name);
        let mut tags = log.tags().into_iter().map(|tag| tag.as_str()).collect::<Vec<_>>();
        tags.sort_unstable();
        assert_eq!(tags, case.tags, "{}: tags", case.name);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.trace_id"),
            case.trace_id.map(|id| json!(id)).as_ref(),
            "{}: trace ID",
            case.name
        );
        assert_eq!(
            props.get("otel.span_id"),
            case.span_id.map(|id| json!(id)).as_ref(),
            "{}: span ID",
            case.name
        );
    }
}

proptest::proptest! {
    #[test]
    fn property_test_timestamps_are_utc_milliseconds_rounded_down(time_unix_nano in 1u64..) {
        let lr = LogRecord {
            time_unix_nano,
            ..Default::default()
        };
        let log = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default());
        let formatted = log.additional_properties()["@timestamp"]
            .as_str()
            .expect("@timestamp should be a string")
            .to_string();

        // Nanoseconds are read as signed, and every signed value is within the years 1677 to 2262.
        let nanos = i128::from(time_unix_nano as i64);
        let parsed = formatted.parse::<Timestamp>().expect("@timestamp should be RFC 3339");
        proptest::prop_assert_eq!(formatted.len(), 24);
        proptest::prop_assert!(formatted.ends_with('Z'));
        proptest::prop_assert_eq!(parsed.as_unix_nanos(), nanos.div_euclid(1_000_000) * 1_000_000);
    }
}

#[test]
fn timestamps_fall_back_to_the_observed_timestamp() {
    // (timestamp, observed timestamp, `otel.timestamp`, `@timestamp`)
    let cases = [
        (
            1_700_499_303_397_999_999,
            0,
            Some("1700499303397999999"),
            Some("2023-11-20T16:55:03.397Z"),
        ),
        // The timestamp wins over the observed timestamp, and the observed timestamp fills in when it's 0.
        (
            1_700_499_303_397_000_000,
            1_600_000_000_000_000_000,
            Some("1700499303397000000"),
            Some("2023-11-20T16:55:03.397Z"),
        ),
        (
            0,
            1_700_499_303_397_000_000,
            Some("1700499303397000000"),
            Some("2023-11-20T16:55:03.397Z"),
        ),
        // Neither is set.
        (0, 0, None, None),
        // Nanoseconds are read as signed, so timestamps past `i64::MAX` wrap to before 1970.
        (
            1 << 63,
            0,
            Some("-9223372036854775808"),
            Some("1677-09-21T00:12:43.145Z"),
        ),
        (0, u64::MAX, Some("-1"), Some("1969-12-31T23:59:59.999Z")),
        // Milliseconds are truncated, never rounded, and a whole second still has three fraction digits.
        (1, 0, Some("1"), Some("1970-01-01T00:00:00.000Z")),
        (999_999, 0, Some("999999"), Some("1970-01-01T00:00:00.000Z")),
        (1_000_000, 0, Some("1000000"), Some("1970-01-01T00:00:00.001Z")),
        (
            1_700_499_303_999_999_999,
            0,
            Some("1700499303999999999"),
            Some("2023-11-20T16:55:03.999Z"),
        ),
        (
            1_700_499_303_000_000_000,
            0,
            Some("1700499303000000000"),
            Some("2023-11-20T16:55:03.000Z"),
        ),
        // The ends of the signed range, and times before 1970 round down to the earlier millisecond.
        (
            i64::MAX as u64,
            0,
            Some("9223372036854775807"),
            Some("2262-04-11T23:47:16.854Z"),
        ),
        (
            u64::MAX - 999_999,
            0,
            Some("-1000000"),
            Some("1969-12-31T23:59:59.999Z"),
        ),
        (
            u64::MAX - 999_999_998,
            0,
            Some("-999999999"),
            Some("1969-12-31T23:59:59.000Z"),
        ),
    ];

    for (time_unix_nano, observed_time_unix_nano, otel_timestamp, timestamp) in cases {
        let lr = LogRecord {
            time_unix_nano,
            observed_time_unix_nano,
            ..Default::default()
        };
        let log = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default());

        let props = log.additional_properties();
        let context = format!("timestamp {}, observed {}", time_unix_nano, observed_time_unix_nano);
        assert_eq!(
            props.get("otel.timestamp"),
            otel_timestamp.map(|ts| json!(ts)).as_ref(),
            "{}",
            context
        );
        assert_eq!(
            props.get("@timestamp"),
            timestamp.map(|ts| json!(ts)).as_ref(),
            "{}",
            context
        );
    }
}

#[test]
fn event_name_overwrites_attributes_with_the_same_key() {
    // (event name, record attribute, resource attribute, scope attribute, `otel.event_name`)
    let cases = [
        ("event", None, None, None, Some("event")),
        ("", None, None, None, None),
        // The event name wins over an attribute from each source.
        ("event", Some("record"), None, None, Some("event")),
        ("event", None, Some("resource"), None, Some("event")),
        ("event", None, None, Some("scope"), Some("event")),
        // Without an event name, the attributes keep their usual precedence.
        ("", Some("record"), None, None, Some("record")),
        ("", Some("record"), Some("resource"), None, Some("resource")),
        ("", Some("record"), Some("resource"), Some("scope"), Some("scope")),
    ];

    for (event_name, record_attr, resource_attr, scope_attr, expected) in cases {
        let attributes = |value: Option<&str>| {
            value
                .map(|value| vec![key_value("otel.event_name", string(value))])
                .unwrap_or_default()
        };
        let lr = LogRecord {
            event_name: event_name.to_string(),
            attributes: attributes(record_attr),
            ..Default::default()
        };
        let resource = Resource {
            attributes: attributes(resource_attr),
            ..Default::default()
        };
        let scope = otlp_common::InstrumentationScope {
            attributes: attributes(scope_attr),
            ..Default::default()
        };
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default());

        assert_eq!(
            log.additional_properties().get("otel.event_name"),
            expected.map(|value| json!(value)).as_ref(),
            "event name {:?}, record {:?}, resource {:?}, scope {:?}",
            event_name,
            record_attr,
            resource_attr,
            scope_attr
        );
    }
}

#[test]
fn scope_metadata_follows_the_reference_precedence() {
    type Attributes = &'static [(&'static str, &'static str)];
    struct Case {
        name: &'static str,
        record: Attributes,
        resource: Attributes,
        scope_attributes: Attributes,
        scope_name: &'static str,
        scope_version: &'static str,
        expected: Attributes,
    }
    const NONE: Attributes = &[];
    let case = |name| Case {
        name,
        record: NONE,
        resource: NONE,
        scope_attributes: NONE,
        scope_name: "",
        scope_version: "",
        expected: NONE,
    };

    let cases = [
        // Record attributes: an alias maps to its canonical key, unless the record also has the canonical key.
        Case {
            record: &[("otel.library.name", "alias")],
            expected: &[("otel.scope.name", "alias")],
            ..case("a record alias maps to the canonical key")
        },
        Case {
            record: &[("otel.scope.name", "canonical"), ("otel.library.name", "alias")],
            expected: &[("otel.scope.name", "canonical")],
            ..case("a record alias defers to an earlier record canonical key")
        },
        Case {
            record: &[("otel.library.name", "alias"), ("otel.scope.name", "canonical")],
            expected: &[("otel.scope.name", "canonical")],
            ..case("a record alias defers to a later record canonical key")
        },
        Case {
            record: &[("OTEL.Library.Version", "alias")],
            expected: &[("otel.scope.version", "alias")],
            ..case("a record alias matches in any case")
        },
        Case {
            record: &[("otel.library.name", "alias"), ("OTEL.SCOPE.NAME", "canonical")],
            expected: &[("otel.scope.name", "alias"), ("OTEL.SCOPE.NAME", "canonical")],
            ..case("a record alias defers only to a canonical key spelled exactly")
        },
        // Resource attributes: the same within the resource, and they overwrite record-derived values.
        Case {
            resource: &[("otel.library.version", "alias")],
            expected: &[("otel.scope.version", "alias")],
            ..case("a resource alias maps to the canonical key")
        },
        Case {
            resource: &[("otel.library.name", "alias"), ("otel.scope.name", "canonical")],
            expected: &[("otel.scope.name", "canonical")],
            ..case("a resource alias defers to a resource canonical key")
        },
        Case {
            resource: &[("OTEL.LIBRARY.NAME", "alias")],
            expected: &[("OTEL.LIBRARY.NAME", "alias")],
            ..case("a resource alias matches only in lowercase")
        },
        Case {
            record: &[("otel.scope.name", "record")],
            resource: &[("otel.library.name", "resource")],
            expected: &[("otel.scope.name", "resource")],
            ..case("a resource alias overwrites a record canonical key")
        },
        Case {
            record: &[("otel.library.name", "record")],
            resource: &[("otel.scope.name", "resource")],
            expected: &[("otel.scope.name", "resource")],
            ..case("a resource canonical key overwrites a record alias")
        },
        // Scope attributes: they overwrite both, under their own keys.
        Case {
            record: &[("otel.scope.name", "record")],
            resource: &[("otel.scope.name", "resource")],
            scope_attributes: &[("otel.scope.name", "scope")],
            expected: &[("otel.scope.name", "scope")],
            ..case("a scope attribute overwrites record and resource values")
        },
        Case {
            scope_attributes: &[("otel.library.name", "alias")],
            expected: &[("otel.library.name", "alias")],
            ..case("a scope attribute alias keeps its key")
        },
        Case {
            resource: &[("hostname", "resource")],
            scope_attributes: &[("hostname", "scope")],
            expected: &[("otel.hostname", "resource"), ("hostname", "scope")],
            ..case("a scope attribute hostname isn't prefixed")
        },
        // The instrumentation scope's name and version: they only fill in missing keys.
        Case {
            scope_name: "name",
            scope_version: "version",
            expected: &[("otel.scope.name", "name"), ("otel.scope.version", "version")],
            ..case("the scope name and version fill in missing keys")
        },
        Case {
            record: &[("otel.scope.name", "record")],
            scope_name: "name",
            expected: &[("otel.scope.name", "record")],
            ..case("the scope name defers to a record canonical key")
        },
        Case {
            record: &[("otel.library.name", "alias")],
            scope_name: "name",
            expected: &[("otel.scope.name", "alias")],
            ..case("the scope name defers to a record alias")
        },
        Case {
            resource: &[("otel.scope.version", "resource")],
            scope_version: "version",
            expected: &[("otel.scope.version", "resource")],
            ..case("the scope version defers to a resource canonical key")
        },
        Case {
            scope_attributes: &[("otel.scope.name", "scope")],
            scope_name: "name",
            expected: &[("otel.scope.name", "scope")],
            ..case("the scope name defers to a scope attribute")
        },
        Case {
            scope_attributes: &[("otel.library.name", "alias")],
            scope_name: "name",
            expected: &[("otel.library.name", "alias"), ("otel.scope.name", "name")],
            ..case("the scope name doesn't defer to a scope attribute alias")
        },
    ];

    let attributes =
        |attrs: Attributes| -> Vec<KeyValue> { attrs.iter().map(|(k, v)| key_value(k, string(v))).collect() };
    for case in cases {
        let lr = LogRecord {
            attributes: attributes(case.record),
            ..Default::default()
        };
        let resource = Resource {
            attributes: attributes(case.resource),
            ..Default::default()
        };
        let scope = otlp_common::InstrumentationScope {
            name: case.scope_name.to_string(),
            version: case.scope_version.to_string(),
            attributes: attributes(case.scope_attributes),
            ..Default::default()
        };
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default());

        let actual = log
            .additional_properties()
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = case
            .expected
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(actual, expected, "{}", case.name);
    }
}

#[test]
fn derive_status_follows_the_reference_precedence() {
    // (status attribute, severity text, severity number, status)
    let cases = [
        // The status attribute comes first, then the severity text, then the severity number.
        (Some("error"), "alert", 5, "error"),
        (None, "alert", 5, "alert"),
        (None, "", 5, "debug"),
        // Attribute and severity text are kept as they are, even when they aren't a known status.
        (Some("Not A Status"), "", 0, "Not A Status"),
        (None, "Information", 9, "Information"),
        (None, "WARN", 0, "WARN"),
        // An empty attribute or severity text falls through to the next source.
        (Some(""), "notice", 13, "notice"),
        (Some(""), "", 13, "warn"),
        // Severity number 0 has no name, so a log with nothing else has no status.
        (None, "", 0, ""),
        (Some(""), "", 0, ""),
    ];

    for (status, severity_text, severity_number, expected) in cases {
        assert_eq!(
            derive_status(status, severity_text, severity_number),
            expected,
            "derive_status({:?}, {:?}, {})",
            status,
            severity_text,
            severity_number
        );
    }
}

#[test]
fn status_from_severity_number_matches_the_reference() {
    // Ported from `TestDeriveStatus` in the reference implementation:
    // https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go
    // with the boundaries of every range, and negative numbers, which the reference implementation treats as trace.
    let cases = [
        (-1, "trace"),
        (1, "trace"),
        (3, "trace"),
        (4, "trace"),
        (5, "debug"),
        (7, "debug"),
        (8, "debug"),
        (9, "info"),
        (12, "info"),
        (13, "warn"),
        (16, "warn"),
        (17, "error"),
        (20, "error"),
        (21, "fatal"),
        (24, "fatal"),
        (25, "error"),
        (50, "error"),
    ];

    for (severity_number, expected) in cases {
        assert_eq!(
            status_from_severity_number(severity_number),
            expected,
            "status_from_severity_number({})",
            severity_number
        );
    }
}

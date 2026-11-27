use otlp_common::{any_value::Value, AnyValue, KeyValue, KeyValueList};
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_core::data_model::tags::TagSet;
use serde_json::{json, Value as JsonValue};

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
    transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default(), None)
        .expect("the record should transform")
}

fn value(value: Value) -> AnyValue {
    AnyValue { value: Some(value) }
}

fn empty() -> AnyValue {
    AnyValue { value: None }
}

fn map(entries: Vec<(&str, AnyValue)>) -> AnyValue {
    value(Value::KvlistValue(KeyValueList {
        values: entries.into_iter().map(|(key, value)| key_value(key, value)).collect(),
    }))
}

fn array(values: Vec<AnyValue>) -> AnyValue {
    value(Value::ArrayValue(otlp_common::ArrayValue { values }))
}

/// Nests `leaf` in `depth` maps, keyed `m1` to `m<depth>` from the outside in.
fn nested_maps(depth: usize, leaf: AnyValue) -> AnyValue {
    (1..=depth)
        .rev()
        .fold(leaf, |inner, level| map(vec![(&format!("m{}", level), inner)]))
}

/// Nests `leaf` in `depth` arrays of one map each, keyed `level1` to `level<depth>` from the outside in.
fn nested_through_arrays(depth: usize, leaf: AnyValue) -> AnyValue {
    (1..=depth).rev().fold(leaf, |inner, level| {
        array(vec![map(vec![(&format!("level{}", level), inner)])])
    })
}

/// Returns the log's message, status, and additional properties as one JSON object, as the Agent's
/// `HTTPLogItem` holds them without its `ddtags`.
fn item_json(log: &Log) -> JsonValue {
    let mut item = serde_json::Map::new();
    item.insert("message".to_string(), json!(log.message()));
    item.insert("status".to_string(), json!(log.status()));
    for (key, value) in log.additional_properties() {
        item.insert(key.to_string(), value.clone());
    }
    JsonValue::Object(item)
}

#[test]
fn values_coerce_as_the_reference() {
    // Each target is what the Agent's `transform` returns for the record, marshaled by `HTTPLogItem.MarshalJSON`
    // without its `ddtags`, at the Agent version that `_version.txt` pins.
    struct Case {
        name: &'static str,
        record: Vec<KeyValue>,
        body: Option<AnyValue>,
        resource: Vec<KeyValue>,
        scope: Vec<KeyValue>,
        target: &'static str,
    }
    let case = |name, record| Case {
        name,
        record,
        body: None,
        resource: vec![],
        scope: vec![],
        target: "",
    };
    let body = |name, body| Case {
        body: Some(body),
        ..case(name, vec![])
    };

    let cases = [
        Case {
            target: r#"{"array":[1,"","aGk=",[true],{},"collapsed",{"a.b":2}],"bytes":"/wBh","bytes_empty":"","empty":"","map.inner_bytes":"aGk=","map.inner_value_empty":"","message":"","spread":"x","status":""}"#,
            ..case(
                "record values keep their types, and other values become strings",
                vec![
                    key_value("empty", empty()),
                    key_value("bytes_empty", value(Value::BytesValue(vec![]))),
                    key_value("bytes", value(Value::BytesValue(vec![0xff, 0x00, b'a']))),
                    key_value("map_empty", map(vec![])),
                    key_value(
                        "map",
                        map(vec![
                            ("inner_empty", map(vec![])),
                            ("inner_value_empty", empty()),
                            ("inner_bytes", value(Value::BytesValue(b"hi".to_vec()))),
                        ]),
                    ),
                    key_value(
                        "array",
                        array(vec![
                            value(Value::IntValue(1)),
                            empty(),
                            value(Value::BytesValue(b"hi".to_vec())),
                            array(vec![value(Value::BoolValue(true))]),
                            map(vec![]),
                            map(vec![("", string("collapsed"))]),
                            map(vec![("a", map(vec![("b", value(Value::IntValue(2)))]))]),
                        ]),
                    ),
                    key_value("", map(vec![("spread", string("x"))])),
                ],
            )
        },
        Case {
            target: r#"{"attr_depth_4":[{"level1":[{"level2":[{"level3":[{"level4":"leaf"}]}]}]}],"attr_depth_5":[{"level1":[{"level2":[{"level3":[{"level4":["{\"level5\":\"leaf\"}"]}]}]}]}],"attr_depth_6":[{"level1":[{"level2":[{"level3":[{"level4":["{\"level5\":[{\"level6\":\"leaf\"}]}"]}]}]}]}],"message":"","status":""}"#,
            ..case(
                "a map that arrays nest at depth 10 becomes a string",
                (4..=6)
                    .map(|depth| {
                        key_value(
                            &format!("attr_depth_{}", depth),
                            nested_through_arrays(depth, string("leaf")),
                        )
                    })
                    .collect(),
            )
        },
        Case {
            target: r#"{"m.m1.m2.m3.m4.m5.m6.m7.m8.m9":"{\"m10\":1}","message":"","n.m1.m2.m3.m4.m5.m6.m7.m8.m9":1,"status":""}"#,
            ..case(
                "a map at depth 10 becomes a string",
                vec![
                    key_value("m", nested_maps(10, value(Value::IntValue(1)))),
                    key_value("n", nested_maps(9, value(Value::IntValue(1)))),
                ],
            )
        },
        Case {
            target: r#"{"message":"12","otel.scope.name":"1.5","status":"true"}"#,
            ..case(
                "reserved attributes become strings",
                vec![
                    key_value("message", value(Value::IntValue(12))),
                    key_value("level", value(Value::BoolValue(true))),
                    key_value("ddtags", value(Value::IntValue(5))),
                    key_value("otel.library.name", value(Value::DoubleValue(1.5))),
                ],
            )
        },
        Case {
            target: r#"{"message":"{\"k\":\"v\"}","status":""}"#,
            ..case(
                "a map message becomes JSON",
                vec![key_value("msg", map(vec![("k", string("v"))]))],
            )
        },
        Case {
            body: Some(value(Value::DoubleValue(1e21))),
            target: r#"{"message":"1e+21","status":""}"#,
            ..case(
                "an empty message falls back to the body",
                vec![key_value("msg", empty())],
            )
        },
        Case {
            target: r#"{"message":"-3","status":""}"#,
            ..body("an integer body", value(Value::IntValue(-3)))
        },
        Case {
            target: r#"{"message":"Ym9keQ==","status":""}"#,
            ..body("a bytes body", value(Value::BytesValue(b"body".to_vec())))
        },
        Case {
            target: r#"{"message":"NaN","status":""}"#,
            ..body("a NaN body", value(Value::DoubleValue(f64::NAN)))
        },
        Case {
            target: r#"{"message":"[\"<&>\"]","status":""}"#,
            ..body("an array body", array(vec![string("<&>")]))
        },
        Case {
            resource: vec![
                key_value("res_empty", empty()),
                key_value("res_inf", value(Value::DoubleValue(f64::NEG_INFINITY))),
                key_value("res_bytes", value(Value::BytesValue(b"hi".to_vec()))),
                key_value("service", value(Value::IntValue(3))),
            ],
            scope: vec![
                key_value("scope_bool", value(Value::BoolValue(false))),
                key_value("scope_empty", empty()),
                key_value("scope_array", array(vec![value(Value::IntValue(2))])),
            ],
            target: r#"{"message":"","otel.service":"3","res_bytes":"aGk=","res_empty":"","res_inf":"-Infinity","scope_array":"[2]","scope_bool":"false","scope_empty":"","status":""}"#,
            ..case("resource and scope attributes become strings", vec![])
        },
        Case {
            resource: vec![key_value("d", string("resource"))],
            target: r#"{"d":"resource","message":"","status":""}"#,
            ..case(
                "a NaN record attribute that a resource attribute overwrites",
                vec![key_value("d", value(Value::DoubleValue(f64::NAN)))],
            )
        },
    ];

    for case in cases {
        let lr = LogRecord {
            attributes: case.record,
            body: case.body,
            ..Default::default()
        };
        let resource = Resource {
            attributes: case.resource,
            ..Default::default()
        };
        let scope = otlp_common::InstrumentationScope {
            attributes: case.scope,
            ..Default::default()
        };
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default(), None)
            .unwrap_or_else(|e| panic!("{}: {}", case.name, e));

        let target: JsonValue = serde_json::from_str(case.target).expect("the target should be JSON");
        assert_eq!(item_json(&log), target, "{}", case.name);
    }
}

#[test]
fn non_finite_record_attributes_drop_the_log() {
    // The Agent fails to marshal each of these logs, and drops it.
    let cases = [
        ("a NaN attribute", value(Value::DoubleValue(f64::NAN))),
        (
            "an infinity in an array",
            array(vec![value(Value::DoubleValue(f64::INFINITY))]),
        ),
        ("a NaN in a map", map(vec![("d", value(Value::DoubleValue(f64::NAN)))])),
        (
            "a NaN leaf at depth 10",
            nested_maps(9, value(Value::DoubleValue(f64::NAN))),
        ),
        (
            "a negative infinity in a map in an array",
            array(vec![map(vec![("d", value(Value::DoubleValue(f64::NEG_INFINITY)))])]),
        ),
    ];

    for (name, attribute) in cases {
        let lr = LogRecord {
            attributes: vec![key_value("x", attribute)],
            ..Default::default()
        };
        let result = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default(), None);
        assert!(result.is_err(), "{}: {:?}", name, result.map(|log| item_json(&log)));
    }
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
        let log = transform_log_record(lr, &Resource::default(), None, None, None, tags, None)
            .expect("the record should transform");

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
        let log = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default(), None)
        .expect("the record should transform");
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
        let log = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default(), None)
            .expect("the record should transform");

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
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default(), None)
            .expect("the record should transform");

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
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default(), None)
            .expect("the record should transform");

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

/// A `datadog.log.source` case: the record, resource, and scope attributes, and the expected source and properties.
struct LogSourceCase {
    name: &'static str,
    record: Vec<KeyValue>,
    resource: Vec<KeyValue>,
    scope: Vec<KeyValue>,
    source: &'static str,
    properties: JsonValue,
}

fn log_source_case(name: &'static str) -> LogSourceCase {
    LogSourceCase {
        name,
        record: vec![],
        resource: vec![],
        scope: vec![],
        source: DEFAULT_SOURCE.as_ref(),
        properties: json!({}),
    }
}

/// Checks the source of each case, and that the properties holding a source stay in the log.
fn check_log_source_cases(cases: Vec<LogSourceCase>) {
    for case in cases {
        let lr = LogRecord {
            attributes: case.record,
            ..Default::default()
        };
        let resource = Resource {
            attributes: case.resource,
            ..Default::default()
        };
        let scope = otlp_common::InstrumentationScope {
            attributes: case.scope,
            ..Default::default()
        };
        let log = transform_log_record(lr, &resource, Some(&scope), None, None, TagSet::default(), None)
            .unwrap_or_else(|e| panic!("{}: {}", case.name, e));

        assert_eq!(log.source().as_deref(), Some(case.source), "{}", case.name);
        let properties: serde_json::Map<String, JsonValue> = log
            .additional_properties()
            .iter()
            .filter(|(key, _)| key.to_lowercase().contains("source"))
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        assert_eq!(JsonValue::Object(properties), case.properties, "{}", case.name);
    }
}

#[test]
fn log_source_follows_the_reference() {
    // The Agent's logs agent exporter uses the final `datadog.log.source` property as the source when it's a non-empty
    // string. Record attributes keep their types, and resource and scope attributes become strings.
    let attr = |value| vec![key_value(LOG_SOURCE_ATTR, value)];
    let cases = vec![
        log_source_case("no attribute keeps the default source"),
        LogSourceCase {
            record: attr(string("record-source")),
            source: "record-source",
            properties: json!({"datadog.log.source": "record-source"}),
            ..log_source_case("a record string sets the source")
        },
        LogSourceCase {
            record: attr(string("")),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty record string keeps the default source")
        },
        LogSourceCase {
            record: attr(empty()),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty record value keeps the default source")
        },
        LogSourceCase {
            record: attr(value(Value::BytesValue(vec![]))),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("empty record bytes keep the default source")
        },
        LogSourceCase {
            record: attr(value(Value::IntValue(1))),
            properties: json!({"datadog.log.source": 1}),
            ..log_source_case("a record int keeps the default source")
        },
        LogSourceCase {
            record: attr(value(Value::DoubleValue(1.5))),
            properties: json!({"datadog.log.source": 1.5}),
            ..log_source_case("a record double keeps the default source")
        },
        LogSourceCase {
            record: attr(value(Value::BoolValue(true))),
            properties: json!({"datadog.log.source": true}),
            ..log_source_case("a record bool keeps the default source")
        },
        LogSourceCase {
            record: attr(array(vec![string("x")])),
            properties: json!({"datadog.log.source": ["x"]}),
            ..log_source_case("a record array keeps the default source")
        },
        LogSourceCase {
            record: attr(value(Value::BytesValue(vec![0xff, 0x00, b'a']))),
            source: "/wBh",
            properties: json!({"datadog.log.source": "/wBh"}),
            ..log_source_case("record bytes set the source to their base64")
        },
        LogSourceCase {
            record: vec![key_value("datadog.log", map(vec![("source", string("nested-source"))]))],
            source: "nested-source",
            properties: json!({"datadog.log.source": "nested-source"}),
            ..log_source_case("a flattened record map sets the source")
        },
        LogSourceCase {
            record: vec![key_value("Datadog.Log.Source", string("mixed-case"))],
            properties: json!({"Datadog.Log.Source": "mixed-case"}),
            ..log_source_case("a record key matches only in lowercase")
        },
        LogSourceCase {
            resource: attr(string("resource-source")),
            source: "resource-source",
            properties: json!({"datadog.log.source": "resource-source"}),
            ..log_source_case("a resource string sets the source")
        },
        LogSourceCase {
            resource: attr(value(Value::IntValue(1))),
            source: "1",
            properties: json!({"datadog.log.source": "1"}),
            ..log_source_case("a resource int becomes a string source")
        },
        LogSourceCase {
            resource: attr(value(Value::DoubleValue(f64::NAN))),
            source: "NaN",
            properties: json!({"datadog.log.source": "NaN"}),
            ..log_source_case("a resource NaN becomes a string source")
        },
        LogSourceCase {
            resource: attr(empty()),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty resource value keeps the default source")
        },
        LogSourceCase {
            resource: attr(value(Value::BytesValue(vec![]))),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("empty resource bytes keep the default source")
        },
        LogSourceCase {
            scope: attr(value(Value::BoolValue(false))),
            source: "false",
            properties: json!({"datadog.log.source": "false"}),
            ..log_source_case("a scope bool becomes a string source")
        },
        LogSourceCase {
            scope: attr(value(Value::DoubleValue(f64::INFINITY))),
            source: "Infinity",
            properties: json!({"datadog.log.source": "Infinity"}),
            ..log_source_case("a scope infinity becomes a string source")
        },
        LogSourceCase {
            scope: attr(string("")),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty scope string keeps the default source")
        },
        LogSourceCase {
            record: attr(string("record-source")),
            resource: attr(string("resource-source")),
            source: "resource-source",
            properties: json!({"datadog.log.source": "resource-source"}),
            ..log_source_case("a resource attribute overwrites a record attribute")
        },
        LogSourceCase {
            record: attr(string("record-source")),
            resource: attr(string("")),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty resource string overwrites a record source")
        },
        LogSourceCase {
            record: attr(value(Value::IntValue(1))),
            resource: attr(value(Value::IntValue(2))),
            source: "2",
            properties: json!({"datadog.log.source": "2"}),
            ..log_source_case("a resource int overwrites a record int")
        },
        LogSourceCase {
            resource: attr(string("resource-source")),
            scope: attr(string("scope-source")),
            source: "scope-source",
            properties: json!({"datadog.log.source": "scope-source"}),
            ..log_source_case("a scope attribute overwrites a resource attribute")
        },
        LogSourceCase {
            record: attr(string("record-source")),
            scope: attr(string("")),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("an empty scope string overwrites a record source")
        },
        LogSourceCase {
            record: attr(value(Value::DoubleValue(f64::NAN))),
            resource: attr(string("resource-source")),
            source: "resource-source",
            properties: json!({"datadog.log.source": "resource-source"}),
            ..log_source_case("a resource attribute overwrites a record NaN")
        },
    ];
    check_log_source_cases(cases);
}

#[test]
fn a_non_finite_record_log_source_drops_the_log() {
    let lr = LogRecord {
        attributes: vec![key_value(LOG_SOURCE_ATTR, value(Value::DoubleValue(f64::NAN)))],
        ..Default::default()
    };
    let result = transform_log_record(lr, &Resource::default(), None, None, None, TagSet::default(), None);
    assert!(result.is_err(), "{:?}", result.map(|log| log.source().clone()));
}

#[test]
#[ignore = "target for `gap/value-formatting`"]
fn target_log_source_value_formatting() {
    // The Agent's source is `AsString` of a resource or scope array or map, so it follows pdata's JSON.
    let attr = |value| vec![key_value(LOG_SOURCE_ATTR, value)];
    let cases = vec![
        LogSourceCase {
            resource: attr(array(vec![value(Value::DoubleValue(1.0))])),
            source: "[1]",
            properties: json!({"datadog.log.source": "[1]"}),
            ..log_source_case("a resource array of a whole double")
        },
        LogSourceCase {
            // A non-finite element makes `AsString` fail, so it returns an empty string.
            resource: attr(array(vec![value(Value::DoubleValue(f64::NAN))])),
            properties: json!({"datadog.log.source": ""}),
            ..log_source_case("a resource array of a NaN keeps the default source")
        },
        LogSourceCase {
            scope: attr(map(vec![("b", value(Value::DoubleValue(1.0)))])),
            source: r#"{"b":1}"#,
            properties: json!({"datadog.log.source": r#"{"b":1}"#}),
            ..log_source_case("a scope map of a whole double")
        },
    ];
    check_log_source_cases(cases);
}

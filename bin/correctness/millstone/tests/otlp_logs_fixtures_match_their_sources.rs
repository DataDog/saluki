//! Builds the `static_base64` OTLP logs corpora used by the `otlp-logs-*` correctness cases.
//!
//! Each corpus is built here from readable Rust, and the test checks that the encoded request matches the
//! `static_base64` payload in the case's `millstone.yaml`. To regenerate the payloads after changing a fixture, run:
//!
//! ```text
//! UPDATE_OTLP_FIXTURES=1 cargo nextest run -p millstone otlp_logs_fixtures_match_their_sources
//! ```
//!
//! Every log record carries a `fixture_id` attribute, which the cases use as their identity field, so that records
//! still pair when a gap changes their message or timestamp. Comments name the gaps from the OTLP logs parity plan
//! (DataDog/saluki#2966) that each record exercises.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use otlp_protos::opentelemetry::proto::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::{any_value::Value, AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message as _;

/// 2026-10-29T10:00:00Z, in nanoseconds.
const BASE_TIME_NANOS: u64 = 1_793_268_000_000_000_000;

fn string(value: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(value.to_string())),
    }
}

fn int(value: i64) -> AnyValue {
    AnyValue {
        value: Some(Value::IntValue(value)),
    }
}

fn double(value: f64) -> AnyValue {
    AnyValue {
        value: Some(Value::DoubleValue(value)),
    }
}

fn boolean(value: bool) -> AnyValue {
    AnyValue {
        value: Some(Value::BoolValue(value)),
    }
}

fn bytes(value: &[u8]) -> AnyValue {
    AnyValue {
        value: Some(Value::BytesValue(value.to_vec())),
    }
}

fn array(values: Vec<AnyValue>) -> AnyValue {
    AnyValue {
        value: Some(Value::ArrayValue(ArrayValue { values })),
    }
}

fn map(entries: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(Value::KvlistValue(KeyValueList { values: entries })),
    }
}

fn empty() -> AnyValue {
    AnyValue { value: None }
}

fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(value),
    }
}

/// Builds a log record with a unique `fixture_id`, a string body, and a timestamp derived from `index`.
fn record(index: u64, id: &str, mut attributes: Vec<KeyValue>) -> LogRecord {
    attributes.insert(0, kv("fixture_id", string(id)));
    LogRecord {
        time_unix_nano: BASE_TIME_NANOS + index * 1_000_000,
        severity_number: 9,
        severity_text: "INFO".to_string(),
        body: Some(string(&format!("{} body", id))),
        attributes,
        ..Default::default()
    }
}

fn request(resource_attributes: Vec<KeyValue>, scope: InstrumentationScope, records: Vec<LogRecord>) -> Vec<u8> {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: resource_attributes,
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(scope),
                log_records: records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

/// Nests `leaf` under `depth` levels of single-key maps, alternating with single-element arrays.
fn nested_through_arrays(depth: usize, leaf: AnyValue) -> AnyValue {
    (0..depth).fold(leaf, |inner, level| {
        if level % 2 == 0 {
            map(vec![kv(&format!("level{}", depth - level), inner)])
        } else {
            array(vec![inner])
        }
    })
}

/// Record attributes: value types, key spelling, reserved keys, and trace context.
fn attributes_fixture() -> Vec<u8> {
    let records = vec![
        // Native scalar types stay native in record attributes.
        record(
            0,
            "scalar-types",
            vec![
                kv("attr_string", string("value")),
                kv("attr_int", int(-42)),
                kv("attr_double", double(1.5)),
                kv("attr_bool", boolean(true)),
            ],
        ),
        // gap/array-attributes: arrays stay JSON arrays, with maps inside them flattened.
        record(
            1,
            "arrays",
            vec![
                kv("attr_array", array(vec![string("a"), int(1), boolean(false)])),
                kv("attr_array_of_maps", array(vec![map(vec![kv("inner", string("v"))])])),
                kv("attr_empty_array", array(vec![])),
            ],
        ),
        // Maps are flattened to dotted keys.
        record(
            2,
            "nested-maps",
            vec![kv(
                "attr_map",
                map(vec![
                    kv("leaf", string("v")),
                    kv("child", map(vec![kv("grandchild", int(7))])),
                ]),
            )],
        ),
        // gap/array-attributes: arrays count toward the flattening depth, so maps nested under arrays pass depth 10.
        record(
            3,
            "deep-nesting",
            vec![
                kv("attr_depth_9", nested_through_arrays(9, string("leaf"))),
                kv("attr_depth_11", nested_through_arrays(11, string("leaf"))),
            ],
        ),
        // gap/bytes-encoding: bytes are base64 in the Agent.
        record(
            4,
            "bytes",
            vec![
                kv("attr_bytes_utf8", bytes(b"hello")),
                kv("attr_bytes_binary", bytes(&[0, 159, 146, 150])),
            ],
        ),
        // gap/attribute-key-case: only reserved keys are matched case-insensitively; others keep their spelling.
        record(
            5,
            "key-spelling",
            vec![
                kv("RequestID", string("r-1")),
                kv("nested_Map", map(vec![kv("InnerKey", int(1))])),
            ],
        ),
        // gap/as-string-coercion: empty values become "" in the Agent.
        record(6, "empty-value", vec![kv("attr_empty", empty())]),
        // Message attributes override the body.
        record(7, "message-string", vec![kv("msg", string("from msg"))]),
        // gap/non-string-message: non-string message attributes still override the body in the Agent.
        record(8, "message-int", vec![kv("message", int(42))]),
        record(9, "message-map", vec![kv("log", map(vec![kv("k", string("v"))]))]),
        // Status from attributes, and an upper-case reserved key.
        record(10, "status-attribute", vec![kv("LEVEL", string("warn"))]),
        // ddtags from attributes are appended to the resource tags; an empty one adds nothing after intake parsing
        // (gap/ddtags-formatting is an intentional formatting difference).
        record(11, "ddtags", vec![kv("ddtags", string("team:a, env:b"))]),
        record(12, "ddtags-empty", vec![kv("ddtags", string(""))]),
        // Trace context from attributes.
        record(
            13,
            "trace-attributes",
            vec![
                kv("trace_id", string("0102030405060708090a0b0c0d0e0f10")),
                kv("span_id", string("0102030405060708")),
            ],
        ),
        // gap/scope-metadata: library aliases map to scope keys, unless the canonical key is present.
        record(
            14,
            "library-aliases",
            vec![
                kv("otel.library.name", string("alias-name")),
                kv("otel.library.version", string("alias-version")),
            ],
        ),
        record(
            15,
            "library-alias-and-canonical",
            vec![
                kv("otel.library.name", string("alias-name")),
                kv("otel.scope.name", string("canonical-name")),
            ],
        ),
    ];

    let mut records = records;
    // Trace context from the record fields overwrites the attribute-derived IDs.
    let mut trace_fields = record(
        16,
        "trace-fields",
        vec![kv("trace_id", string("ffffffffffffffffffffffffffffffff"))],
    );
    trace_fields.trace_id = (1..=16).collect();
    trace_fields.span_id = (1..=8).collect();
    records.push(trace_fields);
    // gap/event-name: the event name is stored as `otel.event_name`.
    let mut event = record(17, "event-name", vec![]);
    event.event_name = "fixture.event".to_string();
    records.push(event);

    request(
        vec![
            kv("service.name", string("fixture-attributes")),
            kv("host.name", string("fixture-host")),
        ],
        InstrumentationScope {
            name: "fixture-scope".to_string(),
            version: "1.0.0".to_string(),
            ..Default::default()
        },
        records,
    )
}

fn case_millstone_config(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test/correctness/cases")
        .join(case)
        .join("millstone.yaml")
}

/// Checks that the case's `static_base64` payload matches `payload`, or rewrites it when `UPDATE_OTLP_FIXTURES` is set.
fn check_fixture(case: &str, payload: &[u8]) -> Result<(), String> {
    let path = case_millstone_config(case);
    let config = std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let expected = base64::engine::general_purpose::STANDARD.encode(payload);

    let prefix = "static_base64: \"";
    let start = config
        .find(prefix)
        .map(|idx| idx + prefix.len())
        .ok_or_else(|| format!("{}: no `static_base64` payload", path.display()))?;
    let end = start
        + config[start..]
            .find('"')
            .ok_or_else(|| format!("{}: unterminated `static_base64` payload", path.display()))?;

    if config[start..end] == expected {
        return Ok(());
    }
    if std::env::var_os("UPDATE_OTLP_FIXTURES").is_some() {
        let updated = format!("{}{}{}", &config[..start], expected, &config[end..]);
        std::fs::write(&path, updated).map_err(|e| format!("{}: {}", path.display(), e))?;
        return Ok(());
    }
    Err(format!(
        "{}: `static_base64` doesn't match the fixture; rerun with UPDATE_OTLP_FIXTURES=1 to regenerate it",
        path.display()
    ))
}

#[test]
fn otlp_logs_fixtures_match_their_sources() {
    let fixtures: &[(&str, fn() -> Vec<u8>)] = &[("otlp-logs-attributes", attributes_fixture)];

    let errors = fixtures
        .iter()
        .filter_map(|(case, build)| check_fixture(case, &build()).err())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

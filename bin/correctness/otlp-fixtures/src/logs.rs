//! OTLP logs fixtures, for the `otlp-logs-*` cases.
//!
//! Every log record carries a `fixture_id` attribute, which the cases use as their identity field, so that records
//! still pair when a gap changes their message or timestamp. Comments name the gaps from the OTLP logs parity plan
//! (DataDog/saluki#2966) that each record exercises.

use otlp_protos::opentelemetry::proto::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::{AnyValue, InstrumentationScope, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message as _;

use crate::common::{array, boolean, bytes, double, empty, int, kv, map, string, LAMBDA_ARN};

/// 2026-10-29T10:00:00Z, in nanoseconds.
const BASE_TIME_NANOS: u64 = 1_793_268_000_000_000_000;

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
pub fn attributes_fixture() -> Vec<u8> {
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
        // gap/attribute-key-case: only reserved keys are matched case-insensitively; others keep their spelling, so
        // `Hostname` and `Service` stay in the content next to `hostname` and `service`.
        record(
            5,
            "key-spelling",
            vec![
                kv("RequestID", string("r-1")),
                kv("nested_Map", map(vec![kv("InnerKey", int(1))])),
                kv("Hostname", string("record-hostname")),
                kv("Service", string("record-service")),
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
    // gap/event-name: the event name is stored as `otel.event_name`, overwriting an attribute with the same key.
    let mut event = record(17, "event-name", vec![kv("otel.event_name", string("from-attribute"))]);
    event.event_name = "fixture.event".to_string();
    records.push(event);
    // gap/reserved-key-duplicates: when a reserved key repeats, in any case, the last message, status, and `ddtags`
    // win, even when the last one is empty or has no value, and the first trace and span IDs that decode win.
    records.extend([
        record(
            18,
            "duplicate-message",
            vec![kv("message", string("first")), kv("MESSAGE", string("second"))],
        ),
        record(
            19,
            "duplicate-message-empty-last",
            vec![kv("msg", string("first")), kv("Message", string(""))],
        ),
        record(
            20,
            "duplicate-message-missing-last",
            vec![kv("message", string("first")), kv("MESSAGE", empty())],
        ),
        record(
            21,
            "duplicate-status",
            vec![kv("status", string("info")), kv("Level", string("error"))],
        ),
        record(
            22,
            "duplicate-status-missing-last",
            vec![kv("severity", string("error")), kv("LEVEL", empty())],
        ),
        record(
            23,
            "duplicate-ddtags",
            vec![kv("ddtags", string("team:a")), kv("DDTags", string("team:b"))],
        ),
        record(
            24,
            "duplicate-ddtags-missing-last",
            vec![kv("ddtags", string("team:a")), kv("DDTAGS", empty())],
        ),
        record(
            25,
            "duplicate-trace-ids",
            vec![
                kv("trace_id", string("not-hex")),
                kv("TraceID", string("0102030405060708090a0b0c0d0e0f10")),
                kv("Trace_Id", string("1112131415161718191a1b1c1d1e1f20")),
                kv("SpanID", string("0102030405060708")),
                kv("span_id", string("1112131415161718")),
            ],
        ),
    ]);
    // gap/event-name: without an event name, an `otel.event_name` attribute stays.
    records.push(record(
        26,
        "event-name-unset",
        vec![kv("otel.event_name", string("from-attribute"))],
    ));

    // gap/scope-metadata: a record alias matches in any case, and defers only to a canonical key spelled exactly.
    records.extend([
        record(
            27,
            "record-alias-any-case",
            vec![kv("OTEL.Library.Name", string("upper-alias"))],
        ),
        record(
            28,
            "record-alias-exact-canonical",
            vec![
                kv("otel.library.name", string("alias-name")),
                kv("OTEL.SCOPE.NAME", string("upper-canonical")),
            ],
        ),
    ]);

    let resource_attributes = |extra: Vec<KeyValue>| {
        let mut attributes = vec![
            kv("service.name", string("fixture-attributes")),
            kv("host.name", string("fixture-host")),
        ];
        attributes.extend(extra);
        attributes
    };
    let no_scope = InstrumentationScope::default;
    let resources = vec![
        resource_logs(
            resource_attributes(vec![]),
            vec![scope_logs(
                InstrumentationScope {
                    name: "fixture-scope".to_string(),
                    version: "1.0.0".to_string(),
                    ..Default::default()
                },
                records,
            )],
        ),
        // gap/scope-metadata: a resource alias maps to its scope key, and overwrites a record's canonical key.
        resource_logs(
            resource_attributes(vec![
                kv("otel.library.name", string("resource-alias")),
                kv("otel.library.version", string("resource-alias-version")),
            ]),
            vec![scope_logs(
                no_scope(),
                vec![
                    record(29, "resource-alias", vec![]),
                    record(
                        30,
                        "resource-alias-over-record",
                        vec![kv("otel.scope.name", string("record-canonical"))],
                    ),
                ],
            )],
        ),
        // gap/scope-metadata: a resource alias defers to the resource's canonical key.
        resource_logs(
            resource_attributes(vec![
                kv("otel.library.name", string("resource-alias")),
                kv("otel.scope.name", string("resource-canonical")),
            ]),
            vec![scope_logs(
                no_scope(),
                vec![record(31, "resource-alias-and-canonical", vec![])],
            )],
        ),
        // gap/scope-metadata: a resource alias matches only in lowercase.
        resource_logs(
            resource_attributes(vec![kv("OTEL.LIBRARY.NAME", string("resource-upper-alias"))]),
            vec![scope_logs(
                no_scope(),
                vec![record(32, "resource-alias-lowercase-only", vec![])],
            )],
        ),
        // gap/scope-metadata: a resource canonical key overwrites a record alias and a record canonical key.
        resource_logs(
            resource_attributes(vec![kv("otel.scope.name", string("resource-canonical"))]),
            vec![scope_logs(
                no_scope(),
                vec![
                    record(
                        33,
                        "resource-over-record-alias",
                        vec![kv("otel.library.name", string("record-alias"))],
                    ),
                    record(
                        34,
                        "resource-over-record-canonical",
                        vec![kv("otel.scope.name", string("record-canonical"))],
                    ),
                ],
            )],
        ),
        // gap/scope-metadata: scope attributes overwrite resource and record values under their own keys, so an alias
        // isn't mapped and `hostname` isn't prefixed, and the scope's name only fills in a missing key.
        resource_logs(
            resource_attributes(vec![kv("otel.scope.name", string("resource-canonical"))]),
            vec![scope_logs(
                InstrumentationScope {
                    name: "scope-metadata-name".to_string(),
                    attributes: vec![
                        kv("otel.scope.name", string("scope-canonical")),
                        kv("otel.library.version", string("scope-alias-version")),
                        kv("hostname", string("scope-host")),
                    ],
                    ..Default::default()
                },
                vec![record(
                    35,
                    "scope-attributes-over-all",
                    vec![kv("otel.scope.name", string("record-canonical"))],
                )],
            )],
        ),
    ];

    ExportLogsServiceRequest {
        resource_logs: resources,
    }
    .encode_to_vec()
}

/// Severity, timestamps, bodies, and processing rules.
pub fn severity_body_fixture() -> Vec<u8> {
    let mut records = Vec::new();
    let mut index = 0;
    let mut next = |id: String, attributes: Vec<KeyValue>| {
        let built = record(index, &id, attributes);
        index += 1;
        built
    };

    // gap/zero-severity-status: severity number 0 with no text gives no status, which becomes `info` on the envelope.
    // gap/raw-status-strings and the number mapping: each range boundary, and a number past the last range.
    for number in [0, 1, 4, 5, 8, 9, 12, 13, 16, 17, 20, 21, 24, 25] {
        let mut severity = next(format!("severity-number-{}", number), vec![]);
        severity.severity_number = number;
        severity.severity_text = String::new();
        records.push(severity);
    }

    // Severity text takes precedence over the number, and unknown text passes through raw (gap/raw-status-strings).
    for (id, text, number) in [
        ("severity-text-over-number", "WARN", 17),
        ("severity-text-unknown", "notice-ish", 9),
        ("severity-text-only", "Error", 0),
    ] {
        let mut severity = next(id.to_string(), vec![]);
        severity.severity_number = number;
        severity.severity_text = text.to_string();
        records.push(severity);
    }

    // Status attributes take precedence over severity, and unknown values pass through raw (gap/raw-status-strings).
    records.push(next(
        "status-attribute-unknown".to_string(),
        vec![kv("status", string("custom"))],
    ));
    records.push(next(
        "severity-attribute".to_string(),
        vec![kv("severity", string("debug"))],
    ));

    // gap/observed-timestamp: the observed timestamp is the fallback when the timestamp is unset.
    let mut observed = next("timestamp-observed-only".to_string(), vec![]);
    observed.observed_time_unix_nano = observed.time_unix_nano;
    observed.time_unix_nano = 0;
    records.push(observed);
    let mut both = next("timestamp-and-observed".to_string(), vec![]);
    both.observed_time_unix_nano = both.time_unix_nano + 5_000_000;
    records.push(both);
    let mut neither = next("timestamp-neither".to_string(), vec![]);
    neither.time_unix_nano = 0;
    records.push(neither);
    // Nanoseconds are read as signed, so a timestamp past `i64::MAX` wraps to before 1970, from either field.
    let mut wrapped = next("timestamp-wraps".to_string(), vec![]);
    wrapped.time_unix_nano = 1 << 63;
    records.push(wrapped);
    let mut wrapped_observed = next("timestamp-observed-wraps".to_string(), vec![]);
    wrapped_observed.time_unix_nano = 0;
    wrapped_observed.observed_time_unix_nano = u64::MAX;
    records.push(wrapped_observed);

    // gap/value-formatting and gap/bytes-encoding: bodies other than strings use `AsString` in the Agent.
    for (id, body) in [
        ("body-int", int(-7)),
        ("body-double", double(0.25)),
        ("body-large-double", double(1e21)),
        ("body-bool", boolean(true)),
        ("body-bytes", bytes(b"body bytes")),
        ("body-map", map(vec![kv("b", int(2)), kv("a", string("<&>"))])),
        (
            "body-array",
            array(vec![int(1), string("two"), map(vec![kv("k", boolean(false))])]),
        ),
        ("body-empty", empty()),
    ] {
        let mut with_body = next(id.to_string(), vec![]);
        with_body.body = Some(body);
        records.push(with_body);
    }
    let mut no_body = next("body-missing".to_string(), vec![]);
    no_body.body = None;
    records.push(no_body);

    // Keep all severity, timestamp, and body cases through the include rule. The additional records exercise both
    // filtering and masking without adding another correctness case and its container startup cost.
    for record in &mut records {
        record.attributes.push(kv("marker", string("keep")));
    }
    records.extend(processing_rule_records(index));

    request(
        vec![
            kv("service.name", string("fixture-severity-body")),
            kv("host.name", string("fixture-host")),
        ],
        InstrumentationScope::default(),
        records,
    )
}

fn resource_logs(resource_attributes: Vec<KeyValue>, scope_logs: Vec<ScopeLogs>) -> ResourceLogs {
    ResourceLogs {
        resource: Some(Resource {
            attributes: resource_attributes,
            ..Default::default()
        }),
        scope_logs,
        ..Default::default()
    }
}

fn scope_logs(scope: InstrumentationScope, records: Vec<LogRecord>) -> ScopeLogs {
    ScopeLogs {
        scope: Some(scope),
        log_records: records,
        ..Default::default()
    }
}

/// Resource and scope attributes, and host and service resolution.
pub fn resource_host_fixture() -> Vec<u8> {
    let no_scope = InstrumentationScope::default;
    let mut resources = vec![
        // gap/as-string-coercion: resource and scope attributes use `AsString`, and `hostname`/`service` are renamed.
        resource_logs(
            vec![
                kv("service.name", string("fixture-resource")),
                kv("host.name", string("resource-host")),
                kv("res_int", int(3)),
                kv("res_double", double(2.5)),
                kv("res_bool", boolean(false)),
                kv("res_map", map(vec![kv("k", string("v"))])),
                kv("res_array", array(vec![int(1), int(2)])),
                kv("res_empty", empty()),
                kv("hostname", string("renamed-host")),
                kv("service", string("renamed-service")),
            ],
            vec![scope_logs(
                InstrumentationScope {
                    name: "resource-scope".to_string(),
                    version: "2.0.0".to_string(),
                    attributes: vec![kv("scope_string", string("s")), kv("scope_int", int(5))],
                    ..Default::default()
                },
                vec![record(0, "resource-types", vec![])],
            )],
        ),
        // gap/as-string-coercion: a non-string `service.name` uses `AsString`.
        resource_logs(
            vec![kv("service.name", int(12)), kv("host.name", string("resource-host"))],
            vec![scope_logs(no_scope(), vec![record(1, "service-name-int", vec![])])],
        ),
        // gap/default-hostname: with no host at all, the Agent's hostname fills the envelope.
        resource_logs(
            vec![kv("service.name", string("fixture-no-host"))],
            vec![scope_logs(no_scope(), vec![record(2, "no-host", vec![])])],
        ),
        // gap/log-source-attribute: `datadog.log.source` sets the source when it's a non-empty string, from record or resource attributes.
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
            ],
            vec![scope_logs(
                no_scope(),
                vec![
                    record(
                        6,
                        "log-source-record",
                        vec![kv("datadog.log.source", string("record-source"))],
                    ),
                    record(7, "log-source-empty", vec![kv("datadog.log.source", string(""))]),
                    record(8, "log-source-int", vec![kv("datadog.log.source", int(1))]),
                ],
            )],
        ),
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
                kv("datadog.log.source", string("resource-source")),
            ],
            vec![scope_logs(no_scope(), vec![record(9, "log-source-resource", vec![])])],
        ),
        // gap/log-source-attribute: resource and scope attributes become strings, so any non-empty value sets the source.
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
                kv("datadog.log.source", int(1)),
            ],
            vec![scope_logs(
                no_scope(),
                vec![record(65, "log-source-resource-int", vec![])],
            )],
        ),
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
            ],
            vec![
                scope_logs(
                    InstrumentationScope {
                        attributes: vec![kv("datadog.log.source", string("scope-source"))],
                        ..Default::default()
                    },
                    vec![record(66, "log-source-scope", vec![])],
                ),
                // Record bytes become base64, and a record map flattens into the `datadog.log.source` key.
                scope_logs(
                    no_scope(),
                    vec![
                        record(
                            69,
                            "log-source-record-bytes",
                            vec![kv("datadog.log.source", bytes(&[0xff, 0x00, b'a']))],
                        ),
                        record(
                            70,
                            "log-source-record-nested",
                            vec![kv("datadog.log", map(vec![kv("source", string("nested-source"))]))],
                        ),
                    ],
                ),
            ],
        ),
        // The scope attribute overwrites the resource attribute.
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
                kv("datadog.log.source", string("resource-source")),
            ],
            vec![scope_logs(
                InstrumentationScope {
                    attributes: vec![kv("datadog.log.source", string("scope-source"))],
                    ..Default::default()
                },
                vec![record(67, "log-source-scope-over-resource", vec![])],
            )],
        ),
        // An empty resource attribute overwrites the record attribute, so the default source applies.
        resource_logs(
            vec![
                kv("service.name", string("fixture-log-source")),
                kv("host.name", string("resource-host")),
                kv("datadog.log.source", string("")),
            ],
            vec![scope_logs(
                no_scope(),
                vec![record(
                    68,
                    "log-source-resource-empty-over-record",
                    vec![kv("datadog.log.source", string("record-source"))],
                )],
            )],
        ),
        // gap/host-resolution: the record-level host and service fallbacks carry over to later records of the same resource,
        // including records in a later `ScopeLogs`.
        resource_logs(
            vec![],
            vec![
                scope_logs(
                    no_scope(),
                    vec![
                        record(
                            3,
                            "carry-over-first",
                            vec![
                                kv("host.name", string("record-host")),
                                kv("service.name", string("record-service")),
                            ],
                        ),
                        record(4, "carry-over-second", vec![]),
                    ],
                ),
                scope_logs(no_scope(), vec![record(5, "carry-over-next-scope", vec![])]),
            ],
        ),
        // The record-level host fallback resolves the record's source like a resource's: `datadog.host.name` wins and
        // sticks.
        resource_logs(
            vec![],
            vec![scope_logs(
                no_scope(),
                vec![
                    record(
                        60,
                        "carry-over-datadog-host",
                        vec![
                            kv("datadog.host.name", string("record-datadog-host")),
                            kv("host.name", string("record-host")),
                        ],
                    ),
                    record(61, "carry-over-datadog-host-second", vec![]),
                ],
            )],
        ),
        // A serverless record has no host, so the host of a later record sticks.
        resource_logs(
            vec![],
            vec![scope_logs(
                no_scope(),
                vec![
                    record(
                        62,
                        "carry-over-serverless",
                        vec![
                            kv("cloud.platform", string("azure.functions")),
                            kv("service.name", string("record-function")),
                            kv("cloud.account.id", string("sub-1")),
                            kv("azure.resource_group.name", string("rg-1")),
                            kv("host.name", string("serverless-host")),
                        ],
                    ),
                    record(
                        63,
                        "carry-over-after-serverless",
                        vec![kv("host.name", string("later-host"))],
                    ),
                    record(64, "carry-over-after-serverless-second", vec![]),
                ],
            )],
        ),
    ];

    // gap/host-resolution: resource host precedence.
    let host_cases: Vec<(&str, Vec<KeyValue>)> = vec![
        (
            "host-datadog-over-host-name",
            vec![
                kv("datadog.host.name", string("datadog-host")),
                kv("host.name", string("plain-host")),
            ],
        ),
        (
            "host-k8s-node-and-cluster",
            vec![
                kv("k8s.node.name", string("node-1")),
                kv("k8s.cluster.name", string("cluster-1")),
            ],
        ),
        (
            "host-aws-host-id",
            vec![
                kv("cloud.provider", string("aws")),
                kv("host.id", string("i-0123456789")),
                kv("host.name", string("aws-host-name")),
            ],
        ),
        ("host-id-only", vec![kv("host.id", string("generic-host-id"))]),
        ("host-localhost", vec![kv("host.name", string("localhost"))]),
    ];
    for (index, (id, mut attributes)) in host_cases.into_iter().enumerate() {
        attributes.push(kv("service.name", string("fixture-host-precedence")));
        resources.push(resource_logs(
            attributes,
            vec![scope_logs(no_scope(), vec![record(10 + index as u64, id, vec![])])],
        ));
    }

    // The derived status replaces resource and scope attributes named `status`.
    resources.push(resource_logs(
        vec![
            kv("service.name", string("fixture-status-attribute")),
            kv("host.name", string("resource-host")),
            kv("status", string("resource-status")),
        ],
        vec![scope_logs(
            no_scope(),
            vec![record(15, "status-resource-attribute", vec![])],
        )],
    ));
    resources.push(resource_logs(
        vec![
            kv("service.name", string("fixture-status-attribute")),
            kv("host.name", string("resource-host")),
        ],
        vec![scope_logs(
            InstrumentationScope {
                attributes: vec![kv("status", string("scope-status"))],
                ..Default::default()
            },
            vec![record(16, "status-scope-attribute", vec![])],
        )],
    ));

    // Resource attributes that become tags: `service.namespace`, and `container.image.tags` values and precedence.
    let tag_cases: Vec<(&str, Vec<KeyValue>)> = vec![
        ("tags-service-namespace", vec![kv("service.namespace", string("shop"))]),
        (
            "tags-service-namespace-empty",
            vec![kv("service.namespace", string(""))],
        ),
        (
            "tags-image-tags-array",
            vec![kv("container.image.tags", array(vec![string("v1"), string("latest")]))],
        ),
        ("tags-image-tags-string", vec![kv("container.image.tags", string("v1"))]),
        (
            "tags-image-tags-empty-array",
            vec![kv("container.image.tags", array(vec![]))],
        ),
        (
            "tags-image-tags-empty-first",
            vec![kv("container.image.tags", array(vec![string(""), string("latest")]))],
        ),
        (
            "tags-image-tags-int-first",
            vec![kv("container.image.tags", array(vec![int(1)]))],
        ),
        (
            "tags-image-tag-then-tags",
            vec![
                kv("container.image.tag", string("old")),
                kv("container.image.tags", array(vec![string("new")])),
            ],
        ),
        (
            "tags-image-tags-then-tag",
            vec![
                kv("container.image.tags", array(vec![string("new")])),
                kv("container.image.tag", string("old")),
            ],
        ),
        (
            "tags-duplicate-container-id",
            vec![
                kv("container.id", string("first-container")),
                kv("container.id", string("second-container")),
            ],
        ),
        (
            "tags-duplicate-custom",
            vec![
                kv("datadog.container.tag.team", string("first-team")),
                kv("datadog.container.tag.team", string("second-team")),
            ],
        ),
        (
            "tags-custom-before-semconv",
            vec![
                kv("datadog.container.tag.image_tag", string("custom")),
                kv("container.image.tags", array(vec![string("v1")])),
            ],
        ),
    ];
    for (index, (id, mut attributes)) in tag_cases.into_iter().enumerate() {
        attributes.push(kv("service.name", string("fixture-tags")));
        attributes.push(kv("host.name", string("tags-host")));
        resources.push(resource_logs(
            attributes,
            vec![scope_logs(no_scope(), vec![record(20 + index as u64, id, vec![])])],
        ));
    }

    // Serverless sources take precedence over the resource's `host.name`, and non-string literal `host` values.
    let azure = |platform: &str| {
        vec![
            kv("cloud.platform", string(platform)),
            kv("cloud.account.id", string("sub-1")),
            kv("azure.resource_group.name", string("rg-1")),
        ]
    };
    let gcp = |platform: &str| {
        vec![
            kv("cloud.platform", string(platform)),
            kv("cloud.account.id", string("project-1")),
            kv("cloud.region", string("us-central1")),
            kv("faas.name", string("svc-1")),
            kv("faas.instance", string("instance-1")),
            kv("faas.version", string("rev-1")),
        ]
    };
    let without = |mut attributes: Vec<KeyValue>, key: &str| {
        attributes.retain(|attribute| attribute.key != key);
        attributes
    };
    let with = |mut attributes: Vec<KeyValue>, attribute: KeyValue| {
        attributes.push(attribute);
        attributes
    };
    let source_cases: Vec<(&str, Vec<KeyValue>)> = vec![
        ("source-azure-functions", azure("azure.functions")),
        (
            "source-azure-functions-incomplete",
            without(azure("azure.functions"), "azure.resource_group.name"),
        ),
        ("source-azure-app-service", azure("azure.app_service")),
        (
            "source-azure-app-service-incomplete",
            without(azure("azure.app_service"), "cloud.account.id"),
        ),
        (
            "source-azure-container-apps",
            with(
                azure("azure.container_apps"),
                kv("azure.container_app.instance.id", string("replica-1")),
            ),
        ),
        ("source-azure-container-apps-no-replica", azure("azure.container_apps")),
        ("source-gcp-cloud-run", gcp("gcp_cloud_run")),
        ("source-gcp-cloud-functions", gcp("gcp_cloud_functions")),
        (
            "source-gcp-cloud-run-incomplete",
            without(gcp("gcp_cloud_run"), "faas.instance"),
        ),
        (
            "source-gcp-cloud-run-job",
            with(gcp("gcp_cloud_run"), kv("gcp.cloud_run.job.execution", string("job-1"))),
        ),
        (
            "source-aws-lambda",
            vec![
                kv("cloud.provider", string("aws")),
                kv("cloud.platform", string("aws_lambda")),
                kv("faas.id", string(LAMBDA_ARN)),
            ],
        ),
        ("source-literal-host-int", vec![kv("host", int(5))]),
        ("source-literal-host-bool", vec![kv("host", boolean(true))]),
        ("source-literal-host-double", vec![kv("host", double(1.5))]),
    ];
    for (index, (id, mut attributes)) in source_cases.into_iter().enumerate() {
        attributes.push(kv("service.name", string("fixture-source")));
        attributes.push(kv("host.name", string("source-host")));
        resources.push(resource_logs(
            attributes,
            vec![scope_logs(no_scope(), vec![record(40 + index as u64, id, vec![])])],
        ));
    }

    ExportLogsServiceRequest {
        resource_logs: resources,
    }
    .encode_to_vec()
}

/// Logs for the processing rules in the case's `datadog.yaml`.
///
/// The rules match the rendered content of each log, so they see its properties as well as its message. Every log
/// whose `fixture_id` starts with `keep` matches the include rule through that property, unless the exclude rule
/// drops it first. The `attr-include` record matches through a separate property. Only string values are used, so
/// that no other gap changes the content.
///
/// The last records are masked differently by the Agent and ADP, through gaps of the processing rules themselves.
fn processing_rule_records(start_index: u64) -> Vec<LogRecord> {
    let record = |index: u64, id: &str, attributes: Vec<KeyValue>| record(start_index + index, id, attributes);
    let with_body = |index: u64, id: &str, body: &str, attributes: Vec<KeyValue>| LogRecord {
        body: Some(string(body)),
        ..record(index, id, attributes)
    };
    vec![
        // Kept by every rule, and left as it is.
        record(0, "keep-plain", vec![]),
        // Masked in the message, with and without a capture group in the placeholder.
        with_body(1, "keep-card", "paid with card=1234-5678 today", vec![]),
        with_body(2, "keep-user", "login by user=alice from the web", vec![]),
        // Masked in a property, across its key and value.
        record(3, "keep-token", vec![kv("token", string("secret-abc123"))]),
        // Matched by the include rule only through a property, so kept.
        record(4, "attr-include", vec![kv("marker", string("keep"))]),
        // Dropped by the exclude rule, through the message and through a property.
        with_body(5, "keep-body-exclude", "healthcheck ok", vec![]),
        record(6, "keep-attr-exclude", vec![kv("probe", string("healthcheck"))]),
        // Dropped by the include rule.
        record(7, "drop-no-marker", vec![]),
        // gap/regex-dialect: `\w` matches only ASCII letters in Go, so the Agent leaves the `é` unmasked.
        with_body(8, "keep-regex-dialect", "ordered word=café today", vec![]),
        // gap/placeholder-expansion: Go expands `$01` to nothing, where ADP expands it to the first group.
        with_body(9, "keep-placeholder-expansion", "linked ref=abc today", vec![]),
        // gap/json-encoding: the Agent's content escapes `<` and `>` as `\u003c` and `\u003e`, which the pattern
        // matches, while ADP's content holds them as they are.
        with_body(10, "keep-json-encoding", "rendered <b> today", vec![]),
    ]
}

/// Builds the fixture of the `otlp-logs-enrichment` case.
///
/// Both sides set `DD_PROVIDER_KIND`, so the tagger's global entity carries `provider_kind`. Each resource exercises
/// how the infra attributes processor places that global tag, and which unified service tag attributes it has.
pub fn enrichment_fixture() -> Vec<u8> {
    let no_scope = InstrumentationScope::default;
    let cases: Vec<(&str, Vec<KeyValue>)> = vec![
        // gap/infra-attributes: the global tag becomes a resource attribute, and so a content property.
        (
            "enrich-all-service-tags",
            vec![
                kv("service.name", string("enrich-service")),
                kv("deployment.environment", string("enrich-env")),
                kv("service.version", string("1.2.3")),
            ],
        ),
        ("enrich-no-service-tags", vec![]),
        (
            "enrich-env-name",
            vec![
                kv("service.name", string("enrich-service")),
                kv("deployment.environment.name", string("enrich-env-name")),
            ],
        ),
        (
            "enrich-both-env-spellings",
            vec![
                kv("deployment.environment", string("enrich-env")),
                kv("deployment.environment.name", string("enrich-env-name")),
            ],
        ),
        // gap/infra-attributes: a global tag overwrites a resource attribute with the same key.
        (
            "enrich-provider-kind-collision",
            vec![kv("provider_kind", string("resource-value"))],
        ),
        // gap/infra-attributes: a container ID that the tagger doesn't know adds no tags, and stays as it is.
        (
            "enrich-unknown-container-id",
            vec![kv(
                "container.id",
                string("0000000000000000000000000000000000000000000000000000000000000000"),
            )],
        ),
        // gap/origin-detection: a process ID that doesn't exist adds no `container.id`.
        ("enrich-unknown-process-id", vec![kv("process.pid", int(2_147_483_647))]),
    ];

    let mut resources = Vec::new();
    for (index, (id, mut attributes)) in cases.into_iter().enumerate() {
        attributes.push(kv("host.name", string("enrich-host")));
        resources.push(resource_logs(
            attributes,
            vec![scope_logs(no_scope(), vec![record(index as u64, id, vec![])])],
        ));
    }

    ExportLogsServiceRequest {
        resource_logs: resources,
    }
    .encode_to_vec()
}

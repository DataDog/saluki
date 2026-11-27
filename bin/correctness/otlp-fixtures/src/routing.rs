//! OTLP fixtures for the `otlp-routing-*` and `otlp-traces-endpoints*` integration cases.
//!
//! A case sends one fixture per signal and protocol, and then checks which software delivered each fixture to the
//! intake. Every fixture holds three items. The names of its items hold the case and the protocol, so the items of one
//! fixture never match the selection of another: metric names start with `otlp.routing.<case>.<protocol>.`, and spans
//! and logs have the service `otlp-routing-<case>-<protocol>`. The `otlp-traces-endpoints*` cases send one fixture for
//! each signal and each arm, and check which intake received it and with which API key.

use otlp_protos::opentelemetry::proto::{
    collector::{logs::v1::ExportLogsServiceRequest, metrics::v1::ExportMetricsServiceRequest},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    metrics::v1::{metric::Data, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics},
    resource::v1::Resource,
};
use prost::Message as _;

use crate::{
    common::{kv, string},
    traces,
};

/// 2023-11-14T22:13:20Z, in nanoseconds.
const TIME_NANOS: u64 = 1_700_000_000_000_000_000;

/// The names of the three items of a fixture. A metric takes its name from the entry and its value from the position,
/// counted from 1.
const ITEMS: [&str; 3] = ["a", "b", "c"];

/// The number of spans in the trace of a traces fixture.
const SPAN_COUNT: u32 = 3;

/// The service of the items of the fixtures for `case` and `protocol`.
fn service(case: &str, protocol: &str) -> String {
    format!("otlp-routing-{}-{}", case, protocol)
}

/// The resource of the items of the fixtures for `case` and `protocol`.
fn resource(case: &str, protocol: &str) -> Resource {
    Resource {
        attributes: vec![kv("service.name", string(&service(case, protocol)))],
        ..Default::default()
    }
}

/// Builds a request with three gauges named `otlp.routing.<case>.<protocol>.{a,b,c}`, with the values 1, 2 and 3.
pub fn metrics_fixture(case: &str, protocol: &str) -> Vec<u8> {
    let metrics = ITEMS
        .iter()
        .zip(1..)
        .map(|(item, value)| Metric {
            name: format!("otlp.routing.{}.{}.{}", case, protocol, item),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    time_unix_nano: TIME_NANOS,
                    value: Some(number_data_point::Value::AsDouble(f64::from(value))),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        })
        .collect();
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource(case, protocol)),
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

/// Builds one trace of three spans, from the service `otlp-routing-<case>-<protocol>`.
///
/// `seed` gives the trace its IDs. Give each fixture that can reach one intake its own seed.
pub fn traces_fixture(case: &str, protocol: &str, seed: u32) -> Vec<u8> {
    traces::trace_fixture(seed, &service(case, protocol), SPAN_COUNT)
}

/// Builds a request with three log records, from the service `otlp-routing-<case>-<protocol>`. Each record has a
/// `fixture_id` attribute that holds the name of its item.
pub fn logs_fixture(case: &str, protocol: &str) -> Vec<u8> {
    let log_records = ITEMS
        .iter()
        .zip(0..)
        .map(|(item, index)| LogRecord {
            time_unix_nano: TIME_NANOS + index * 1_000_000,
            severity_number: 9,
            severity_text: "INFO".to_string(),
            body: Some(string(&format!("{} {}", service(case, protocol), item))),
            attributes: vec![kv("fixture_id", string(item))],
            ..Default::default()
        })
        .collect();
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource(case, protocol)),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::{
        collector::trace::v1::ExportTraceServiceRequest,
        common::v1::{any_value::Value, AnyValue, KeyValue},
    };

    use super::*;

    fn string_value(value: &Option<AnyValue>) -> &str {
        match value.as_ref().and_then(|value| value.value.as_ref()) {
            Some(Value::StringValue(text)) => text,
            other => panic!("not a string: {:?}", other),
        }
    }

    fn service_name(resource: &Option<Resource>) -> &str {
        let attributes: &[KeyValue] = &resource.as_ref().unwrap().attributes;
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].key, "service.name");
        string_value(&attributes[0].value)
    }

    #[test]
    fn metrics_fixture_has_three_gauges_named_for_the_case_and_protocol() {
        let request = ExportMetricsServiceRequest::decode(metrics_fixture("mixed-grpc", "grpc").as_slice()).unwrap();

        let resource_metrics = &request.resource_metrics[0];
        assert_eq!(service_name(&resource_metrics.resource), "otlp-routing-mixed-grpc-grpc");
        let metrics = &resource_metrics.scope_metrics[0].metrics;
        let points: Vec<(&str, Option<number_data_point::Value>)> = metrics
            .iter()
            .map(|metric| {
                let Some(Data::Gauge(gauge)) = &metric.data else {
                    panic!("{} is not a gauge", metric.name);
                };
                assert_eq!(gauge.data_points.len(), 1);
                (metric.name.as_str(), gauge.data_points[0].value)
            })
            .collect();
        assert_eq!(
            points,
            [
                (
                    "otlp.routing.mixed-grpc.grpc.a",
                    Some(number_data_point::Value::AsDouble(1.0))
                ),
                (
                    "otlp.routing.mixed-grpc.grpc.b",
                    Some(number_data_point::Value::AsDouble(2.0))
                ),
                (
                    "otlp.routing.mixed-grpc.grpc.c",
                    Some(number_data_point::Value::AsDouble(3.0))
                ),
            ]
        );
    }

    #[test]
    fn traces_fixture_has_one_trace_of_three_spans_from_the_service() {
        let request = ExportTraceServiceRequest::decode(traces_fixture("internal", "http", 4).as_slice()).unwrap();

        let resource_spans = &request.resource_spans[0];
        assert_eq!(service_name(&resource_spans.resource), "otlp-routing-internal-http");
        let spans = &resource_spans.scope_spans[0].spans;
        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|span| span.trace_id == spans[0].trace_id));
    }

    #[test]
    fn logs_fixture_has_three_records_from_the_service() {
        let request = ExportLogsServiceRequest::decode(logs_fixture("mixed-http", "http").as_slice()).unwrap();

        let resource_logs = &request.resource_logs[0];
        assert_eq!(service_name(&resource_logs.resource), "otlp-routing-mixed-http-http");
        let records = &resource_logs.scope_logs[0].log_records;
        let ids: Vec<&str> = records
            .iter()
            .map(|record| string_value(&record.attributes[0].value))
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(records.iter().all(|record| record.attributes[0].key == "fixture_id"));
    }

    #[test]
    fn fixtures_of_other_cases_and_protocols_share_no_name() {
        // The case and protocol of each fixture that the `otlp-routing-*` cases send.
        let fixtures = [
            ("internal", "grpc"),
            ("internal", "http"),
            ("mixed-grpc", "grpc"),
            ("mixed-http", "http"),
        ];
        for (seed, (case, protocol)) in (1..).zip(fixtures) {
            let metrics = ExportMetricsServiceRequest::decode(metrics_fixture(case, protocol).as_slice()).unwrap();
            let logs = ExportLogsServiceRequest::decode(logs_fixture(case, protocol).as_slice()).unwrap();
            let traces = ExportTraceServiceRequest::decode(traces_fixture(case, protocol, seed).as_slice()).unwrap();
            let names: Vec<&str> = metrics.resource_metrics[0].scope_metrics[0]
                .metrics
                .iter()
                .map(|metric| metric.name.as_str())
                .collect();
            let services = [
                service_name(&metrics.resource_metrics[0].resource),
                service_name(&logs.resource_logs[0].resource),
                service_name(&traces.resource_spans[0].resource),
            ];

            // Each fixture matches the selections of its own `intake_from` steps, and no other fixture's.
            for (other_case, other_protocol) in fixtures {
                let own = (other_case, other_protocol) == (case, protocol);
                let prefix = format!("otlp.routing.{}.{}.", other_case, other_protocol);
                let service = format!("otlp-routing-{}-{}", other_case, other_protocol);
                for name in &names {
                    assert_eq!(
                        name.starts_with(&prefix),
                        own,
                        "{} of {}/{} against {}",
                        name,
                        case,
                        protocol,
                        prefix
                    );
                }
                for found in services {
                    assert_eq!(
                        found == service,
                        own,
                        "{} of {}/{} against {}",
                        found,
                        case,
                        protocol,
                        service
                    );
                }
            }
        }
    }

    #[test]
    fn fixtures_are_deterministic() {
        assert_eq!(metrics_fixture("internal", "grpc"), metrics_fixture("internal", "grpc"));
        assert_eq!(logs_fixture("internal", "grpc"), logs_fixture("internal", "grpc"));
        assert_eq!(
            traces_fixture("internal", "grpc", 3),
            traces_fixture("internal", "grpc", 3)
        );
    }
}

//! OTLP fixtures for the `otlp-rcv-http` integration case.
//!
//! The case sends each body to the OTLP/HTTP receiver of ADP and of the Agent, and then checks which software delivered
//! it to its intake. A metrics body holds one gauge named `otlp.rcv.http.<probe>.probe`, so the metrics of one probe
//! never match the `name_prefix` selection of another. A traces body holds one span from the service `otlp-rcv-http`.
//!
//! A JSON body is the OTLP JSON form of the request that the protobuf body of the same signal holds.

use otlp_protos::opentelemetry::proto::{
    collector::{metrics::v1::ExportMetricsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::{any_value, KeyValue},
    metrics::v1::{metric::Data, number_data_point, Metric, NumberDataPoint},
    resource::v1::Resource,
    trace::v1::Span,
};
use prost::Message as _;
use serde_json::{json, Value};

use crate::{metrics, traces};

/// The directory of the body files, relative to the root of the repository.
pub const BODIES_DIR: &str = "test/integration/cases/otlp-rcv-http/bodies";

/// The `service.name` of the resource of every body.
const SERVICE: &str = "otlp-rcv-http";

/// The seed of the trace of the traces bodies.
const TRACE_SEED: u32 = 1;

/// The probes whose body is a protobuf metrics request. A probe is named for the file of its body.
const PROTOBUF_PROBES: [&str; 8] = [
    "metrics_protobuf",
    "metrics_protobuf_charset",
    "metrics_gzip",
    "metrics_br",
    "metrics_text_plain",
    "metrics_no_content_type",
    "unknown_path",
    "trailing_slash",
];

/// The probes whose body is a JSON metrics request.
const JSON_PROBES: [&str; 1] = ["metrics_json"];

/// The name of the metric that the body of `probe` holds.
fn metric_name(probe: &str) -> String {
    format!("otlp.rcv.http.{}.probe", probe)
}

fn metrics_request(probe: &str) -> ExportMetricsServiceRequest {
    metrics::unit_gauge_request(SERVICE, &metric_name(probe))
}

fn traces_request() -> ExportTraceServiceRequest {
    traces::trace_request(TRACE_SEED, SERVICE, 1)
}

/// Every body file of the case: its name, relative to [`BODIES_DIR`], and its bytes.
pub fn bodies() -> Vec<(String, Vec<u8>)> {
    let mut bodies = Vec::new();
    for probe in PROTOBUF_PROBES {
        bodies.push((format!("{}.pb", probe), metrics_request(probe).encode_to_vec()));
    }
    for probe in JSON_PROBES {
        bodies.push((
            format!("{}.json", probe),
            pretty(&metrics_json(&metrics_request(probe))),
        ));
    }
    bodies.push(("traces.pb".to_string(), traces_request().encode_to_vec()));
    bodies.push(("traces.json".to_string(), pretty(&traces_json(&traces_request()))));
    bodies
}

/// The bytes of a JSON file: indented, with a final newline.
fn pretty(value: &Value) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(value).expect("should not fail to write a JSON value");
    text.push('\n');
    text.into_bytes()
}

/// The OTLP JSON form of `request`, which has camel case names, hexadecimal IDs, and 64-bit integers as strings.
///
/// It covers the parts that the fixtures use, and drops other fields. It panics on a metric that is not a gauge, a
/// point that does not hold a double, and an attribute that does not hold a string.
fn metrics_json(request: &ExportMetricsServiceRequest) -> Value {
    let resource_metrics: Vec<Value> = request
        .resource_metrics
        .iter()
        .map(|resource_metrics| {
            let scope_metrics: Vec<Value> = resource_metrics
                .scope_metrics
                .iter()
                .map(|scope_metrics| {
                    let metrics: Vec<Value> = scope_metrics.metrics.iter().map(metric_json).collect();
                    json!({ "metrics": metrics })
                })
                .collect();
            json!({ "resource": resource_json(&resource_metrics.resource), "scopeMetrics": scope_metrics })
        })
        .collect();
    json!({ "resourceMetrics": resource_metrics })
}

fn metric_json(metric: &Metric) -> Value {
    let Some(Data::Gauge(gauge)) = &metric.data else {
        panic!("the metric '{}' should be a gauge", metric.name);
    };
    let points: Vec<Value> = gauge.data_points.iter().map(point_json).collect();
    json!({ "name": metric.name, "gauge": { "dataPoints": points } })
}

fn point_json(point: &NumberDataPoint) -> Value {
    let Some(number_data_point::Value::AsDouble(value)) = point.value else {
        panic!("a data point should hold a double");
    };
    json!({ "timeUnixNano": point.time_unix_nano.to_string(), "asDouble": value })
}

/// The OTLP JSON form of `request`. See [`metrics_json`].
fn traces_json(request: &ExportTraceServiceRequest) -> Value {
    let resource_spans: Vec<Value> = request
        .resource_spans
        .iter()
        .map(|resource_spans| {
            let scope_spans: Vec<Value> = resource_spans
                .scope_spans
                .iter()
                .map(|scope_spans| {
                    let spans: Vec<Value> = scope_spans.spans.iter().map(span_json).collect();
                    json!({ "spans": spans })
                })
                .collect();
            json!({ "resource": resource_json(&resource_spans.resource), "scopeSpans": scope_spans })
        })
        .collect();
    json!({ "resourceSpans": resource_spans })
}

fn span_json(span: &Span) -> Value {
    let mut json = json!({
        "traceId": hex(&span.trace_id),
        "spanId": hex(&span.span_id),
        "name": span.name,
        "kind": span.kind,
        "startTimeUnixNano": span.start_time_unix_nano.to_string(),
        "endTimeUnixNano": span.end_time_unix_nano.to_string(),
    });
    if !span.parent_span_id.is_empty() {
        json["parentSpanId"] = Value::String(hex(&span.parent_span_id));
    }
    json
}

fn resource_json(resource: &Option<Resource>) -> Value {
    let resource = resource.as_ref().expect("a request should have a resource");
    let attributes: Vec<Value> = resource.attributes.iter().map(attribute_json).collect();
    json!({ "attributes": attributes })
}

fn attribute_json(attribute: &KeyValue) -> Value {
    let Some(any_value::Value::StringValue(text)) = attribute.value.as_ref().and_then(|value| value.value.as_ref())
    else {
        panic!("the attribute '{}' should hold a string", attribute.key);
    };
    json!({ "key": attribute.key, "value": { "stringValue": text } })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(name: &str) -> Vec<u8> {
        bodies()
            .into_iter()
            .find(|(file, _)| file == name)
            .unwrap_or_else(|| panic!("no body '{}'", name))
            .1
    }

    fn json_body(name: &str) -> Value {
        serde_json::from_slice(&body(name)).unwrap()
    }

    fn metric_names(request: &ExportMetricsServiceRequest) -> Vec<&str> {
        request
            .resource_metrics
            .iter()
            .flat_map(|resource| &resource.scope_metrics)
            .flat_map(|scope| &scope.metrics)
            .map(|metric| metric.name.as_str())
            .collect()
    }

    #[test]
    fn each_metrics_body_holds_one_metric_that_no_other_probe_selects() {
        let probes: Vec<&str> = PROTOBUF_PROBES.iter().chain(&JSON_PROBES).copied().collect();
        for probe in &probes {
            let request = metrics_request(probe);
            let names = metric_names(&request);
            assert_eq!(names.len(), 1);
            for other in &probes {
                let prefix = format!("otlp.rcv.http.{}.", other);
                assert_eq!(
                    names[0].starts_with(&prefix),
                    probe == other,
                    "{} against {}",
                    names[0],
                    prefix
                );
            }
        }
    }

    #[test]
    fn bodies_are_one_file_for_each_probe_and_each_traces_encoding() {
        let mut files: Vec<String> = bodies().into_iter().map(|(file, _)| file).collect();
        files.sort_unstable();
        assert_eq!(
            files,
            [
                "metrics_br.pb",
                "metrics_gzip.pb",
                "metrics_json.json",
                "metrics_no_content_type.pb",
                "metrics_protobuf.pb",
                "metrics_protobuf_charset.pb",
                "metrics_text_plain.pb",
                "traces.json",
                "traces.pb",
                "trailing_slash.pb",
                "unknown_path.pb",
            ]
        );
    }

    #[test]
    fn the_protobuf_body_decodes_to_the_request_of_its_probe() {
        let request = ExportMetricsServiceRequest::decode(body("metrics_br.pb").as_slice()).unwrap();

        assert_eq!(request, metrics_request("metrics_br"));
        assert_eq!(metric_names(&request), ["otlp.rcv.http.metrics_br.probe"]);
    }

    #[test]
    fn the_json_metrics_body_holds_what_the_protobuf_request_holds() {
        let request = metrics_request("metrics_json");
        let metric = &request.resource_metrics[0].scope_metrics[0].metrics[0];
        let Some(Data::Gauge(gauge)) = &metric.data else {
            panic!("not a gauge");
        };
        let point = &gauge.data_points[0];
        let Some(number_data_point::Value::AsDouble(value)) = point.value else {
            panic!("not a double");
        };

        assert_eq!(
            json_body("metrics_json.json"),
            json!({
                "resourceMetrics": [{
                    "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "otlp-rcv-http" } }] },
                    "scopeMetrics": [{
                        "metrics": [{
                            "name": metric.name,
                            "gauge": { "dataPoints": [{ "timeUnixNano": point.time_unix_nano.to_string(), "asDouble": value }] },
                        }],
                    }],
                }],
            })
        );
        assert_eq!(metric.name, "otlp.rcv.http.metrics_json.probe");
    }

    #[test]
    fn the_json_traces_body_holds_what_the_protobuf_request_holds() {
        let request = ExportTraceServiceRequest::decode(body("traces.pb").as_slice()).unwrap();
        let span = &request.resource_spans[0].scope_spans[0].spans[0];

        assert_eq!(
            json_body("traces.json"),
            json!({
                "resourceSpans": [{
                    "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "otlp-rcv-http" } }] },
                    "scopeSpans": [{
                        "spans": [{
                            "traceId": "00000000000000000000000000000001",
                            "spanId": "0000000100000001",
                            "name": span.name,
                            "kind": span.kind,
                            "startTimeUnixNano": span.start_time_unix_nano.to_string(),
                            "endTimeUnixNano": span.end_time_unix_nano.to_string(),
                        }],
                    }],
                }],
            })
        );
        assert_eq!(request.resource_spans[0].scope_spans[0].spans.len(), 1);
    }

    #[test]
    fn json_bodies_end_with_a_newline() {
        assert!(body("metrics_json.json").ends_with(b"}\n"));
        assert!(body("traces.json").ends_with(b"}\n"));
    }

    #[test]
    fn bodies_are_deterministic() {
        assert_eq!(bodies(), bodies());
    }

    #[test]
    #[should_panic(expected = "should be a gauge")]
    fn the_json_form_refuses_a_metric_that_is_not_a_gauge() {
        let mut request = metrics_request("metrics_json");
        request.resource_metrics[0].scope_metrics[0].metrics[0].data = None;

        metrics_json(&request);
    }
}

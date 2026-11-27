//! OTLP fixtures for the `otlp-rcv-http` integration case.
//!
//! The case sends each body to the OTLP/HTTP receiver of ADP and of the Agent, and then checks which software delivered
//! it to its intake. A metrics body holds one gauge named `otlp.rcv.http.<probe>.probe`, so the metrics of one probe
//! never match the `name_prefix` selection of another. The `traces` bodies hold one span from the service
//! `otlp-rcv-http`.
//!
//! A JSON body is the OTLP JSON form of the request that the protobuf body of the same signal holds.
//!
//! An ID body is a protobuf traces request with an identifier of the wrong length, which OTLP does not allow. Its
//! spans are from a service of its own, so the intake shows which spans each receiver kept of each body. The service
//! is `otlp-rcv-http`, a hyphen, and the name of the probe in kebab case.

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

/// The seed of the trace of the `traces` bodies.
const TRACE_SEED: u32 = 1;

/// Builds the request of an ID probe, whose spans are from `service`.
type IdRequest = fn(&str) -> ExportTraceServiceRequest;

/// The probes whose body is a protobuf traces request with an identifier of the wrong length, each with the function
/// that builds its request. A probe is named for the file of its body.
const ID_PROBES: [(&str, IdRequest); 3] = [
    ("traces_short_trace_id", short_trace_id_request),
    ("traces_short_span_id", short_span_id_request),
    ("traces_short_parent_id", short_parent_id_request),
];

/// The seeds of the two traces of the `traces_short_trace_id` body: the valid trace, and the trace whose trace ID is
/// too short.
const SHORT_TRACE_ID_SEEDS: [u32; 2] = [2, 3];

/// The seeds of the two traces of the `traces_short_span_id` body: the valid trace, and the trace whose span ID is too
/// short.
const SHORT_SPAN_ID_SEEDS: [u32; 2] = [4, 5];

/// The seed of the one trace of the `traces_short_parent_id` body.
const SHORT_PARENT_ID_SEED: u32 = 6;

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

/// The service of the spans of the body of `probe`: the service of the case, and the name of the probe in kebab case.
fn id_service(probe: &str) -> String {
    format!("{}-{}", SERVICE, probe.replace('_', "-"))
}

/// The spans of a request that has one resource and one scope, as `trace_request` builds it.
fn spans_mut(request: &mut ExportTraceServiceRequest) -> &mut Vec<Span> {
    &mut request.resource_spans[0].scope_spans[0].spans
}

/// Span `index`, counted from 0, of the trace of `span_count` spans that `trace_request` builds for `seed`. It is named
/// `name`.
fn named_span(seed: u32, service: &str, index: usize, span_count: u32, name: &str) -> Span {
    let mut request = traces::trace_request(seed, service, span_count);
    let mut span = spans_mut(&mut request).remove(index);
    span.name = name.to_string();
    span
}

/// A request that holds `spans` in the resource of `service`, which is the resource that `trace_request` builds.
fn spans_request(service: &str, spans: Vec<Span>) -> ExportTraceServiceRequest {
    let mut request = traces::trace_request(TRACE_SEED, service, 1);
    *spans_mut(&mut request) = spans;
    request
}

/// A valid trace, and a span of another trace whose trace ID has 8 bytes.
///
/// ADP groups spans by the low half of the trace ID. The two traces have different low halves, so that a shared half
/// does not make ADP reject the valid trace as well.
fn short_trace_id_request(service: &str) -> ExportTraceServiceRequest {
    let [valid_seed, short_seed] = SHORT_TRACE_ID_SEEDS;
    let valid = named_span(valid_seed, service, 0, 1, "valid");
    let mut short = named_span(short_seed, service, 0, 1, "short");
    short.trace_id = short.trace_id[8..].to_vec();
    spans_request(service, vec![valid, short])
}

/// A valid trace, and a span of another trace whose span ID has 4 bytes.
fn short_span_id_request(service: &str) -> ExportTraceServiceRequest {
    let [valid_seed, short_seed] = SHORT_SPAN_ID_SEEDS;
    let valid = named_span(valid_seed, service, 0, 1, "valid");
    let mut short = named_span(short_seed, service, 0, 1, "short");
    short.span_id.truncate(4);
    spans_request(service, vec![valid, short])
}

/// A root, and two children of it in the same trace: one whose parent ID has 4 bytes, and one whose parent ID has 9
/// bytes.
fn short_parent_id_request(service: &str) -> ExportTraceServiceRequest {
    let root = named_span(SHORT_PARENT_ID_SEED, service, 0, 3, "root");
    let mut short = named_span(SHORT_PARENT_ID_SEED, service, 1, 3, "short");
    short.parent_span_id.truncate(4);
    let mut long = named_span(SHORT_PARENT_ID_SEED, service, 2, 3, "long");
    long.parent_span_id.push(0xff);
    spans_request(service, vec![root, short, long])
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
    for (probe, request) in id_requests() {
        bodies.push((format!("{}.pb", probe), request.encode_to_vec()));
    }
    bodies
}

/// The name and the request of each probe of [`ID_PROBES`].
fn id_requests() -> Vec<(&'static str, ExportTraceServiceRequest)> {
    ID_PROBES
        .iter()
        .map(|(probe, build)| (*probe, build(&id_service(probe))))
        .collect()
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

    /// The service and the spans of the traces body `file`, which holds one resource and one scope.
    fn decoded_traces(file: &str) -> (String, Vec<Span>) {
        let request = ExportTraceServiceRequest::decode(body(file).as_slice()).unwrap();
        let [resource_spans] = <[_; 1]>::try_from(request.resource_spans).expect("one resource");
        let [attribute] = <[_; 1]>::try_from(resource_spans.resource.unwrap().attributes).expect("one attribute");
        assert_eq!(attribute.key, "service.name");
        let Some(any_value::Value::StringValue(service)) = attribute.value.unwrap().value else {
            panic!("the service should be a string");
        };
        let [scope_spans] = <[_; 1]>::try_from(resource_spans.scope_spans).expect("one scope");
        (service, scope_spans.spans)
    }

    /// The low half of a trace ID, which is how ADP groups spans: the last 8 bytes, or all of an ID with fewer.
    fn low_half(trace_id: &[u8]) -> &[u8] {
        &trace_id[trace_id.len().saturating_sub(8)..]
    }

    /// Whether every byte string in `ids` has a byte that is not zero, since ADP and the Agent drop an ID of zeros.
    fn all_nonzero(ids: &[&[u8]]) -> bool {
        ids.iter().all(|id| id.iter().any(|byte| *byte != 0))
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
                "traces_short_parent_id.pb",
                "traces_short_span_id.pb",
                "traces_short_trace_id.pb",
                "trailing_slash.pb",
                "unknown_path.pb",
            ]
        );
    }

    #[test]
    fn the_short_trace_id_body_holds_a_valid_trace_and_a_trace_id_of_8_bytes() {
        let (service, spans) = decoded_traces("traces_short_trace_id.pb");
        let [valid, short] = <[_; 2]>::try_from(spans).expect("two spans");

        assert_eq!(service, "otlp-rcv-http-traces-short-trace-id");
        assert_eq!((valid.trace_id.len(), short.trace_id.len()), (16, 8));
        assert_eq!((valid.span_id.len(), short.span_id.len()), (8, 8));
        assert!(valid.parent_span_id.is_empty() && short.parent_span_id.is_empty());
        assert_ne!(
            low_half(&valid.trace_id),
            low_half(&short.trace_id),
            "ADP groups spans by the low half, and a shared half makes it reject the valid trace too"
        );
        assert!(all_nonzero(&[
            &valid.trace_id,
            &valid.span_id,
            &short.trace_id,
            &short.span_id
        ]));
    }

    #[test]
    fn the_short_span_id_body_holds_a_valid_trace_and_a_span_id_of_4_bytes() {
        let (service, spans) = decoded_traces("traces_short_span_id.pb");
        let [valid, short] = <[_; 2]>::try_from(spans).expect("two spans");

        assert_eq!(service, "otlp-rcv-http-traces-short-span-id");
        assert_eq!((valid.span_id.len(), short.span_id.len()), (8, 4));
        assert_eq!((valid.trace_id.len(), short.trace_id.len()), (16, 16));
        assert!(valid.parent_span_id.is_empty() && short.parent_span_id.is_empty());
        assert_ne!(
            low_half(&valid.trace_id),
            low_half(&short.trace_id),
            "the traces are two, so that the valid trace survives"
        );
        assert!(all_nonzero(&[
            &valid.trace_id,
            &valid.span_id,
            &short.trace_id,
            &short.span_id
        ]));
    }

    #[test]
    fn the_short_parent_id_body_holds_a_root_and_two_children_with_parents_of_wrong_lengths() {
        let (service, spans) = decoded_traces("traces_short_parent_id.pb");
        let [root, short, long] = <[_; 3]>::try_from(spans).expect("three spans");

        assert_eq!(service, "otlp-rcv-http-traces-short-parent-id");
        assert_eq!(short.trace_id, root.trace_id, "one trace");
        assert_eq!(long.trace_id, root.trace_id, "one trace");
        assert_eq!(root.trace_id.len(), 16);
        assert_eq!(
            (
                root.parent_span_id.len(),
                short.parent_span_id.len(),
                long.parent_span_id.len()
            ),
            (0, 4, 9)
        );
        for span in [&root, &short, &long] {
            assert_eq!(span.span_id.len(), 8);
        }
        assert!(root.span_id != short.span_id && root.span_id != long.span_id && short.span_id != long.span_id);
        assert!(all_nonzero(&[
            &root.trace_id,
            &root.span_id,
            &short.span_id,
            &short.parent_span_id,
            &long.span_id,
            &long.parent_span_id
        ]));
    }

    #[test]
    fn each_id_body_has_a_service_of_its_own_and_spans_that_no_other_body_shares() {
        let files = [
            "traces.pb",
            "traces_short_trace_id.pb",
            "traces_short_span_id.pb",
            "traces_short_parent_id.pb",
        ];
        let decoded: Vec<_> = files.iter().map(|file| decoded_traces(file)).collect();
        let services: Vec<&str> = decoded.iter().map(|(service, _)| service.as_str()).collect();
        let mut halves: Vec<&[u8]> = decoded
            .iter()
            .flat_map(|(_, spans)| spans.iter().map(|span| low_half(&span.trace_id)))
            .collect();

        assert_eq!(
            services,
            [
                "otlp-rcv-http",
                "otlp-rcv-http-traces-short-trace-id",
                "otlp-rcv-http-traces-short-span-id",
                "otlp-rcv-http-traces-short-parent-id",
            ]
        );
        assert_eq!(halves.len(), 8, "spans");
        halves.sort_unstable();
        halves.dedup();
        assert_eq!(
            halves.len(),
            6,
            "traces: the bodies hold 1, 2, 2 and 1, and none shares a trace"
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

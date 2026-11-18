//! OTLP metrics fixtures, for the `otlp-metrics-*` cases.

use otlp_protos::opentelemetry::proto::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{InstrumentationScope, KeyValue},
    metrics::v1::{
        metric::Data, number_data_point, summary_data_point::ValueAtQuantile, AggregationTemporality, Gauge, Histogram,
        HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, Summary, SummaryDataPoint,
    },
    resource::v1::Resource,
};
use prost::Message as _;

use crate::common::{kv, string};

/// One second after the Unix epoch, in nanoseconds.
const ONE_SECOND_NANOS: u64 = 1_000_000_000;

/// 2023-11-14T22:13:20Z, in nanoseconds.
const NOV_2023_NANOS: u64 = 1_700_000_000_000_000_000;

fn point(time_unix_nano: u64, value: f64, attributes: Vec<KeyValue>) -> NumberDataPoint {
    NumberDataPoint {
        attributes,
        time_unix_nano,
        value: Some(number_data_point::Value::AsDouble(value)),
        ..Default::default()
    }
}

fn metric(name: &str, data: Data) -> Metric {
    Metric {
        name: name.to_string(),
        data: Some(data),
        ..Default::default()
    }
}

fn gauge(name: &str, data_points: Vec<NumberDataPoint>) -> Metric {
    metric(name, Data::Gauge(Gauge { data_points }))
}

/// A gauge with a single point of value 1, one second after the epoch.
fn unit_gauge(name: &str) -> Metric {
    gauge(name, vec![point(ONE_SECOND_NANOS, 1.0, vec![])])
}

fn monotonic_sum(name: &str, temporality: AggregationTemporality, data_points: Vec<NumberDataPoint>) -> Metric {
    metric(
        name,
        Data::Sum(Sum {
            data_points,
            aggregation_temporality: temporality as i32,
            is_monotonic: true,
        }),
    )
}

fn resource(attributes: Vec<KeyValue>) -> Option<Resource> {
    Some(Resource {
        attributes,
        ..Default::default()
    })
}

/// Builds a `ResourceMetrics` with a single `ScopeMetrics`.
fn resource_metrics(
    resource: Option<Resource>, scope: Option<InstrumentationScope>, metrics: Vec<Metric>,
) -> ResourceMetrics {
    ResourceMetrics {
        resource,
        scope_metrics: vec![ScopeMetrics {
            scope,
            metrics,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn request(resource_metrics: Vec<ResourceMetrics>) -> Vec<u8> {
    ExportMetricsServiceRequest { resource_metrics }.encode_to_vec()
}

/// Builds a request with a single resource whose `service.name` is `otlp-test`, plus `attributes`.
fn service_request(attributes: Vec<KeyValue>, scope: Option<InstrumentationScope>, metric: Metric) -> Vec<u8> {
    let mut resource_attributes = vec![kv("service.name", string("otlp-test"))];
    resource_attributes.extend(attributes);
    request(vec![resource_metrics(
        resource(resource_attributes),
        scope,
        vec![metric],
    )])
}

/// A `otlphost.metric` gauge whose value identifies which resource it came from.
fn host_gauge(resource: Option<Resource>, value: f64) -> ResourceMetrics {
    resource_metrics(
        resource,
        None,
        vec![gauge("otlphost.metric", vec![point(ONE_SECOND_NANOS, value, vec![])])],
    )
}

fn host_name(name: &str) -> Option<Resource> {
    resource(vec![kv("host.name", string(name))])
}

/// Host fallbacks: no resource, an empty `host.name`, the Agent's own hostname, and a custom one, then Azure VM-ID
/// precedence over `host.name` and the `host.name` fallback without a VM ID.
pub fn host_fixture() -> Vec<u8> {
    request(vec![
        host_gauge(None, 1.0),
        host_gauge(host_name(""), 2.0),
        host_gauge(host_name("correctness-testing"), 3.0),
        host_gauge(host_name("custom"), 4.0),
        host_gauge(
            resource(vec![
                kv("cloud.provider", string("azure")),
                kv("host.id", string("azure-vm-id")),
                kv("host.name", string("azure-host-name")),
            ]),
            5.0,
        ),
        host_gauge(
            resource(vec![
                kv("cloud.provider", string("azure")),
                kv("host.name", string("azure-host-name")),
            ]),
            6.0,
        ),
    ])
}

/// Four gauge points that differ only by resource host, starting with an empty resource.
pub fn configured_tags_fixture() -> Vec<u8> {
    request(vec![
        host_gauge(resource(vec![]), 1.0),
        host_gauge(host_name(""), 2.0),
        host_gauge(host_name("correctness-testing"), 3.0),
        host_gauge(host_name("custom"), 4.0),
    ])
}

/// One gauge with a semantic-convention and a custom resource attribute.
pub fn resource_attributes_fixture() -> Vec<u8> {
    service_request(
        vec![kv("custom.resource.attribute", string("present"))],
        None,
        unit_gauge("otlpresource.metric"),
    )
}

/// One gauge with the core mappings and both environment convention forms.
pub fn core_resource_attributes_fixture() -> Vec<u8> {
    service_request(
        vec![
            kv("service.instance.id", string("instance-42")),
            kv("deployment.environment.name", string("production")),
            kv("deployment.environment", string("legacy")),
            kv("custom.resource.attribute", string("present")),
        ],
        None,
        unit_gauge("otlpresource.metric"),
    )
}

/// An AWS resource with both an EC2 instance ID and a `host.name`.
pub fn ec2_host_fixture() -> Vec<u8> {
    service_request(
        vec![
            kv("cloud.provider", string("aws")),
            kv("host.id", string("i-1234567890abcdef0")),
            kv("host.name", string("ec2-1-2-3-4.compute.internal")),
        ],
        None,
        unit_gauge("otlp.ec2.metric"),
    )
}

/// A GCP resource with an instance `host.name` and a project ID.
pub fn gcp_host_fixture() -> Vec<u8> {
    service_request(
        vec![
            kv("cloud.provider", string("gcp")),
            kv("host.name", string("my-instance.c.my-project.internal")),
            kv("cloud.account.id", string("my-project-id")),
        ],
        None,
        unit_gauge("otlp.gcp.metric"),
    )
}

/// An ECS Fargate resource with a task ARN and no host.
pub fn fargate_fixture() -> Vec<u8> {
    service_request(
        vec![
            kv("aws.ecs.launchtype", string("fargate")),
            kv(
                "aws.ecs.task.arn",
                string("arn:aws:ecs:us-east-1:123456789012:task/test-cluster/test-task-id"),
            ),
        ],
        None,
        unit_gauge("otlp.fargate.metric"),
    )
}

/// Instrumentation scope name, version, and attributes.
pub fn scope_metadata_fixture() -> Vec<u8> {
    service_request(
        vec![],
        Some(InstrumentationScope {
            name: "my-instrument".to_string(),
            version: "1.0.0".to_string(),
            attributes: vec![kv("scope.attr", string("scope-value"))],
            ..Default::default()
        }),
        unit_gauge("otlp.scope.metric"),
    )
}

/// A delta monotonic sum that `datadog.metric.as_type` turns into a rate.
pub fn as_type_rate_fixture() -> Vec<u8> {
    service_request(
        vec![],
        None,
        monotonic_sum(
            "otlp.rate.sum",
            AggregationTemporality::Delta,
            vec![point(
                ONE_SECOND_NANOS,
                5.0,
                vec![kv("datadog.metric.as_type", string("rate"))],
            )],
        ),
    )
}

/// Two points of a cumulative monotonic sum, ten seconds apart.
pub fn cumulative_monotonic_fixture() -> Vec<u8> {
    service_request(
        vec![],
        None,
        monotonic_sum(
            "otlp.cumulative.monotonic",
            AggregationTemporality::Cumulative,
            vec![
                point(ONE_SECOND_NANOS, 10.0, vec![]),
                point(11 * ONE_SECOND_NANOS, 25.0, vec![]),
            ],
        ),
    )
}

/// One valid delta histogram with two explicit bounds and three bucket counts.
///
/// Shared by the `otlp-metrics-histogram-counters` and `otlp-metrics-histogram-nobuckets` cases, which differ only in
/// histogram mode.
pub fn histogram_fixture() -> Vec<u8> {
    let histogram = Histogram {
        data_points: vec![HistogramDataPoint {
            start_time_unix_nano: ONE_SECOND_NANOS,
            time_unix_nano: 2 * ONE_SECOND_NANOS,
            count: 6,
            sum: Some(23.5),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 10.0],
            ..Default::default()
        }],
        aggregation_temporality: AggregationTemporality::Delta as i32,
    };
    request(vec![resource_metrics(
        None,
        None,
        vec![metric("otlp.histogram", Data::Histogram(histogram))],
    )])
}

/// One summary point with two quantiles.
pub fn summary_fixture() -> Vec<u8> {
    let summary = Summary {
        data_points: vec![SummaryDataPoint {
            time_unix_nano: NOV_2023_NANOS,
            count: 10,
            sum: 100.0,
            quantile_values: vec![
                ValueAtQuantile {
                    quantile: 0.5,
                    value: 12.5,
                },
                ValueAtQuantile {
                    quantile: 0.99,
                    value: 25.0,
                },
            ],
            ..Default::default()
        }],
    };
    request(vec![resource_metrics(
        None,
        None,
        vec![metric("otlp.summary.quantiles", Data::Summary(summary))],
    )])
}

/// Kafka metric names that historically triggered remapping, tag copying, filter fan-out, and collision renaming.
pub fn kafka_parity_fixture() -> Vec<u8> {
    let tagged = |key: &str, values: &[&str]| {
        values
            .iter()
            .zip(1..)
            .map(|(value, n)| point(NOV_2023_NANOS, f64::from(n), vec![kv(key, string(value))]))
            .collect::<Vec<_>>()
    };
    request(vec![resource_metrics(
        None,
        None,
        vec![
            gauge("kafka.producer.request-rate", tagged("client-id", &["producer-1"])),
            gauge("kafka.producer.byte-rate", tagged("client-id", &["producer-1"])),
            gauge("kafka.network.io", tagged("state", &["out", "in", "unknown"])),
            gauge(
                "kafka.request.time.avg",
                tagged("type", &["produce", "fetchconsumer", "fetchfollower"]),
            ),
            gauge("kafka.purgatory.size", tagged("type", &["unexpected"])),
            gauge("kafka.consumer_group.offset", tagged("group", &["analytics"])),
        ],
    )])
}

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

use crate::common::{boolean, double, int, kv, string, LAMBDA_ARN};

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
/// precedence over `host.name` and the `host.name` fallback without a VM ID. Then one `otlpsource.<case>` gauge per
/// source case: serverless sources have no host, an incomplete identity falls back to `host.name`, except on Cloud
/// Run, which never falls back, and a literal `host` of any type takes precedence over `host.name`.
pub fn host_fixture() -> Vec<u8> {
    let mut resources = vec![
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
    ];
    resources.extend(source_cases().into_iter().map(|(name, attributes)| {
        resource_metrics(
            resource(attributes),
            None,
            vec![gauge(
                &format!("otlpsource.{name}"),
                vec![point(ONE_SECOND_NANOS, 1.0, vec![])],
            )],
        )
    }));
    request(resources)
}

/// The resource attributes of an AWS Lambda function. The function is the source of its metrics, so they have no host,
/// although the resource has a `host.name`.
fn aws_lambda_attributes() -> Vec<KeyValue> {
    vec![
        kv("cloud.provider", string("aws")),
        kv("cloud.platform", string("aws_lambda")),
        kv("faas.id", string(LAMBDA_ARN)),
        kv("host.name", string("source-host")),
    ]
}

/// Source cases for `host_fixture`, each named for its gauge.
fn source_cases() -> Vec<(&'static str, Vec<KeyValue>)> {
    let azure = |platform: &str| {
        vec![
            kv("cloud.platform", string(platform)),
            kv("service.name", string("fixture-app")),
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
    let with_host = |mut attributes: Vec<KeyValue>| {
        attributes.push(kv("host.name", string("source-host")));
        attributes
    };

    vec![
        ("host_name", with_host(vec![])),
        ("azure_functions", with_host(azure("azure.functions"))),
        (
            "azure_functions_incomplete",
            with_host(without(azure("azure.functions"), "azure.resource_group.name")),
        ),
        ("azure_app_service", with_host(azure("azure.app_service"))),
        (
            "azure_app_service_incomplete",
            with_host(without(azure("azure.app_service"), "cloud.account.id")),
        ),
        ("azure_container_apps", {
            let mut attributes = with_host(azure("azure.container_apps"));
            attributes.push(kv("azure.container_app.instance.id", string("replica-1")));
            attributes
        }),
        (
            "azure_container_apps_no_replica",
            with_host(azure("azure.container_apps")),
        ),
        ("aws_lambda", aws_lambda_attributes()),
        ("gcp_cloud_run", with_host(gcp("gcp_cloud_run"))),
        ("gcp_cloud_functions", with_host(gcp("gcp_cloud_functions"))),
        ("gcp_cloud_run_job", {
            let mut attributes = with_host(gcp("gcp_cloud_run"));
            attributes.push(kv("gcp.cloud_run.job.execution", string("job-1")));
            attributes
        }),
        (
            "gcp_cloud_run_incomplete",
            with_host(without(gcp("gcp_cloud_run"), "faas.instance")),
        ),
        (
            "gcp_cloud_run_incomplete_no_host",
            without(gcp("gcp_cloud_run"), "faas.instance"),
        ),
        ("unknown_platform_no_host", vec![kv("cloud.platform", string("other"))]),
        (
            "non_string_datadog_host_name",
            with_host(vec![kv("datadog.host.name", int(1))]),
        ),
        ("literal_host_int", with_host(vec![kv("host", int(5))])),
        ("literal_host_bool", with_host(vec![kv("host", boolean(true))])),
        ("literal_host_double", with_host(vec![kv("host", double(1.5))])),
    ]
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

/// Two cumulative monotonic sums, each with two points ten seconds apart.
///
/// ADP's list of sums to report as a gauge of their rate holds `raymond.test.cumulative.as.gauge`, and the Agent's
/// list does not.
pub fn cumulative_monotonic_fixture() -> Vec<u8> {
    let sum = |name, first, second| {
        monotonic_sum(
            name,
            AggregationTemporality::Cumulative,
            vec![
                point(ONE_SECOND_NANOS, first, vec![]),
                point(11 * ONE_SECOND_NANOS, second, vec![]),
            ],
        )
    };
    request(vec![resource_metrics(
        resource(vec![kv("service.name", string("otlp-test"))]),
        None,
        vec![
            sum("otlp.cumulative.monotonic", 10.0, 25.0),
            sum("raymond.test.cumulative.as.gauge", 100.0, 130.0),
        ],
    )])
}

/// One valid delta histogram with two explicit bounds and three bucket counts.
///
/// Shared by the `otlp-metrics-histogram-counters` and `otlp-metrics-histogram-nobuckets` cases, which differ only in
/// histogram mode.
pub fn histogram_fixture() -> Vec<u8> {
    request(vec![resource_metrics(
        None,
        None,
        vec![delta_histogram(
            "otlp.histogram",
            ONE_SECOND_NANOS,
            2 * ONE_SECOND_NANOS,
        )],
    )])
}

/// A delta histogram with one point of count 6 and sum 23.5. The explicit bounds are 1 and 10, and the bucket counts are
/// 1, 2 and 3. The point has no minimum or maximum.
fn delta_histogram(name: &str, start_time_unix_nano: u64, time_unix_nano: u64) -> Metric {
    let histogram = Histogram {
        data_points: vec![HistogramDataPoint {
            start_time_unix_nano,
            time_unix_nano,
            count: 6,
            sum: Some(23.5),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 10.0],
            ..Default::default()
        }],
        aggregation_temporality: AggregationTemporality::Delta as i32,
    };
    metric(name, Data::Histogram(histogram))
}

/// Two cumulative points of a summary with two quantiles, ten seconds apart.
///
/// Summary counts and sums are cumulative. A receiver that sends them as deltas, as the Agent and ADP do, uses the
/// first point as the start value only, and sends the count and sum deltas of the second.
pub fn summary_fixture() -> Vec<u8> {
    let summary_point = |time_unix_nano, count, sum| SummaryDataPoint {
        time_unix_nano,
        count,
        sum,
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
    };
    let summary = Summary {
        data_points: vec![
            summary_point(NOV_2023_NANOS, 10, 100.0),
            summary_point(NOV_2023_NANOS + 10 * ONE_SECOND_NANOS, 25, 260.0),
        ],
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

/// Three gauges named `otlpprobe.ctl.{a,b,c}`, with the values 1, 2 and 3.
///
/// The `otlp-metrics` case sends them next to its random load, whose metric names are random, so that the case can
/// check which software sent a known metric to the intake.
pub fn control_fixture() -> Vec<u8> {
    let gauges = ["a", "b", "c"]
        .iter()
        .zip(1..)
        .map(|(item, value)| {
            gauge(
                &format!("otlpprobe.ctl.{item}"),
                vec![point(NOV_2023_NANOS, f64::from(value), vec![])],
            )
        })
        .collect();
    request(vec![resource_metrics(None, None, gauges)])
}

/// Series and sketches that a case sends over the v3 series and v3 sketches paths of an intake.
///
/// The fixture holds 3 series points whose names start with `otlpprobe.v3.series.`, and 2 sketch points whose names
/// start with `otlpprobe.v3.sketch.`. The prefix tells a case which intake path the points must arrive on. A resource
/// with a host holds a gauge, a delta monotonic sum and a histogram. A Lambda resource, which gives its metrics no
/// host, holds a gauge and a histogram. So the series and the sketches each include a point with a host and a point
/// without one. A new probe in the fixture takes one of the two prefixes, and changes the counts.
pub fn v3_fixture() -> Vec<u8> {
    request(vec![
        resource_metrics(
            resource(vec![
                kv("service.name", string("otlp-test")),
                kv("host.name", string("v3-host")),
            ]),
            None,
            vec![
                gauge("otlpprobe.v3.series.gauge", vec![point(NOV_2023_NANOS, 1.5, vec![])]),
                monotonic_sum(
                    "otlpprobe.v3.series.count",
                    AggregationTemporality::Delta,
                    vec![point(NOV_2023_NANOS, 5.0, vec![])],
                ),
                delta_histogram(
                    "otlpprobe.v3.sketch.histogram",
                    NOV_2023_NANOS,
                    NOV_2023_NANOS + 10 * ONE_SECOND_NANOS,
                ),
            ],
        ),
        resource_metrics(
            resource(aws_lambda_attributes()),
            None,
            vec![
                gauge("otlpprobe.v3.series.hostless", vec![point(NOV_2023_NANOS, 2.0, vec![])]),
                delta_histogram(
                    "otlpprobe.v3.sketch.hostless",
                    NOV_2023_NANOS,
                    NOV_2023_NANOS + 10 * ONE_SECOND_NANOS,
                ),
            ],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_fixture_sends_series_and_histograms_under_their_own_prefixes() {
        let request = ExportMetricsServiceRequest::decode(v3_fixture().as_slice()).unwrap();
        let (mut series, mut sketches) = (0, 0);
        for metric in request
            .resource_metrics
            .iter()
            .flat_map(|resource| &resource.scope_metrics)
            .flat_map(|scope| &scope.metrics)
        {
            match (&metric.data, metric.name.as_str()) {
                (Some(Data::Gauge(Gauge { data_points })), name) if name.starts_with("otlpprobe.v3.series.") => {
                    series += data_points.len();
                }
                (Some(Data::Sum(Sum { data_points, .. })), name) if name.starts_with("otlpprobe.v3.series.") => {
                    series += data_points.len();
                }
                (Some(Data::Histogram(Histogram { data_points, .. })), name)
                    if name.starts_with("otlpprobe.v3.sketch.") =>
                {
                    sketches += data_points.len();
                }
                (_, name) => panic!("'{}' is not a probe of its prefix", name),
            }
        }
        assert_eq!((series, sketches), (3, 2));
    }
}

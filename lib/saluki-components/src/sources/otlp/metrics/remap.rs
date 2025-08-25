//! Mappings for OTLP metrics to Datadog-conventional metric names for remapping and renaming.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use otlp_protos::opentelemetry::proto::{
    common::v1::{any_value::Value, KeyValue},
    metrics::v1::{
        metric::Data as OtlpMetricData, number_data_point::Value as OtlpNumberValue, Metric as OtlpMetric,
        NumberDataPoint as OtlpNumberDataPoint,
    },
};

const DIV_MEBIBYTES: f64 = 1024.0 * 1024.0;
const DIV_PERCENTAGE: f64 = 0.01;

/// Extracts any Datadog-specific metrics from a metric and appends them to the new_metrics vector.
pub(super) fn remap_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    remap_system_metrics(new_metrics, metric);
    remap_container_metrics(new_metrics, metric);
    remap_kafka_metrics(new_metrics, metric);
    remap_jvm_metrics(new_metrics, metric);
}

/// Renames the given metric by adding the `otel.` prefix.
pub(super) fn rename_metric(metric: &mut OtlpMetric) {
    rename_host_metrics(metric);
    rename_kafka_metrics(metric);
    rename_agent_internal_otel_metric(metric);
}

// Represents a key/value pair for attribute filtering.
struct Kv {
    k: &'static str,
    v: &'static str,
}

// Contains mappings for attributes from OTel to Datadog.
struct AttributesMapping {
    // Attributes to be added with a fixed, known value.
    fixed: HashMap<&'static str, &'static str>,
    // Attributes whose values are copied from another attribute on the same data point.
    dynamic: HashMap<&'static str, &'static str>,
}

impl AttributesMapping {
    fn empty() -> Self {
        Self {
            fixed: HashMap::new(),
            dynamic: HashMap::new(),
        }
    }
}

fn remap_system_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    let name = metric.name.as_str();
    if !is_host_metric(name) {
        return;
    }
    match name {
        "system.cpu.load_average.1m" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.load.1",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "system.cpu.load_average.5m" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.load.5",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "system.cpu.load_average.15m" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.load.15",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "system.cpu.utilization" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.cpu.idle",
                DIV_PERCENTAGE,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "idle" }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.cpu.user",
                DIV_PERCENTAGE,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "user" }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.cpu.system",
                DIV_PERCENTAGE,
                AttributesMapping::empty(),
                &[Kv {
                    k: "state",
                    v: "system",
                }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.cpu.iowait",
                DIV_PERCENTAGE,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "wait" }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.cpu.stolen",
                DIV_PERCENTAGE,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "steal" }],
            );
        }
        "system.memory.usage" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.mem.total",
                DIV_MEBIBYTES,
                AttributesMapping::empty(),
                &[],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.mem.usable",
                DIV_MEBIBYTES,
                AttributesMapping::empty(),
                &[
                    Kv { k: "state", v: "free" },
                    Kv {
                        k: "state",
                        v: "cached",
                    },
                    Kv {
                        k: "state",
                        v: "buffered",
                    },
                ],
            );
        }
        "system.network.io" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.net.bytes_rcvd",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "direction",
                    v: "receive",
                }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.net.bytes_sent",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "direction",
                    v: "transmit",
                }],
            );
        }
        "system.paging.usage" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.swap.free",
                DIV_MEBIBYTES,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "free" }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.swap.used",
                DIV_MEBIBYTES,
                AttributesMapping::empty(),
                &[Kv { k: "state", v: "used" }],
            );
        }
        "system.filesystem.utilization" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "system.disk.in_use",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        _ => {}
    }
}

fn remap_container_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    let name = metric.name.as_str();
    if !name.starts_with("container.") {
        return;
    }
    match name {
        "container.cpu.usage.total" => {
            if copy_metric_with_attr(
                new_metrics,
                metric,
                "container.cpu.usage",
                1.0,
                AttributesMapping::empty(),
                &[],
            ) {
                if let Some(addm) = new_metrics.last_mut() {
                    addm.unit = "nanocore".to_string();
                }
            }
        }
        "container.cpu.usage.usermode" => {
            if copy_metric_with_attr(
                new_metrics,
                metric,
                "container.cpu.user",
                1.0,
                AttributesMapping::empty(),
                &[],
            ) {
                if let Some(addm) = new_metrics.last_mut() {
                    addm.unit = "nanocore".to_string();
                }
            }
        }
        "container.cpu.usage.system" => {
            if copy_metric_with_attr(
                new_metrics,
                metric,
                "container.cpu.system",
                1.0,
                AttributesMapping::empty(),
                &[],
            ) {
                if let Some(addm) = new_metrics.last_mut() {
                    addm.unit = "nanocore".to_string();
                }
            }
        }
        "container.cpu.throttling_data.throttled_time" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.cpu.throttled",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.cpu.throttling_data.throttled_periods" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.cpu.throttled.periods",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.usage.total" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.usage",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.active_anon" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.kernel",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.hierarchical_memory_limit" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.limit",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.usage.limit" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.soft_limit",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.total_cache" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.cache",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.memory.total_swap" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.memory.swap",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.blockio.io_service_bytes_recursive" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.io.write",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "operation",
                    v: "write",
                }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.io.read",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "operation",
                    v: "read",
                }],
            );
        }
        "container.blockio.io_serviced_recursive" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.io.write.operations",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "operation",
                    v: "write",
                }],
            );
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.io.read.operations",
                1.0,
                AttributesMapping::empty(),
                &[Kv {
                    k: "operation",
                    v: "read",
                }],
            );
        }
        "container.network.io.usage.tx_bytes" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.net.sent",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.network.io.usage.tx_packets" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.net.sent.packets",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.network.io.usage.rx_bytes" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.net.rcvd",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        "container.network.io.usage.rx_packets" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "container.net.rcvd.packets",
                1.0,
                AttributesMapping::empty(),
                &[],
            );
        }
        _ => {}
    }
}

fn remap_kafka_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    let name = metric.name.as_str();
    if !name.starts_with("kafka.") {
        return;
    }
    match name {
        "kafka.consumer.fetch_latency" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "kafka.consumer.fetch_latency",
                1.0,
                AttributesMapping {
                    dynamic: HashMap::from([("group", "consumer_group")]),
                    ..AttributesMapping::empty()
                },
                &[],
            );
        }
        "kafka.consumer.records_lag" => {
            copy_metric_with_attr(
                new_metrics,
                metric,
                "kafka.consumer_lag",
                1.0,
                AttributesMapping {
                    dynamic: HashMap::from([("group", "consumer_group")]),
                    ..AttributesMapping::empty()
                },
                &[],
            );
        }
        _ => {}
    }
}

fn remap_jvm_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    if metric.name.as_str() == "jvm.memory.committed" {
        copy_metric_with_attr(
            new_metrics,
            metric,
            "jvm.heap_memory_committed",
            1.0,
            AttributesMapping::empty(),
            &[Kv { k: "pool", v: "heap" }],
        );
        copy_metric_with_attr(
            new_metrics,
            metric,
            "jvm.non_heap_memory_committed",
            1.0,
            AttributesMapping::empty(),
            &[Kv {
                k: "pool",
                v: "non-heap",
            }],
        );
    }
}

fn is_host_metric(name: &str) -> bool {
    name.starts_with("process.") || name.starts_with("system.")
}

fn rename_host_metrics(metric: &mut OtlpMetric) {
    if is_host_metric(metric.name.as_str()) {
        metric.name = format!("otel.{}", metric.name);
    }
}

fn rename_kafka_metrics(metric: &mut OtlpMetric) {
    if KAFKA_METRICS_TO_RENAME.contains(metric.name.as_str()) {
        metric.name = format!("otel.{}", metric.name);
    }
}

fn copy_metric_with_attr(
    dest: &mut Vec<OtlpMetric>, m: &OtlpMetric, newname: &str, div: f64, attributes_mapping: AttributesMapping,
    filter: &[Kv],
) -> bool {
    let mut new_metric = m.clone();
    new_metric.name = newname.to_string();

    let data_points = match new_metric.data {
        Some(OtlpMetricData::Gauge(ref mut gauge)) => &mut gauge.data_points,
        Some(OtlpMetricData::Sum(ref mut sum)) => &mut sum.data_points,
        _ => return false,
    };

    let mut kept_dps = Vec::new();
    for mut dp in data_points.drain(..) {
        if !has_any(&dp, filter) {
            continue; // Remove data point if it doesn't match the filter.
        }

        // Apply division logic.
        match dp.value {
            Some(OtlpNumberValue::AsInt(ref mut i)) => {
                if div >= 1.0 {
                    *i /= div as i64;
                }
            }
            Some(OtlpNumberValue::AsDouble(ref mut d)) => {
                if div != 0.0 {
                    *d /= div;
                }
            }
            None => {}
        }

        // Apply attribute mappings.
        for (k, v) in &attributes_mapping.fixed {
            dp.attributes.push(KeyValue {
                key: k.to_string(),
                value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                    value: Some(Value::StringValue(v.to_string())),
                }),
            });
        }
        for (old_key, new_key) in &attributes_mapping.dynamic {
            if let Some(kv) = dp.attributes.iter().find(|kv| kv.key == *old_key) {
                if let Some(value_to_copy) = kv.value.clone() {
                    dp.attributes.push(KeyValue {
                        key: new_key.to_string(),
                        value: Some(value_to_copy),
                    });
                }
            }
        }
        kept_dps.push(dp);
    }
    *data_points = kept_dps;

    let has_data_points = match &new_metric.data {
        Some(OtlpMetricData::Gauge(gauge)) => !gauge.data_points.is_empty(),
        Some(OtlpMetricData::Sum(sum)) => !sum.data_points.is_empty(),
        _ => false,
    };

    if has_data_points {
        dest.push(new_metric);
        true
    } else {
        false
    }
}

/// Reports whether a data point has any of the given string tags.
/// If no tags are provided, it returns true.
fn has_any(point: &OtlpNumberDataPoint, tags: &[Kv]) -> bool {
    if tags.is_empty() {
        return true;
    }

    for tag in tags {
        for attr in &point.attributes {
            if attr.key == tag.k {
                if let Some(any_value) = &attr.value {
                    if let Some(Value::StringValue(s_val)) = &any_value.value {
                        if s_val == tag.v {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

fn is_agent_internal_otel_metric(name: &str) -> bool {
    name.starts_with("datadog_trace_agent") || name.starts_with("datadog_otlp")
}

fn rename_agent_internal_otel_metric(metric: &mut OtlpMetric) {
    if is_agent_internal_otel_metric(metric.name.as_str()) {
        metric.name = format!("otelcol_{}", metric.name);
    }
}

static KAFKA_METRICS_TO_RENAME: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut m = HashSet::new();
    m.insert("kafka.producer.request-rate");
    m.insert("kafka.producer.response-rate");
    m.insert("kafka.producer.request-latency-avg");
    m.insert("kafka.consumer.fetch-size-avg");
    m.insert("kafka.producer.compression-rate");
    m.insert("kafka.producer.record-retry-rate");
    m.insert("kafka.producer.record-send-rate");
    m.insert("kafka.producer.record-error-rate");
    m
});

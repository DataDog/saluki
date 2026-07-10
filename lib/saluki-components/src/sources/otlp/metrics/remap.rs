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

/// Extracts any Datadog-specific metrics from a metric and appends them to the `new_metrics` vector.
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

fn copy_metric_with_unit(
    dest: &mut Vec<OtlpMetric>, m: &OtlpMetric, newname: &str, div: f64, attributes_mapping: AttributesMapping,
    filter: &[Kv], unit: &str,
) {
    if copy_metric_with_attr(dest, m, newname, div, attributes_mapping, filter) {
        if let Some(added) = dest.last_mut() {
            added.unit = unit.to_string();
        }
    }
}

fn remap_container_metrics(new_metrics: &mut Vec<OtlpMetric>, metric: &OtlpMetric) {
    let name = metric.name.as_str();
    if !name.starts_with("container.") {
        return;
    }
    match name {
        "container.cpu.usage.total" => {
            copy_metric_with_unit(
                new_metrics,
                metric,
                "container.cpu.usage",
                1.0,
                AttributesMapping::empty(),
                &[],
                "nanocore",
            );
        }
        "container.cpu.usage.usermode" => {
            copy_metric_with_unit(
                new_metrics,
                metric,
                "container.cpu.user",
                1.0,
                AttributesMapping::empty(),
                &[],
                "nanocore",
            );
        }
        "container.cpu.usage.system" => {
            copy_metric_with_unit(
                new_metrics,
                metric,
                "container.cpu.system",
                1.0,
                AttributesMapping::empty(),
                &[],
                "nanocore",
            );
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
            Some(OtlpNumberValue::AsInt(ref mut i)) if div >= 1.0 => {
                *i /= div as i64;
            }
            Some(OtlpNumberValue::AsDouble(ref mut d)) if div != 0.0 => {
                *d /= div;
            }
            _ => {}
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

#[cfg(test)]
mod tests {
    // These tests port the input->output mappings verified by the Go opentelemetry-mapping-go
    // remapping/renaming suite:
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/remapping_test.go
    use otlp_protos::opentelemetry::proto::common::v1::AnyValue;
    use otlp_protos::opentelemetry::proto::metrics::v1::{Gauge, Sum};

    use super::*;

    fn string_attr(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
        }
    }

    fn double_dp(value: f64, attrs: &[KeyValue]) -> OtlpNumberDataPoint {
        OtlpNumberDataPoint {
            value: Some(OtlpNumberValue::AsDouble(value)),
            attributes: attrs.to_vec(),
            ..Default::default()
        }
    }

    fn gauge_metric(name: &str, data_points: Vec<OtlpNumberDataPoint>) -> OtlpMetric {
        OtlpMetric {
            name: name.to_string(),
            data: Some(OtlpMetricData::Gauge(Gauge { data_points })),
            ..Default::default()
        }
    }

    fn sum_metric(name: &str, data_points: Vec<OtlpNumberDataPoint>) -> OtlpMetric {
        OtlpMetric {
            name: name.to_string(),
            data: Some(OtlpMetricData::Sum(Sum {
                data_points,
                is_monotonic: true,
                aggregation_temporality: 0,
            })),
            ..Default::default()
        }
    }

    fn data_points(metric: &OtlpMetric) -> &[OtlpNumberDataPoint] {
        match metric.data.as_ref().expect("metric data") {
            OtlpMetricData::Gauge(g) => &g.data_points,
            OtlpMetricData::Sum(s) => &s.data_points,
            _ => panic!("unexpected metric data type"),
        }
    }

    fn first_double_value(metric: &OtlpMetric) -> f64 {
        match data_points(metric)[0].value.as_ref().expect("data point value") {
            OtlpNumberValue::AsDouble(d) => *d,
            OtlpNumberValue::AsInt(i) => *i as f64,
        }
    }

    fn attr_value<'a>(dp: &'a OtlpNumberDataPoint, key: &str) -> Option<&'a str> {
        dp.attributes.iter().find(|kv| kv.key == key).and_then(|kv| {
            match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(Value::StringValue(s)) => Some(s.as_str()),
                _ => None,
            }
        })
    }

    fn remapped_by_name(metric: &OtlpMetric) -> HashMap<String, OtlpMetric> {
        let mut new_metrics = Vec::new();
        remap_metrics(&mut new_metrics, metric);
        new_metrics.into_iter().map(|m| (m.name.clone(), m)).collect()
    }

    #[test]
    fn remap_system_load_average_copies_value_unchanged() {
        let metric = gauge_metric("system.cpu.load_average.1m", vec![double_dp(0.75, &[])]);
        let remapped = remapped_by_name(&metric);

        assert_eq!(remapped.len(), 1);
        assert_eq!(first_double_value(&remapped["system.load.1"]), 0.75);
    }

    #[test]
    fn remap_cpu_utilization_splits_by_state_and_scales_to_percentage() {
        // `system.cpu.utilization` is reported as a fraction (0.0-1.0); the Datadog `system.cpu.*`
        // metrics are percentages, so each state is divided by DIV_PERCENTAGE (0.01), i.e. scaled x100,
        // and each output keeps only the data point whose `state` attribute matches.
        let metric = gauge_metric(
            "system.cpu.utilization",
            vec![
                double_dp(0.5, &[string_attr("state", "idle")]),
                double_dp(0.3, &[string_attr("state", "user")]),
                double_dp(0.1, &[string_attr("state", "system")]),
                double_dp(0.05, &[string_attr("state", "wait")]),
                double_dp(0.04, &[string_attr("state", "steal")]),
            ],
        );
        let remapped = remapped_by_name(&metric);

        assert_eq!(remapped.len(), 5);
        assert!((first_double_value(&remapped["system.cpu.idle"]) - 50.0).abs() < 1e-9);
        assert!((first_double_value(&remapped["system.cpu.user"]) - 30.0).abs() < 1e-9);
        assert!((first_double_value(&remapped["system.cpu.system"]) - 10.0).abs() < 1e-9);
        assert!((first_double_value(&remapped["system.cpu.iowait"]) - 5.0).abs() < 1e-9);
        assert!((first_double_value(&remapped["system.cpu.stolen"]) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn remap_container_cpu_usage_sets_nanocore_unit() {
        let metric = sum_metric("container.cpu.usage.total", vec![double_dp(123.0, &[])]);
        let remapped = remapped_by_name(&metric);

        assert_eq!(remapped.len(), 1);
        let usage = &remapped["container.cpu.usage"];
        assert_eq!(first_double_value(usage), 123.0);
        assert_eq!(usage.unit, "nanocore");
    }

    #[test]
    fn remap_kafka_consumer_lag_copies_group_into_consumer_group() {
        let metric = gauge_metric(
            "kafka.consumer.records_lag",
            vec![double_dp(5.0, &[string_attr("group", "my-group")])],
        );
        let remapped = remapped_by_name(&metric);

        assert_eq!(remapped.len(), 1);
        let lag = &remapped["kafka.consumer_lag"];
        let dp = &data_points(lag)[0];
        // The dynamic attribute mapping copies `group` to a new `consumer_group` attribute while
        // leaving the original in place.
        assert_eq!(attr_value(dp, "group"), Some("my-group"));
        assert_eq!(attr_value(dp, "consumer_group"), Some("my-group"));
    }

    #[test]
    fn remap_jvm_memory_committed_splits_heap_and_non_heap() {
        let metric = gauge_metric(
            "jvm.memory.committed",
            vec![
                double_dp(100.0, &[string_attr("pool", "heap")]),
                double_dp(50.0, &[string_attr("pool", "non-heap")]),
            ],
        );
        let remapped = remapped_by_name(&metric);

        assert_eq!(remapped.len(), 2);
        assert_eq!(first_double_value(&remapped["jvm.heap_memory_committed"]), 100.0);
        assert_eq!(first_double_value(&remapped["jvm.non_heap_memory_committed"]), 50.0);
    }

    #[test]
    fn remap_ignores_unmapped_metric() {
        let metric = gauge_metric("custom.app.metric", vec![double_dp(1.0, &[])]);
        let remapped = remapped_by_name(&metric);

        assert!(remapped.is_empty());
    }

    #[test]
    fn rename_host_metrics_add_otel_prefix() {
        for name in ["system.cpu.time", "process.cpu.time"] {
            let mut metric = gauge_metric(name, vec![]);
            rename_metric(&mut metric);
            assert_eq!(metric.name, format!("otel.{name}"));
        }
    }

    #[test]
    fn rename_listed_kafka_metric_adds_otel_prefix() {
        let mut metric = gauge_metric("kafka.producer.request-rate", vec![]);
        rename_metric(&mut metric);
        assert_eq!(metric.name, "otel.kafka.producer.request-rate");
    }

    #[test]
    fn rename_agent_internal_metrics_add_otelcol_prefix() {
        for name in ["datadog_trace_agent.receiver.spans", "datadog_otlp.something"] {
            let mut metric = gauge_metric(name, vec![]);
            rename_metric(&mut metric);
            assert_eq!(metric.name, format!("otelcol_{name}"));
        }
    }

    #[test]
    fn rename_leaves_unmapped_metric_unchanged() {
        let mut metric = gauge_metric("kafka.consumer.records_lag", vec![]);
        rename_metric(&mut metric);
        // Not a host metric, not in the kafka-rename set, and not agent-internal.
        assert_eq!(metric.name, "kafka.consumer.records_lag");
    }
}

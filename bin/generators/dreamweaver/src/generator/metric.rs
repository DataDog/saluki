//! Metric generator.

use otlp_protos::opentelemetry::proto::{
    common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue},
    metrics::v1::{
        metric, AggregationTemporality, Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics, Sum,
    },
    resource::v1::Resource,
};
use rand::Rng;

use crate::config::{MetricTemplate, MetricType};
use crate::model::trace_context::now_ns;

/// Generator for OTLP metrics.
pub struct MetricGenerator {
    service_name: String,
    templates: Vec<MetricTemplate>,
    /// Cumulative counter values (for counter metrics).
    counter_values: Vec<f64>,
}

impl MetricGenerator {
    /// Creates a new metric generator for the given service.
    pub fn new(service_name: String, templates: Vec<MetricTemplate>) -> Self {
        let counter_values = templates.iter().map(|_| 0.0).collect();

        Self {
            service_name,
            templates,
            counter_values,
        }
    }

    /// Generates metrics for this service.
    ///
    /// The `request_duration_ms` is used to generate realistic histogram values.
    pub fn generate_metrics(&mut self, rng: &mut impl Rng, request_duration_ms: f64) -> ResourceMetrics {
        let now = now_ns();

        // Clone templates to avoid borrow issues with counter_values mutation
        let templates: Vec<_> = self.templates.iter().cloned().enumerate().collect();
        let mut metrics = Vec::with_capacity(templates.len());

        for (idx, template) in templates {
            let metric = self.generate_metric_inner(rng, template, idx, now, request_duration_ms);
            metrics.push(metric);
        }

        ResourceMetrics {
            resource: Some(self.create_resource()),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(self.create_scope()),
                metrics,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    fn generate_metric_inner(
        &mut self, rng: &mut impl Rng, template: MetricTemplate, idx: usize, now: u64, request_duration_ms: f64,
    ) -> Metric {
        let data = match template.metric_type {
            MetricType::Counter => {
                // Increment counter
                self.counter_values[idx] += 1.0;
                metric::Data::Sum(Sum {
                    data_points: vec![NumberDataPoint {
                        attributes: vec![],
                        start_time_unix_nano: now - 60_000_000_000, // 60 seconds ago
                        time_unix_nano: now,
                        value: Some(
                            otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsDouble(
                                self.counter_values[idx],
                            ),
                        ),
                        exemplars: vec![],
                        flags: 0,
                    }],
                    aggregation_temporality: AggregationTemporality::Cumulative as i32,
                    is_monotonic: true,
                })
            }
            MetricType::Gauge => {
                // Generate a random gauge value
                let value = match template.name.as_str() {
                    name if name.contains("connections") => rng.random_range(1.0..100.0),
                    name if name.contains("active") => rng.random_range(0.0..50.0),
                    name if name.contains("memory") => rng.random_range(100.0..1000.0),
                    _ => rng.random_range(0.0..100.0),
                };

                metric::Data::Gauge(Gauge {
                    data_points: vec![NumberDataPoint {
                        attributes: vec![],
                        start_time_unix_nano: 0,
                        time_unix_nano: now,
                        value: Some(
                            otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsDouble(value),
                        ),
                        exemplars: vec![],
                        flags: 0,
                    }],
                })
            }
            MetricType::Histogram => {
                // Generate a histogram with the request duration
                let value = if template.name.contains("duration") {
                    request_duration_ms
                } else {
                    rng.random_range(1.0..1000.0)
                };

                // Standard histogram buckets (in ms for duration metrics)
                let bounds = vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];
                let mut counts = vec![0u64; bounds.len() + 1];

                // Place the value in the appropriate bucket
                let mut placed = false;
                for (i, &bound) in bounds.iter().enumerate() {
                    if value <= bound {
                        counts[i] = 1;
                        placed = true;
                        break;
                    }
                }
                if !placed {
                    counts[bounds.len()] = 1;
                }

                metric::Data::Histogram(Histogram {
                    data_points: vec![HistogramDataPoint {
                        attributes: vec![],
                        start_time_unix_nano: now - 60_000_000_000,
                        time_unix_nano: now,
                        count: 1,
                        sum: Some(value),
                        bucket_counts: counts,
                        explicit_bounds: bounds,
                        exemplars: vec![],
                        flags: 0,
                        min: Some(value),
                        max: Some(value),
                    }],
                    aggregation_temporality: AggregationTemporality::Delta as i32,
                })
            }
        };

        Metric {
            name: template.name.clone(),
            description: String::new(),
            unit: template.unit.clone().unwrap_or_default(),
            data: Some(data),
            metadata: vec![],
        }
    }

    fn create_resource(&self) -> Resource {
        Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_string(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::StringValue(self.service_name.clone())),
                }),
            }],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        }
    }

    fn create_scope(&self) -> InstrumentationScope {
        InstrumentationScope {
            name: "dreamweaver".to_string(),
            version: "1.0.0".to_string(),
            attributes: vec![],
            dropped_attributes_count: 0,
        }
    }
}

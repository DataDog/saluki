//! Debug logging of the OTLP data ADP receives.
//!
//! Operators turn this on to confirm that data is arriving and to see what it looks like. It's a view into what was
//! received and doesn't change what ADP processes or sends.
//!
//! Each signal has a `render_*` method that decides what to log for one received batch, and a `log_*` method that
//! writes it. Rendering is a pure function of the verbosity and the batch, so tests can check it without capturing logs.

use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs;
use otlp_protos::opentelemetry::proto::metrics::v1::{metric::Data as MetricData, Metric};
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use tracing::info;

/// How much detail is logged for each received batch of OTLP data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OtlpDebugVerbosity {
    /// Log nothing.
    #[default]
    None,

    /// Log one summary line per batch, with the number of items received.
    Basic,

    /// Log the summary line, then about one line for each item received.
    Normal,

    /// Log the summary line, then every field of every item received.
    Detailed,
}

/// Logs the OTLP data ADP receives, at the configured verbosity.
#[derive(Clone, Copy, Debug, Default)]
pub struct OtlpDebugOutput {
    verbosity: OtlpDebugVerbosity,
}

impl OtlpDebugOutput {
    /// Creates debug output that logs at `verbosity`.
    pub fn new(verbosity: OtlpDebugVerbosity) -> Self {
        Self { verbosity }
    }

    /// Logs a received metrics export request.
    pub fn log_metrics(&self, request: &ExportMetricsServiceRequest) {
        if let Some(message) = self.render_metrics(request) {
            info!("{message}");
        }
    }

    /// Logs a received batch of resource logs.
    pub fn log_logs(&self, resource_logs: &ResourceLogs) {
        if let Some(message) = self.render_logs(resource_logs) {
            info!("{message}");
        }
    }

    /// Logs a received batch of resource spans.
    pub fn log_traces(&self, resource_spans: &ResourceSpans) {
        if let Some(message) = self.render_traces(resource_spans) {
            info!("{message}");
        }
    }

    /// Returns what to log for a metrics export request, or `None` when nothing should be logged.
    fn render_metrics(&self, request: &ExportMetricsServiceRequest) -> Option<String> {
        if self.verbosity == OtlpDebugVerbosity::None {
            return None;
        }

        Some(summarize_metrics(request))
    }

    /// Returns what to log for a batch of resource logs, or `None` when nothing should be logged.
    fn render_logs(&self, resource_logs: &ResourceLogs) -> Option<String> {
        if self.verbosity == OtlpDebugVerbosity::None {
            return None;
        }

        Some(summarize_logs(resource_logs))
    }

    /// Returns what to log for a batch of resource spans, or `None` when nothing should be logged.
    fn render_traces(&self, resource_spans: &ResourceSpans) -> Option<String> {
        if self.verbosity == OtlpDebugVerbosity::None {
            return None;
        }

        Some(summarize_traces(resource_spans))
    }
}

/// Summarizes a metrics export request: how many resources, metrics and data points it carries.
fn summarize_metrics(request: &ExportMetricsServiceRequest) -> String {
    let metrics = request
        .resource_metrics
        .iter()
        .flat_map(|resource| &resource.scope_metrics)
        .flat_map(|scope| &scope.metrics);
    let (metric_count, data_point_count) = metrics.fold((0, 0), |(metrics, points), metric| {
        (metrics + 1, points + count_data_points(metric))
    });

    format!(
        "Metrics: resource metrics: {}, metrics: {metric_count}, data points: {data_point_count}",
        request.resource_metrics.len()
    )
}

/// Summarizes a batch of resource logs: how many log records it carries.
fn summarize_logs(resource_logs: &ResourceLogs) -> String {
    let log_record_count: usize = resource_logs
        .scope_logs
        .iter()
        .map(|scope| scope.log_records.len())
        .sum();

    format!("Logs: resource logs: 1, log records: {log_record_count}")
}

/// Summarizes a batch of resource spans: how many spans it carries.
fn summarize_traces(resource_spans: &ResourceSpans) -> String {
    let span_count: usize = resource_spans.scope_spans.iter().map(|scope| scope.spans.len()).sum();

    format!("Traces: resource spans: 1, spans: {span_count}")
}

/// Counts the data points of a metric, whatever its type.
fn count_data_points(metric: &Metric) -> usize {
    match &metric.data {
        Some(MetricData::Gauge(gauge)) => gauge.data_points.len(),
        Some(MetricData::Sum(sum)) => sum.data_points.len(),
        Some(MetricData::Histogram(histogram)) => histogram.data_points.len(),
        Some(MetricData::ExponentialHistogram(histogram)) => histogram.data_points.len(),
        Some(MetricData::Summary(summary)) => summary.data_points.len(),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ScopeLogs};
    use otlp_protos::opentelemetry::proto::metrics::v1::{Gauge, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum};
    use otlp_protos::opentelemetry::proto::trace::v1::{ScopeSpans, Span};

    use super::*;

    fn output(verbosity: OtlpDebugVerbosity) -> OtlpDebugOutput {
        OtlpDebugOutput::new(verbosity)
    }

    fn resource_spans(spans_per_scope: &[usize]) -> ResourceSpans {
        ResourceSpans {
            scope_spans: spans_per_scope
                .iter()
                .map(|&count| ScopeSpans {
                    spans: vec![Span::default(); count],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn resource_logs(records_per_scope: &[usize]) -> ResourceLogs {
        ResourceLogs {
            scope_logs: records_per_scope
                .iter()
                .map(|&count| ScopeLogs {
                    log_records: vec![LogRecord::default(); count],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn metrics_request() -> ExportMetricsServiceRequest {
        let gauge = Metric {
            data: Some(MetricData::Gauge(Gauge {
                data_points: vec![NumberDataPoint::default(); 2],
            })),
            ..Default::default()
        };
        let sum = Metric {
            data: Some(MetricData::Sum(Sum {
                data_points: vec![NumberDataPoint::default(); 3],
                ..Default::default()
            })),
            ..Default::default()
        };
        let empty = Metric::default();

        ExportMetricsServiceRequest {
            resource_metrics: vec![
                ResourceMetrics {
                    scope_metrics: vec![ScopeMetrics {
                        metrics: vec![gauge, sum],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ResourceMetrics {
                    scope_metrics: vec![ScopeMetrics {
                        metrics: vec![empty],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn nothing_is_rendered_when_the_verbosity_is_none() {
        let output = output(OtlpDebugVerbosity::None);

        assert_eq!(output.render_metrics(&metrics_request()), None);
        assert_eq!(output.render_logs(&resource_logs(&[1])), None);
        assert_eq!(output.render_traces(&resource_spans(&[1])), None);
    }

    #[test]
    fn traces_summary_counts_the_spans_of_every_scope() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_traces(&resource_spans(&[2, 3]));

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 1, spans: 5"));
    }

    #[test]
    fn logs_summary_counts_the_log_records_of_every_scope() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_logs(&resource_logs(&[1, 4]));

        assert_eq!(rendered.as_deref(), Some("Logs: resource logs: 1, log records: 5"));
    }

    #[test]
    fn metrics_summary_counts_resources_metrics_and_data_points() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_metrics(&metrics_request());

        assert_eq!(
            rendered.as_deref(),
            Some("Metrics: resource metrics: 2, metrics: 3, data points: 5")
        );
    }

    #[test]
    fn an_empty_batch_still_renders_a_summary() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_traces(&ResourceSpans::default());

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 1, spans: 0"));
    }
}

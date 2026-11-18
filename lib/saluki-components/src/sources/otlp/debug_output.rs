//! Debug logging of the OTLP data ADP receives.
//!
//! Operators turn this on to confirm that data is arriving and to see what it looks like. It's a view into what was
//! received and doesn't change what ADP processes or sends.
//!
//! Each signal has a `render_*` method that decides what to log for one received batch, and a `log_*` method that
//! writes it. Rendering is a pure function of the verbosity and the batch, so tests can check it without capturing logs.

use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value as AnyValueKind, AnyValue, KeyValue};
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::metrics::v1::{metric::Data as MetricData, Metric};
use otlp_protos::opentelemetry::proto::trace::v1::Span;
use tracing::info;

use crate::common::otlp::traces::transform::{bytes_to_hex_lowercase, otlp_value_to_string};

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

    /// Logs a received logs export request.
    pub fn log_logs(&self, request: &ExportLogsServiceRequest) {
        if let Some(message) = self.render_logs(request) {
            info!("{message}");
        }
    }

    /// Logs a received traces export request.
    pub fn log_traces(&self, request: &ExportTraceServiceRequest) {
        if let Some(message) = self.render_traces(request) {
            info!("{message}");
        }
    }

    /// Returns what to log for a metrics export request, or `None` when nothing should be logged.
    fn render_metrics(&self, request: &ExportMetricsServiceRequest) -> Option<String> {
        self.render_batch(
            summarize_metrics(request),
            || normal_metrics(request),
            || detailed(request),
        )
    }

    /// Returns what to log for a logs export request, or `None` when nothing should be logged.
    fn render_logs(&self, request: &ExportLogsServiceRequest) -> Option<String> {
        self.render_batch(summarize_logs(request), || normal_logs(request), || detailed(request))
    }

    /// Returns what to log for a traces export request, or `None` when nothing should be logged.
    fn render_traces(&self, request: &ExportTraceServiceRequest) -> Option<String> {
        self.render_batch(
            summarize_traces(request),
            || normal_traces(request),
            || detailed(request),
        )
    }

    /// Combines a batch's summary with the body its verbosity calls for.
    ///
    /// The body closures only run at the verbosity that needs them, so a lower verbosity never pays to format records
    /// it won't log.
    fn render_batch(
        &self, summary: String, normal_body: impl FnOnce() -> String, detailed_body: impl FnOnce() -> String,
    ) -> Option<String> {
        let body = match self.verbosity {
            OtlpDebugVerbosity::None => return None,
            OtlpDebugVerbosity::Basic => return Some(summary),
            OtlpDebugVerbosity::Normal => normal_body(),
            OtlpDebugVerbosity::Detailed => detailed_body(),
        };

        if body.is_empty() {
            return Some(summary);
        }
        Some(format!("{summary}\n{body}"))
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

/// Summarizes a logs export request: how many resources and log records it carries.
fn summarize_logs(request: &ExportLogsServiceRequest) -> String {
    let log_record_count: usize = request
        .resource_logs
        .iter()
        .flat_map(|resource| &resource.scope_logs)
        .map(|scope| scope.log_records.len())
        .sum();

    format!(
        "Logs: resource logs: {}, log records: {log_record_count}",
        request.resource_logs.len()
    )
}

/// Summarizes a traces export request: how many resources and spans it carries.
fn summarize_traces(request: &ExportTraceServiceRequest) -> String {
    let span_count: usize = request
        .resource_spans
        .iter()
        .flat_map(|resource| &resource.scope_spans)
        .map(|scope| scope.spans.len())
        .sum();

    format!(
        "Traces: resource spans: {}, spans: {span_count}",
        request.resource_spans.len()
    )
}

/// Renders one line per metric: its name, type, unit and number of data points.
fn normal_metrics(request: &ExportMetricsServiceRequest) -> String {
    request
        .resource_metrics
        .iter()
        .flat_map(|resource| &resource.scope_metrics)
        .flat_map(|scope| &scope.metrics)
        .map(normal_metric_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normal_metric_line(metric: &Metric) -> String {
    let unit = if metric.unit.is_empty() {
        String::new()
    } else {
        format!(", unit: {}", metric.unit)
    };

    format!(
        "{} ({}{unit}) data points: {}",
        metric.name,
        metric_kind(metric),
        count_data_points(metric)
    )
}

/// Renders one line per log record: its severity, body and attributes.
fn normal_logs(request: &ExportLogsServiceRequest) -> String {
    request
        .resource_logs
        .iter()
        .flat_map(|resource| &resource.scope_logs)
        .flat_map(|scope| &scope.log_records)
        .map(normal_log_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normal_log_line(record: &LogRecord) -> String {
    let severity = if record.severity_text.is_empty() {
        "UNSPECIFIED"
    } else {
        &record.severity_text
    };
    let body = record.body.as_ref().map(format_value).unwrap_or_default();

    join_fields([severity.to_string(), body, format_attributes(&record.attributes)])
}

/// Renders one line per span: its name, trace and span IDs, and attributes.
fn normal_traces(request: &ExportTraceServiceRequest) -> String {
    request
        .resource_spans
        .iter()
        .flat_map(|resource| &resource.scope_spans)
        .flat_map(|scope| &scope.spans)
        .map(normal_span_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normal_span_line(span: &Span) -> String {
    join_fields([
        span.name.clone(),
        bytes_to_hex_lowercase(&span.trace_id),
        bytes_to_hex_lowercase(&span.span_id),
        format_attributes(&span.attributes),
    ])
}

/// Renders every field of a received message, through its `Debug` implementation.
///
/// Byte fields such as trace and span IDs print as lists of numbers rather than hex.
fn detailed(message: &impl std::fmt::Debug) -> String {
    format!("{message:#?}")
}

/// Joins the non-empty fields of a line with single spaces.
fn join_fields(fields: impl IntoIterator<Item = String>) -> String {
    fields
        .into_iter()
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders attributes as `key=value` pairs separated by spaces.
fn format_attributes(attributes: &[KeyValue]) -> String {
    attributes
        .iter()
        .map(|attribute| {
            let value = attribute.value.as_ref().map(format_value).unwrap_or_default();
            format!("{}={value}", attribute.key)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders an attribute or body value as text. An absent value renders as nothing.
fn format_value(value: &AnyValue) -> String {
    value
        .value
        .as_ref()
        .and_then(|kind: &AnyValueKind| otlp_value_to_string(kind))
        .unwrap_or_default()
}

/// Names the type of a metric.
fn metric_kind(metric: &Metric) -> &'static str {
    match &metric.data {
        Some(MetricData::Gauge(_)) => "gauge",
        Some(MetricData::Sum(_)) => "sum",
        Some(MetricData::Histogram(_)) => "histogram",
        Some(MetricData::ExponentialHistogram(_)) => "exponential histogram",
        Some(MetricData::Summary(_)) => "summary",
        None => "empty",
    }
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
    use otlp_protos::opentelemetry::proto::logs::v1::{ResourceLogs, ScopeLogs};
    use otlp_protos::opentelemetry::proto::metrics::v1::{Gauge, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum};
    use otlp_protos::opentelemetry::proto::trace::v1::{ResourceSpans, ScopeSpans};

    use super::*;

    fn output(verbosity: OtlpDebugVerbosity) -> OtlpDebugOutput {
        OtlpDebugOutput::new(verbosity)
    }

    /// Builds a traces request with one resource for each entry, carrying that many spans.
    fn traces_request(spans_per_resource: &[usize]) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: spans_per_resource
                .iter()
                .map(|&count| ResourceSpans {
                    scope_spans: vec![ScopeSpans {
                        spans: vec![Span::default(); count],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
        }
    }

    /// Builds a logs request with one resource for each entry, carrying that many log records.
    fn logs_request(records_per_resource: &[usize]) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: records_per_resource
                .iter()
                .map(|&count| ResourceLogs {
                    scope_logs: vec![ScopeLogs {
                        log_records: vec![LogRecord::default(); count],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn metrics_request() -> ExportMetricsServiceRequest {
        let gauge = Metric {
            name: "cpu.usage".to_string(),
            unit: "1".to_string(),
            data: Some(MetricData::Gauge(Gauge {
                data_points: vec![NumberDataPoint::default(); 2],
            })),
            ..Default::default()
        };
        let sum = Metric {
            name: "requests".to_string(),
            data: Some(MetricData::Sum(Sum {
                data_points: vec![NumberDataPoint::default(); 3],
                ..Default::default()
            })),
            ..Default::default()
        };
        let empty = Metric {
            name: "pending".to_string(),
            ..Default::default()
        };

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
        assert_eq!(output.render_logs(&logs_request(&[1])), None);
        assert_eq!(output.render_traces(&traces_request(&[1])), None);
    }

    #[test]
    fn traces_summary_counts_the_resources_and_spans_of_the_request() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_traces(&traces_request(&[2, 3]));

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 2, spans: 5"));
    }

    #[test]
    fn logs_summary_counts_the_resources_and_log_records_of_the_request() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_logs(&logs_request(&[1, 4]));

        assert_eq!(rendered.as_deref(), Some("Logs: resource logs: 2, log records: 5"));
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
    fn an_empty_request_still_renders_a_summary() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_traces(&ExportTraceServiceRequest::default());

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 0, spans: 0"));
    }

    fn string_attribute(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AnyValueKind::StringValue(value.to_string())),
            }),
        }
    }

    #[test]
    fn normal_traces_log_one_line_for_each_span_after_the_summary() {
        let span = |name: &str, span_id: u8| Span {
            name: name.to_string(),
            trace_id: vec![0xab; 16],
            span_id: vec![span_id; 8],
            attributes: vec![string_attribute("peer.service", "db")],
            ..Default::default()
        };
        let request = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![span("query", 1), span("fetch", 2)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let rendered = output(OtlpDebugVerbosity::Normal).render_traces(&request);

        assert_eq!(
            rendered.as_deref(),
            Some(
                "Traces: resource spans: 1, spans: 2\n\
                 query abababababababababababababababab 0101010101010101 peer.service=db\n\
                 fetch abababababababababababababababab 0202020202020202 peer.service=db"
            )
        );
    }

    #[test]
    fn normal_logs_log_one_line_for_each_record_after_the_summary() {
        let record = LogRecord {
            severity_text: "ERROR".to_string(),
            body: Some(AnyValue {
                value: Some(AnyValueKind::StringValue("disk full".to_string())),
            }),
            attributes: vec![string_attribute("host", "a")],
            ..Default::default()
        };
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record, LogRecord::default()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let rendered = output(OtlpDebugVerbosity::Normal).render_logs(&request);

        assert_eq!(
            rendered.as_deref(),
            Some("Logs: resource logs: 1, log records: 2\nERROR disk full host=a\nUNSPECIFIED")
        );
    }

    #[test]
    fn normal_metrics_log_one_line_for_each_metric_after_the_summary() {
        let rendered = output(OtlpDebugVerbosity::Normal).render_metrics(&metrics_request());

        assert_eq!(
            rendered.as_deref(),
            Some(
                "Metrics: resource metrics: 2, metrics: 3, data points: 5\n\
                 cpu.usage (gauge, unit: 1) data points: 2\n\
                 requests (sum) data points: 3\n\
                 pending (empty) data points: 0"
            )
        );
    }

    #[test]
    fn normal_omits_the_body_for_a_request_with_no_records() {
        let rendered = output(OtlpDebugVerbosity::Normal).render_traces(&ExportTraceServiceRequest::default());

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 0, spans: 0"));
    }

    #[test]
    fn basic_does_not_log_individual_records() {
        let rendered = output(OtlpDebugVerbosity::Basic).render_traces(&traces_request(&[2]));

        assert_eq!(rendered.as_deref(), Some("Traces: resource spans: 1, spans: 2"));
    }

    #[test]
    fn detailed_traces_log_every_field_of_the_batch_after_the_summary() {
        let request = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        name: "query".to_string(),
                        attributes: vec![string_attribute("peer.service", "db")],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let rendered = output(OtlpDebugVerbosity::Detailed)
            .render_traces(&request)
            .expect("detailed verbosity logs every batch");

        assert!(rendered.starts_with("Traces: resource spans: 1, spans: 1\n"));
        assert!(rendered.contains("name: \"query\""));
        assert!(rendered.contains("peer.service"));
        assert!(rendered.contains("trace_id"));
    }

    #[test]
    fn detailed_logs_and_metrics_log_every_field_after_the_summary() {
        let logs = output(OtlpDebugVerbosity::Detailed)
            .render_logs(&logs_request(&[1]))
            .expect("detailed verbosity logs every batch");
        let metrics = output(OtlpDebugVerbosity::Detailed)
            .render_metrics(&metrics_request())
            .expect("detailed verbosity logs every batch");

        assert!(logs.starts_with("Logs: resource logs: 1, log records: 1\n"));
        assert!(logs.contains("severity_number"));
        assert!(metrics.starts_with("Metrics: resource metrics: 2, metrics: 3, data points: 5\n"));
        assert!(metrics.contains("name: \"cpu.usage\""));
    }

    #[test]
    fn a_verbosity_formats_only_the_body_it_logs() {
        let formats_nothing = || -> String { panic!("this body must not be formatted") };

        for verbosity in [OtlpDebugVerbosity::None, OtlpDebugVerbosity::Basic] {
            output(verbosity).render_batch("summary".to_string(), formats_nothing, formats_nothing);
        }
        output(OtlpDebugVerbosity::Normal).render_batch("summary".to_string(), String::new, formats_nothing);
        output(OtlpDebugVerbosity::Detailed).render_batch("summary".to_string(), formats_nothing, String::new);
    }
}

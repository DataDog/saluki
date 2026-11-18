//! Debug logging of the OTLP data the source receives.
//!
//! Operators turn this on to confirm that data is arriving and to see what it looks like. It's a view into what was
//! received and doesn't change what the source processes or sends.
//!
//! Each signal has a `render_*` method that decides what to log for one export request, and a `log_*` method that
//! writes it. Rendering is a pure function of the verbosity and the request, so tests can check it without capturing
//! logs.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value as AnyValueKind, AnyValue, KeyValue};
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::metrics::v1::{metric::Data as MetricData, Metric};
use otlp_protos::opentelemetry::proto::trace::v1::Span;
use tracing::{enabled, info, Level};

use super::logs::transform::derive_status;
use crate::common::otlp::traces::transform::{bytes_to_hex_lowercase, otlp_value_to_string};

/// How much detail is logged for each received OTLP export request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OtlpDebugVerbosity {
    /// Log nothing.
    #[default]
    None,

    /// Log one summary line per request, with the number of items received.
    Basic,

    /// Log the summary line, then about one line for each item received.
    Normal,

    /// Log the summary line, then every field of every item received.
    Detailed,
}

/// Logs the OTLP data the source receives, at the configured verbosity.
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
        if !self.is_logging() {
            return;
        }

        if let Some(message) = self.render_metrics(request) {
            info!("{message}");
        }
    }

    /// Logs a received logs export request.
    pub fn log_logs(&self, request: &ExportLogsServiceRequest) {
        if !self.is_logging() {
            return;
        }

        if let Some(message) = self.render_logs(request) {
            info!("{message}");
        }
    }

    /// Logs a received traces export request.
    pub fn log_traces(&self, request: &ExportTraceServiceRequest) {
        if !self.is_logging() {
            return;
        }

        if let Some(message) = self.render_traces(request) {
            info!("{message}");
        }
    }

    /// Returns whether anything would be logged right now.
    ///
    /// Checked before a request is rendered, so a deployment that leaves the output off, or whose log level hides it,
    /// doesn't pay to render requests that nobody will see.
    fn is_logging(&self) -> bool {
        self.verbosity != OtlpDebugVerbosity::None && enabled!(Level::INFO)
    }

    /// Returns what to log for a metrics export request, or `None` when nothing should be logged.
    fn render_metrics(&self, request: &ExportMetricsServiceRequest) -> Option<String> {
        self.render_request(
            || summarize_metrics(request),
            || normal_metrics(request),
            || detailed(request),
        )
    }

    /// Returns what to log for a logs export request, or `None` when nothing should be logged.
    fn render_logs(&self, request: &ExportLogsServiceRequest) -> Option<String> {
        self.render_request(
            || summarize_logs(request),
            || normal_logs(request),
            || detailed(request),
        )
    }

    /// Returns what to log for a traces export request, or `None` when nothing should be logged.
    fn render_traces(&self, request: &ExportTraceServiceRequest) -> Option<String> {
        self.render_request(
            || summarize_traces(request),
            || normal_traces(request),
            || detailed(request),
        )
    }

    /// Combines a request's summary with the body its verbosity calls for.
    ///
    /// Each part is a closure that only runs at the verbosity that needs it, so a lower verbosity never pays to
    /// summarize or format what it won't log.
    fn render_request(
        &self, summary: impl FnOnce() -> String, normal_body: impl FnOnce() -> String,
        detailed_body: impl FnOnce() -> String,
    ) -> Option<String> {
        match self.verbosity {
            OtlpDebugVerbosity::None => None,
            OtlpDebugVerbosity::Basic => Some(summary()),
            OtlpDebugVerbosity::Normal => Some(with_body(summary(), normal_body())),
            OtlpDebugVerbosity::Detailed => Some(with_body(summary(), detailed_body())),
        }
    }
}

/// Appends a body to a summary on the lines below it. An empty body leaves the summary alone.
fn with_body(mut summary: String, body: String) -> String {
    if !body.is_empty() {
        summary.push('\n');
        summary.push_str(&body);
    }
    summary
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
        format!(", unit: {}", escape_control_chars(&metric.unit))
    };

    format!(
        "{} ({}{unit}) data points: {}",
        escape_control_chars(&metric.name),
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
    // Falls back from the severity text to the name of the severity number's range, as the logs pipeline does.
    let status = derive_status(None, &record.severity_text, record.severity_number);
    let severity = if status.is_empty() { "UNSPECIFIED" } else { &*status };
    let body = record.body.as_ref().map(format_value).unwrap_or_default();

    join_fields([
        escape_control_chars(severity).into_owned(),
        body,
        format_attributes(&record.attributes),
    ])
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
        escape_control_chars(&span.name).into_owned(),
        bytes_to_hex_lowercase(&span.trace_id),
        bytes_to_hex_lowercase(&span.span_id),
        format_attributes(&span.attributes),
    ])
}

/// The most bytes `detailed` output writes for one request.
///
/// Pretty-printing puts every byte of a byte field on a line of its own, so a request that fits the receiver's size
/// limit can still dump to many times that size. Output past this limit is cut off and marked.
const MAX_DETAILED_BYTES: usize = 1024 * 1024;

/// Ends a `detailed` dump that was cut off at [`MAX_DETAILED_BYTES`].
const TRUNCATION_MARKER: &str = "\n... (output truncated)";

/// Renders every field of a received message, through its `Debug` implementation.
///
/// Byte fields such as trace and span IDs print as lists of numbers rather than hex. The dump is cut off at
/// [`MAX_DETAILED_BYTES`].
fn detailed(message: &impl fmt::Debug) -> String {
    detailed_with_limit(message, MAX_DETAILED_BYTES)
}

fn detailed_with_limit(message: &impl fmt::Debug, limit: usize) -> String {
    let mut dump = BoundedString::new(limit);
    if write!(dump, "{message:#?}").is_err() {
        dump.text.push_str(TRUNCATION_MARKER);
    }
    dump.text
}

/// A string that stops accepting text once it reaches a size limit.
///
/// Writing past the limit fails, which also stops the formatting that was producing the text, so a dump that's too
/// large costs no more than the limit to produce.
struct BoundedString {
    text: String,
    limit: usize,
}

impl BoundedString {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
        }
    }
}

impl fmt::Write for BoundedString {
    fn write_str(&mut self, chunk: &str) -> fmt::Result {
        let room = self.limit.saturating_sub(self.text.len());
        if chunk.len() <= room {
            self.text.push_str(chunk);
            return Ok(());
        }

        // Keep as much of the chunk as fits, without splitting a character.
        let mut end = room;
        while !chunk.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&chunk[..end]);
        Err(fmt::Error)
    }
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
            format!("{}={value}", escape_control_chars(&attribute.key))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders an attribute or body value as text on a single line. An absent value renders as nothing.
fn format_value(value: &AnyValue) -> String {
    let text = value
        .value
        .as_ref()
        .and_then(|kind: &AnyValueKind| otlp_value_to_string(kind))
        .unwrap_or_default();

    match escape_control_chars(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(escaped) => escaped,
    }
}

/// Escapes control characters, so a line of `normal` output can't be split into several by the data it describes.
///
/// A sender controls the names and values that end up in a line. A newline in one would otherwise start what looks
/// like a separate log entry. Text with nothing to escape is returned as it is, without copying it.
fn escape_control_chars(text: &str) -> Cow<'_, str> {
    // U+2028 and U+2029 aren't control characters, but some log viewers break lines on them.
    let needs_escape = |c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}');

    if !text.chars().any(needs_escape) {
        return Cow::Borrowed(text);
    }

    let mut escaped = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if needs_escape(c) {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    Cow::Owned(escaped)
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
    fn detailed_traces_log_every_field_of_the_request_after_the_summary() {
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
            .expect("detailed verbosity logs every request");

        assert!(rendered.starts_with("Traces: resource spans: 1, spans: 1\n"));
        assert!(rendered.contains("name: \"query\""));
        assert!(rendered.contains("peer.service"));
        assert!(rendered.contains("trace_id"));
    }

    #[test]
    fn detailed_logs_and_metrics_log_every_field_after_the_summary() {
        let logs = output(OtlpDebugVerbosity::Detailed)
            .render_logs(&logs_request(&[1]))
            .expect("detailed verbosity logs every request");
        let metrics = output(OtlpDebugVerbosity::Detailed)
            .render_metrics(&metrics_request())
            .expect("detailed verbosity logs every request");

        assert!(logs.starts_with("Logs: resource logs: 1, log records: 1\n"));
        assert!(logs.contains("severity_number"));
        assert!(metrics.starts_with("Metrics: resource metrics: 2, metrics: 3, data points: 5\n"));
        assert!(metrics.contains("name: \"cpu.usage\""));
    }

    #[test]
    fn a_verbosity_renders_only_the_parts_it_logs() {
        let renders_nothing = || -> String { panic!("this part must not be rendered") };

        output(OtlpDebugVerbosity::None).render_request(renders_nothing, renders_nothing, renders_nothing);
        output(OtlpDebugVerbosity::Basic).render_request(String::new, renders_nothing, renders_nothing);
        output(OtlpDebugVerbosity::Normal).render_request(String::new, String::new, renders_nothing);
        output(OtlpDebugVerbosity::Detailed).render_request(String::new, renders_nothing, String::new);
    }

    #[test]
    fn normal_keeps_each_item_on_one_line_when_the_data_has_control_characters() {
        let record = LogRecord {
            severity_text: "ERROR".to_string(),
            body: Some(AnyValue {
                value: Some(AnyValueKind::StringValue("first\nsecond\r\u{2028}third".to_string())),
            }),
            attributes: vec![string_attribute("k\ney", "v\talue")],
            ..Default::default()
        };
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let rendered = output(OtlpDebugVerbosity::Normal)
            .render_logs(&request)
            .expect("normal verbosity logs every request");

        assert_eq!(
            rendered,
            "Logs: resource logs: 1, log records: 1\n\
             ERROR first\\nsecond\\r\\u{2028}third k\\ney=v\\talue"
        );
    }

    #[test]
    fn normal_escapes_control_characters_in_span_and_metric_names() {
        let traces = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        name: "forged\nentry".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let metrics = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "forged\nmetric".to_string(),
                        unit: "u\nit".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let traces = output(OtlpDebugVerbosity::Normal).render_traces(&traces).unwrap();
        let metrics = output(OtlpDebugVerbosity::Normal).render_metrics(&metrics).unwrap();

        assert_eq!(traces.lines().count(), 2);
        assert!(traces.contains("forged\\nentry"));
        assert_eq!(metrics.lines().count(), 2);
        assert!(metrics.contains("forged\\nmetric (empty, unit: u\\nit)"));
    }

    #[test]
    fn text_without_control_characters_is_not_copied_to_escape_it() {
        assert!(matches!(
            escape_control_chars("plain text, ünïcode ok"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(escape_control_chars("line\nbreak"), Cow::Owned(_)));
    }

    #[test]
    fn normal_logs_name_the_severity_range_when_there_is_no_severity_text() {
        let record = |severity_number: i32| LogRecord {
            severity_number,
            ..Default::default()
        };
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record(9), record(17), record(21), record(0)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let rendered = output(OtlpDebugVerbosity::Normal).render_logs(&request).unwrap();

        assert_eq!(
            rendered,
            "Logs: resource logs: 1, log records: 4\ninfo\nerror\nfatal\nUNSPECIFIED"
        );
    }

    #[test]
    fn a_detailed_dump_within_the_limit_is_written_whole() {
        let message = vec![1u8, 2, 3];

        assert_eq!(detailed_with_limit(&message, 1024), format!("{message:#?}"));
    }

    #[test]
    fn a_detailed_dump_past_the_limit_is_cut_off_and_marked() {
        // Pretty-printing writes a line for every byte, so this dumps to far more than the limit.
        let message = vec![0u8; 10_000];

        let dump = detailed_with_limit(&message, 100);

        assert!(dump.ends_with(TRUNCATION_MARKER));
        assert_eq!(dump.len(), 100 + TRUNCATION_MARKER.len());
    }

    #[test]
    fn a_detailed_dump_is_cut_off_on_a_character_boundary() {
        // The dump is an opening quote, then two-byte characters, and the limit falls in the middle of the fifth.
        let message = "é".repeat(100);

        let dump = detailed_with_limit(&message, 10);

        assert!(dump.ends_with(TRUNCATION_MARKER));
        assert_eq!(dump.len(), 9 + TRUNCATION_MARKER.len());
    }
}

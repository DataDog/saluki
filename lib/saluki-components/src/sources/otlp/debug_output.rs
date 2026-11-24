//! Debug logging of the OTLP data the source receives.
//!
//! Operators turn this on to confirm that data is arriving and to see what it looks like. It's a view into what was
//! received and doesn't change what the source processes or sends.
//!
//! Each signal has a `render_*` method that decides what to log for one export request, and a `log_*` method that
//! writes it. Rendering is a pure function of the verbosity and the request, so tests can check it without capturing
//! logs.
//!
//! The text of a request is written straight into one buffer, field by field. Nothing builds a line, or joins lines,
//! before it reaches that buffer, so a request costs its output once and not once more per line and per join.

use std::borrow::Cow;
use std::fmt::{self, Display, Write as _};

use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value as AnyValueKind, AnyValue, KeyValue};
use otlp_protos::opentelemetry::proto::metrics::v1::{metric::Data as MetricData, Metric};
use tracing::{enabled, info, Level};

use super::logs::transform::derive_status;
use crate::common::otlp::traces::transform::bytes_to_hex_lowercase;
use crate::common::otlp::value::otlp_value_to_string;

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
            |output| write_normal_metrics(output, request),
            |output| write_detailed(output, request),
        )
    }

    /// Returns what to log for a logs export request, or `None` when nothing should be logged.
    fn render_logs(&self, request: &ExportLogsServiceRequest) -> Option<String> {
        self.render_request(
            || summarize_logs(request),
            |output| write_normal_logs(output, request),
            |output| write_detailed(output, request),
        )
    }

    /// Returns what to log for a traces export request, or `None` when nothing should be logged.
    fn render_traces(&self, request: &ExportTraceServiceRequest) -> Option<String> {
        self.render_request(
            || summarize_traces(request),
            |output| write_normal_traces(output, request),
            |output| write_detailed(output, request),
        )
    }

    /// Writes a request's summary, then the body its verbosity calls for, into one buffer.
    ///
    /// Each part is a closure that only runs at the verbosity that needs it, so a lower verbosity never pays to
    /// summarize or format what it won't log.
    fn render_request(
        &self, summary: impl FnOnce() -> String, normal_body: impl FnOnce(&mut String) -> fmt::Result,
        detailed_body: impl FnOnce(&mut String) -> fmt::Result,
    ) -> Option<String> {
        let mut output = match self.verbosity {
            OtlpDebugVerbosity::None => return None,
            OtlpDebugVerbosity::Basic | OtlpDebugVerbosity::Normal | OtlpDebugVerbosity::Detailed => summary(),
        };

        // Writing to a `String` can't fail, so the results carry nothing.
        let _ = match self.verbosity {
            OtlpDebugVerbosity::Normal => normal_body(&mut output),
            OtlpDebugVerbosity::Detailed => detailed_body(&mut output),
            OtlpDebugVerbosity::None | OtlpDebugVerbosity::Basic => Ok(()),
        };

        Some(output)
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

/// Writes one line per metric: its name, type, unit and number of data points.
fn write_normal_metrics(output: &mut String, request: &ExportMetricsServiceRequest) -> fmt::Result {
    let metrics = request
        .resource_metrics
        .iter()
        .flat_map(|resource| &resource.scope_metrics)
        .flat_map(|scope| &scope.metrics);

    for metric in metrics {
        write!(output, "\n{} ({}", Escaped(&metric.name), metric_kind(metric))?;
        if !metric.unit.is_empty() {
            write!(output, ", unit: {}", Escaped(&metric.unit))?;
        }
        write!(output, ") data points: {}", count_data_points(metric))?;
    }
    Ok(())
}

/// Writes one line per log record: its severity, body and attributes.
fn write_normal_logs(output: &mut String, request: &ExportLogsServiceRequest) -> fmt::Result {
    let records = request
        .resource_logs
        .iter()
        .flat_map(|resource| &resource.scope_logs)
        .flat_map(|scope| &scope.log_records);

    for record in records {
        // Falls back from the severity text to the name of the severity number's range, as the logs pipeline does.
        let status = derive_status(None, &record.severity_text, record.severity_number);
        let severity = if status.is_empty() { "UNSPECIFIED" } else { &*status };

        let mut line = ItemLine::start(output)?;
        line.text(severity)?;
        line.text(&value_text(record.body.as_ref()))?;
        line.attributes(&record.attributes)?;
    }
    Ok(())
}

/// Writes one line per span: its name, trace and span IDs, and attributes.
fn write_normal_traces(output: &mut String, request: &ExportTraceServiceRequest) -> fmt::Result {
    let spans = request
        .resource_spans
        .iter()
        .flat_map(|resource| &resource.scope_spans)
        .flat_map(|scope| &scope.spans);

    for span in spans {
        let mut line = ItemLine::start(output)?;
        line.text(&span.name)?;
        line.hex(&span.trace_id)?;
        line.hex(&span.span_id)?;
        line.attributes(&span.attributes)?;
    }
    Ok(())
}

/// Writes every field of a received request, through its `Debug` implementation.
///
/// Byte fields such as trace and span IDs print as lists of numbers rather than hex.
fn write_detailed(output: &mut String, request: &impl fmt::Debug) -> fmt::Result {
    write!(output, "\n{request:#?}")
}

/// One line of `normal` output, written field by field.
///
/// Fields are separated by single spaces, and empty fields are left out. Every field is escaped as it's written, so a
/// sender can't split a line into several, and nothing is built up in memory before it reaches the output.
struct ItemLine<'a> {
    output: &'a mut String,
    has_fields: bool,
}

impl<'a> ItemLine<'a> {
    /// Starts a new line.
    fn start(output: &'a mut String) -> Result<Self, fmt::Error> {
        output.write_char('\n')?;
        Ok(Self {
            output,
            has_fields: false,
        })
    }

    /// Writes the space that separates a field from the one before it.
    fn separate(&mut self) -> fmt::Result {
        if self.has_fields {
            self.output.write_char(' ')?;
        }
        self.has_fields = true;
        Ok(())
    }

    /// Writes a text field, unless it's empty.
    fn text(&mut self, text: &str) -> fmt::Result {
        if text.is_empty() {
            return Ok(());
        }
        self.separate()?;
        write!(self.output, "{}", Escaped(text))
    }

    /// Writes a byte field as lowercase hex, unless it's empty.
    fn hex(&mut self, bytes: &[u8]) -> fmt::Result {
        self.text(&bytes_to_hex_lowercase(bytes))
    }

    /// Writes each attribute as a `key=value` field.
    fn attributes(&mut self, attributes: &[KeyValue]) -> fmt::Result {
        for attribute in attributes {
            self.separate()?;
            write!(
                self.output,
                "{}={}",
                Escaped(&attribute.key),
                Escaped(&value_text(attribute.value.as_ref()))
            )?;
        }
        Ok(())
    }
}

/// Returns the text of an attribute or body value. An absent value is empty. A string value isn't copied.
fn value_text(value: Option<&AnyValue>) -> Cow<'_, str> {
    match value.and_then(|value| value.value.as_ref()) {
        Some(AnyValueKind::StringValue(text)) => Cow::Borrowed(text),
        Some(other) => Cow::Owned(otlp_value_to_string(other).unwrap_or_default()),
        None => Cow::Borrowed(""),
    }
}

/// Text with its control characters escaped, so a line of `normal` output can't be split into several by the data it
/// describes.
///
/// A sender controls the names and values that end up in a line. A newline in one would otherwise start what looks
/// like a separate log entry. The text is escaped as it's written, so nothing is copied to escape it.
struct Escaped<'a>(&'a str);

impl Display for Escaped<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // U+2028 and U+2029 aren't control characters, but some log viewers break lines on them.
        let needs_escape = |c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}');

        let mut clean_start = 0;
        for (index, c) in self.0.char_indices() {
            if needs_escape(c) {
                formatter.write_str(&self.0[clean_start..index])?;
                write!(formatter, "{}", c.escape_default())?;
                clean_start = index + c.len_utf8();
            }
        }
        formatter.write_str(&self.0[clean_start..])
    }
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
    use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use otlp_protos::opentelemetry::proto::metrics::v1::{Gauge, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum};
    use otlp_protos::opentelemetry::proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

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
        let writes_nothing = |_: &mut String| -> fmt::Result { panic!("this part must not be rendered") };
        let writes_ok = |_: &mut String| -> fmt::Result { Ok(()) };

        output(OtlpDebugVerbosity::None).render_request(renders_nothing, writes_nothing, writes_nothing);
        output(OtlpDebugVerbosity::Basic).render_request(String::new, writes_nothing, writes_nothing);
        output(OtlpDebugVerbosity::Normal).render_request(String::new, writes_ok, writes_nothing);
        output(OtlpDebugVerbosity::Detailed).render_request(String::new, writes_nothing, writes_ok);
    }

    #[test]
    fn escaping_leaves_clean_text_alone_and_escapes_the_rest() {
        assert_eq!(Escaped("plain text, ünïcode ok").to_string(), "plain text, ünïcode ok");
        assert_eq!(
            Escaped("line\nbreak\u{2028}end").to_string(),
            "line\\nbreak\\u{2028}end"
        );
        assert_eq!(Escaped("").to_string(), "");
    }

    #[test]
    fn normal_output_is_never_truncated() {
        // Debug output is opt-in, and it's used to see everything that arrived, so a large request is written whole.
        let rendered = output(OtlpDebugVerbosity::Normal)
            .render_logs(&logs_request(&[10_000]))
            .unwrap();

        assert_eq!(rendered.lines().count(), 10_001);
    }

    #[test]
    fn detailed_output_is_never_truncated() {
        let mut request = traces_request(&[1]);
        request.resource_spans[0].scope_spans[0].spans[0].trace_id = vec![7; 10_000];

        let rendered = output(OtlpDebugVerbosity::Detailed).render_traces(&request).unwrap();

        // Pretty-printing writes a line for each byte, and every one of them is there.
        assert!(rendered.lines().filter(|line| line.trim() == "7,").count() >= 10_000);
        assert!(rendered.ends_with('}'));
    }
}

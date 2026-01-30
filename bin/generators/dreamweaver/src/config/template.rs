//! Service template configuration.

use serde::Deserialize;

/// A service template that defines what telemetry a service type emits.
#[derive(Clone, Debug, Deserialize)]
pub struct ServiceTemplate {
    /// Trace generation configuration.
    #[serde(default)]
    pub traces: Option<TraceTemplate>,

    /// Metric generation configuration.
    #[serde(default)]
    pub metrics: Option<Vec<MetricTemplate>>,

    /// Log generation configuration.
    #[serde(default)]
    pub logs: Option<Vec<LogTemplate>>,
}

/// Configuration for trace generation.
#[derive(Clone, Debug, Deserialize)]
pub struct TraceTemplate {
    /// The kind of span this service generates.
    pub span_kind: SpanKind,

    /// The list of operation names to choose from.
    pub operations: Vec<String>,

    /// The probability of a span having an error (0.0 to 1.0).
    #[serde(default)]
    pub error_rate: f64,

    /// Base duration in milliseconds for spans (actual duration varies).
    #[serde(default = "default_base_duration_ms")]
    pub base_duration_ms: u64,
}

fn default_base_duration_ms() -> u64 {
    50
}

/// The kind of span.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// A server span (e.g., HTTP server handling a request).
    Server,
    /// A client span (e.g., HTTP client making a request).
    Client,
    /// A producer span (e.g., message queue producer).
    Producer,
    /// A consumer span (e.g., message queue consumer).
    Consumer,
    /// An internal span (e.g., internal function call).
    Internal,
}

impl SpanKind {
    /// Converts to the OTLP span kind integer value.
    pub fn to_otlp(self) -> i32 {
        match self {
            SpanKind::Server => 2,   // SPAN_KIND_SERVER
            SpanKind::Client => 3,   // SPAN_KIND_CLIENT
            SpanKind::Producer => 4, // SPAN_KIND_PRODUCER
            SpanKind::Consumer => 5, // SPAN_KIND_CONSUMER
            SpanKind::Internal => 1, // SPAN_KIND_INTERNAL
        }
    }
}

/// Configuration for metric generation.
#[derive(Clone, Debug, Deserialize)]
pub struct MetricTemplate {
    /// The metric name.
    pub name: String,

    /// The type of metric.
    #[serde(rename = "type")]
    pub metric_type: MetricType,

    /// The unit of measurement (optional).
    #[serde(default)]
    pub unit: Option<String>,
}

/// The type of metric.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricType {
    /// A counter that only increases.
    Counter,
    /// A gauge that can go up or down.
    Gauge,
    /// A histogram for distributions.
    Histogram,
}

/// Configuration for log generation.
#[derive(Clone, Debug, Deserialize)]
pub struct LogTemplate {
    /// The log message patterns to choose from.
    pub patterns: Vec<String>,

    /// The severity level of the logs.
    #[serde(default)]
    pub severity: LogSeverity,
}

/// Log severity levels.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogSeverity {
    /// Trace level.
    Trace,
    /// Debug level.
    Debug,
    /// Info level.
    #[default]
    Info,
    /// Warning level.
    Warn,
    /// Error level.
    Error,
    /// Fatal level.
    Fatal,
}

impl LogSeverity {
    /// Converts to the OTLP severity number.
    pub fn to_otlp_number(self) -> i32 {
        match self {
            LogSeverity::Trace => 1,
            LogSeverity::Debug => 5,
            LogSeverity::Info => 9,
            LogSeverity::Warn => 13,
            LogSeverity::Error => 17,
            LogSeverity::Fatal => 21,
        }
    }

    /// Returns the severity text representation.
    pub fn to_text(self) -> &'static str {
        match self {
            LogSeverity::Trace => "TRACE",
            LogSeverity::Debug => "DEBUG",
            LogSeverity::Info => "INFO",
            LogSeverity::Warn => "WARN",
            LogSeverity::Error => "ERROR",
            LogSeverity::Fatal => "FATAL",
        }
    }
}

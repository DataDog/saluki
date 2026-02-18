//! Configuration for the OTTL filter processor.
//!
//! Follows the [OpenTelemetry filterprocessor] spec for YAML structure.
//!
//! [OpenTelemetry filterprocessor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/filterprocessor

use serde::Deserialize;

/// Error mode when an OTTL condition evaluation fails.
///
/// Defaults to `propagate` if not specified (per filterprocessor spec).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorMode {
    /// Ignore errors, log them, and continue to the next condition.
    #[serde(alias = "ignore")]
    Ignore,

    /// Ignore errors, do not log them, and continue.
    #[serde(alias = "silent")]
    Silent,

    /// Return the error up the pipeline; the payload is dropped.
    #[default]
    #[serde(alias = "propagate")]
    Propagate,
}

/// Traces filter configuration (span and span event conditions).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracesFilterConfig {
    /// OTTL conditions for spans. If any condition matches, the span is dropped.
    #[serde(default)]
    pub span: Vec<String>,
}

/// Root YAML configuration for the OTTL filter processor.
///
/// Matches the structure used by the OpenTelemetry Collector Contrib filterprocessor.
/// Read from either `data_plane.otlp.filter` or `processors.filter/ottl`. Example:
///
/// ```yaml
/// processors:
///   filter/ottl:
///     error_mode: ignore
///     traces:
///       span:
///         - 'attributes["container.name"] == "app_container_1"'
///         - 'resource.attributes["host.name"] == "localhost"'
///         - 'name == "app_3"'
/// ```
///
/// Serde deserializes that into: `error_mode` → [`ErrorMode`], `traces.span` → [`TracesFilterConfig::span`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OttlFilterConfig {
    /// How to handle errors during condition evaluation.
    ///
    /// Defaults to `propagate` if not specified.
    #[serde(default)]
    pub error_mode: ErrorMode,

    /// Trace-level filters (span and span event conditions).
    #[serde(default)]
    pub traces: TracesFilterConfig,
}

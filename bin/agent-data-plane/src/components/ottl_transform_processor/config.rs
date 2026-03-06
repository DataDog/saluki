//! Configuration for the OTTL transform processor.
//!
//! Follows the [OpenTelemetry transform processor] spec for YAML structure.
//!
//! [OpenTelemetry Transform processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor#general-config

use serde::Deserialize;

/// Error mode when an OTTL condition evaluation fails.
///
/// Defaults to `propagate` if not specified (per transform processor spec).
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

/// Root YAML configuration for the OTTL transform processor.
///
/// Matches the structure used by the OpenTelemetry Collector Contrib transform processor.
/// Example:
/// ```yaml
/// ottl_transform_config:
///   error_mode: ignore
///   trace_statements:
///     - 'set(attributes["container.name"], "app_container_1")'
///     - 'set(resource.attributes["host.name"], "localhost")'
/// ```
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OttlTransformConfig {
    /// How to handle errors during condition evaluation.
    ///
    /// Defaults to `propagate` if not specified.
    #[serde(default)]
    pub error_mode: ErrorMode,

    /// OTTL transform statements
    #[serde(default)]
    pub trace_statements: Vec<String>,
}

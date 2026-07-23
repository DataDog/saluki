//! Shared OTLP receiver configuration.

use serde::Deserialize;

/// Configuration for OTLP traces processing.
///
/// Mirrors the Agent's `otlp_config.traces` configuration.
#[derive(Clone, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct TracesConfig {
    /// Whether to skip deriving Datadog fields from standard OTLP attributes.
    ///
    /// When true, only uses explicit `datadog.*` prefixed attributes and skips
    /// fallback resolution from OTLP semantic conventions.
    ///
    /// Corresponds to `otlp_config.traces.ignore_missing_datadog_fields` in the Agent.
    ///
    /// Defaults to `false`.
    #[serde(default)]
    pub ignore_missing_datadog_fields: bool,

    /// When true, `_top_level` and `_dd.measured` are derived using the OTLP span kind.
    ///
    /// Corresponds to the `enable_otlp_compute_top_level_by_span_kind` feature flag
    /// in the Agent's `apm_config.features`.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_otlp_compute_top_level_by_span_kind")]
    pub enable_otlp_compute_top_level_by_span_kind: bool,
}

const fn default_enable_otlp_compute_top_level_by_span_kind() -> bool {
    true
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: default_enable_otlp_compute_top_level_by_span_kind(),
        }
    }
}

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

    /// Probabilistic sampler configuration for OTLP traces.
    ///
    /// Corresponds to `otlp_config.traces.probabilistic_sampler` in the Agent.
    #[serde(default)]
    pub probabilistic_sampler: ProbabilisticSampler,
}

/// Configuration for OTLP traces probabilistic sampling.
#[derive(Clone, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct ProbabilisticSampler {
    /// Percentage of traces to ingest (0, 100].
    ///
    /// Invalid values (<= 0 || > 100) are disregarded and the default is used.
    ///
    /// Corresponds to `otlp_config.traces.probabilistic_sampler.sampling_percentage` in the Agent.
    ///
    /// Defaults to 100.0 (100% sampling).
    #[serde(default = "default_sampling_percentage")]
    pub sampling_percentage: f64,
}

const fn default_sampling_percentage() -> f64 {
    100.0
}

impl Default for ProbabilisticSampler {
    fn default() -> Self {
        Self {
            sampling_percentage: default_sampling_percentage(),
        }
    }
}

const fn default_enable_otlp_compute_top_level_by_span_kind() -> bool {
    true
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: default_enable_otlp_compute_top_level_by_span_kind(),
            probabilistic_sampler: ProbabilisticSampler::default(),
        }
    }
}

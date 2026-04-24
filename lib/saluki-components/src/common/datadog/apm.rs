use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde::Deserialize;
use stringtheory::MetaString;

use super::obfuscation::ObfuscationConfig;

const fn default_target_traces_per_second() -> f64 {
    10.0
}

const fn default_errors_per_second() -> f64 {
    10.0
}
const fn default_sampling_percentage() -> f64 {
    100.0
}

const fn default_error_sampling_enabled() -> bool {
    true
}

const fn default_error_tracking_standalone_enabled() -> bool {
    false
}

const fn default_probabilistic_sampling_enabled() -> bool {
    false
}
const fn default_rare_sampler_enabled() -> bool {
    false
}

const fn default_rare_sampler_tps() -> f64 {
    5.0
}

const fn default_rare_sampler_cooldown_secs() -> f64 {
    300.0 // 5 minutes
}

const fn default_rare_sampler_cardinality() -> usize {
    200
}

const fn default_peer_tags_aggregation() -> bool {
    true
}

const fn default_compute_stats_by_span_kind() -> bool {
    true
}

fn default_env() -> MetaString {
    MetaString::from("none")
}

/// Rare sampler tuning configuration (`apm_config.rare_sampler.*`).
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
struct RareSamplerConfig {
    #[serde(default = "default_rare_sampler_tps")]
    tps: f64,

    #[serde(default = "default_rare_sampler_cooldown_secs")]
    cooldown: f64,

    #[serde(default = "default_rare_sampler_cardinality")]
    cardinality: usize,
}

impl Default for RareSamplerConfig {
    fn default() -> Self {
        Self {
            tps: default_rare_sampler_tps(),
            cooldown: default_rare_sampler_cooldown_secs(),
            cardinality: default_rare_sampler_cardinality(),
        }
    }
}

/// APM configuration.
///
/// This configuration mirrors the Agent's trace agent configuration..
#[derive(Clone, Debug, Deserialize)]
struct ApmConfiguration {
    #[serde(default)]
    apm_config: ApmConfig,

    /// Enables the rare sampler. This needs to live up here rather than nested
    /// within `apm_config` so that we can remap the environment variable path
    /// using the ConfigurationLoader::with_key_aliases.
    #[serde(default = "default_rare_sampler_enabled", rename = "apm_enable_rare_sampler")]
    enable_rare_sampler: bool,

    /// Enables Error Tracking Standalone mode. Lives here (rather than nested within `apm_config`)
    /// so that the env var path (`DD_APM_ERROR_TRACKING_STANDALONE_ENABLED` → `apm_error_tracking_standalone_enabled`)
    /// can be remapped via ConfigurationLoader::with_key_aliases.
    #[serde(
        default = "default_error_tracking_standalone_enabled",
        rename = "apm_error_tracking_standalone_enabled"
    )]
    enable_error_tracking_standalone: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
struct ProbabilisticSamplerConfig {
    /// Enables probabilistic sampling.
    ///
    /// When enabled, the trace sampler keeps approximately `sampling_percentage` of traces using a
    /// deterministic hash of the trace ID.
    ///
    /// Defaults to `false`.
    #[serde(default = "default_probabilistic_sampling_enabled")]
    enabled: bool,

    /// Sampling percentage (0-100).
    ///
    /// Determines the percentage of traces to keep. A value of 100 keeps all traces,
    /// while 50 keeps approximately half. Values outside 0-100 are treated as 100.
    ///
    /// Defaults to 100.0 (keep all traces).
    #[serde(default = "default_sampling_percentage")]
    sampling_percentage: f64,
}

impl Default for ProbabilisticSamplerConfig {
    fn default() -> Self {
        Self {
            enabled: default_probabilistic_sampling_enabled(),
            sampling_percentage: default_sampling_percentage(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ApmConfig {
    /// Target traces per second for priority sampling.
    ///
    /// Defaults to 10.0.
    #[serde(default = "default_target_traces_per_second")]
    target_traces_per_second: f64,

    /// Target traces per second for error sampling.
    ///
    /// Defaults to 10.0.
    #[serde(default = "default_errors_per_second")]
    errors_per_second: f64,

    /// Probabilistic sampler configuration.
    ///
    /// Defaults to enabled with `sampling_percentage` set to 100.0 (keep all traces).
    #[serde(default)]
    probabilistic_sampler: ProbabilisticSamplerConfig,

    /// Enable error sampling in the trace sampler.
    ///
    /// When enabled, traces containing errors will be kept even if they would be dropped by
    /// probabilistic sampling. This ensures error visibility at low sampling rates.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_error_sampling_enabled")]
    error_sampling_enabled: bool,

    #[serde(skip)]
    error_tracking_standalone: bool,

    /// Enables an additional stats computation check on spans to see if they have an eligible `span.kind` (server, consumer, client, producer).
    /// If enabled, a span with an eligible `span.kind` will have stats computed. If disabled, only top-level and measured spans will have stats computed.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_compute_stats_by_span_kind")]
    compute_stats_by_span_kind: bool,

    /// Enables aggregation of peer related tags (for example, `peer.service`, `db.instance`, etc.) in the Agent.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_peer_tags_aggregation")]
    peer_tags_aggregation: bool,

    /// Optional list of supplementary peer tags that go beyond the defaults. The Datadog backend validates all tags
    /// and will drop ones that are unapproved.
    ///
    /// Defaults to an empty list.
    #[serde(default)]
    peer_tags: Vec<MetaString>,

    /// Default environment to use when traces don't provide one.
    ///
    /// Defaults to "none".
    #[serde(default = "default_env")]
    default_env: MetaString,

    /// Default hostname to use when traces don't provide one.
    ///
    /// Defaults to empty string (no fallback).
    #[serde(skip)]
    hostname: MetaString,

    #[serde(skip)]
    enable_rare_sampler: bool,

    #[serde(default)]
    rare_sampler: RareSamplerConfig,

    /// Obfuscation configuration for trace data.
    #[serde(default)]
    obfuscation: ObfuscationConfig,
}

impl ApmConfig {
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let wrapper = config.as_typed::<ApmConfiguration>()?;
        let mut apm_config = wrapper.apm_config;
        apm_config.enable_rare_sampler = wrapper.enable_rare_sampler;
        apm_config.error_tracking_standalone = wrapper.enable_error_tracking_standalone;
        Ok(apm_config)
    }

    /// Returns the target traces per second for priority sampling.
    pub const fn target_traces_per_second(&self) -> f64 {
        self.target_traces_per_second
    }

    /// Returns the target traces per second for error sampling.
    pub const fn errors_per_second(&self) -> f64 {
        self.errors_per_second
    }

    /// Returns `true` if probabilistic sampling is enabled.
    pub const fn probabilistic_sampler_enabled(&self) -> bool {
        self.probabilistic_sampler.enabled
    }

    /// Returns the probabilistic sampler sampling percentage.
    pub const fn probabilistic_sampler_sampling_percentage(&self) -> f64 {
        self.probabilistic_sampler.sampling_percentage
    }

    /// Returns `true` if error sampling is enabled.
    pub const fn error_sampling_enabled(&self) -> bool {
        self.error_sampling_enabled
    }

    /// Returns `true` if error tracking standalone mode is enabled.
    pub const fn error_tracking_standalone_enabled(&self) -> bool {
        self.error_tracking_standalone
    }

    /// Returns `true` if stats computation by span kind is enabled.
    pub const fn compute_stats_by_span_kind(&self) -> bool {
        self.compute_stats_by_span_kind
    }

    /// Returns `true` if peer tags aggregation is enabled.
    pub const fn peer_tags_aggregation(&self) -> bool {
        self.peer_tags_aggregation
    }

    /// Returns the supplementary peer tags.
    pub fn peer_tags(&self) -> &[MetaString] {
        &self.peer_tags
    }

    /// Returns the default environment to use when traces don't provide one.
    pub fn default_env(&self) -> &MetaString {
        &self.default_env
    }

    /// Returns the default hostname to use when traces don't provide one.
    pub fn hostname(&self) -> &MetaString {
        &self.hostname
    }

    /// Sets the hostname if it is currently empty.
    pub fn set_hostname_if_empty(&mut self, hostname: impl Into<MetaString>) {
        if self.hostname.is_empty() {
            self.hostname = hostname.into();
        }
    }

    /// Returns `true` if the rare sampler is enabled.
    pub const fn rare_sampler_enabled(&self) -> bool {
        self.enable_rare_sampler
    }

    /// Returns the rare sampler target traces per second.
    pub const fn rare_sampler_tps(&self) -> f64 {
        self.rare_sampler.tps
    }

    /// Returns the rare sampler cooldown period in seconds.
    pub const fn rare_sampler_cooldown_period_secs(&self) -> f64 {
        self.rare_sampler.cooldown
    }

    /// Returns the rare sampler cardinality limit per shard.
    pub const fn rare_sampler_cardinality(&self) -> usize {
        self.rare_sampler.cardinality
    }

    /// Returns the obfuscation configuration.
    pub fn obfuscation(&self) -> &ObfuscationConfig {
        &self.obfuscation
    }
}

impl Default for ApmConfig {
    fn default() -> Self {
        Self {
            target_traces_per_second: default_target_traces_per_second(),
            errors_per_second: default_errors_per_second(),
            probabilistic_sampler: ProbabilisticSamplerConfig::default(),
            error_sampling_enabled: default_error_sampling_enabled(),
            error_tracking_standalone: default_error_tracking_standalone_enabled(),
            compute_stats_by_span_kind: default_compute_stats_by_span_kind(),
            peer_tags_aggregation: default_peer_tags_aggregation(),
            peer_tags: Vec::new(),
            default_env: default_env(),
            hostname: MetaString::default(),
            enable_rare_sampler: default_rare_sampler_enabled(),
            rare_sampler: RareSamplerConfig::default(),
            obfuscation: ObfuscationConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;

    use super::*;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    async fn apm_config_from(
        file_values: Option<serde_json::Value>, env_vars: Option<&[(String, String)]>,
    ) -> ApmConfig {
        let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
            file_values,
            env_vars,
            false,
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await;
        ApmConfig::from_configuration(&cfg).expect("ApmConfig should deserialize")
    }

    #[tokio::test]
    async fn rare_sampler_disabled_by_default() {
        let config = apm_config_from(None, None).await;
        assert!(!config.rare_sampler_enabled());
    }

    #[tokio::test]
    async fn rare_sampler_enabled_via_yaml() {
        let config = apm_config_from(
            Some(serde_json::json!({ "apm_config": { "enable_rare_sampler": true } })),
            None,
        )
        .await;
        assert!(config.rare_sampler_enabled());
    }

    #[tokio::test]
    async fn rare_sampler_enabled_via_env_var() {
        let env_vars = vec![("APM_ENABLE_RARE_SAMPLER".to_string(), "true".to_string())];
        let config = apm_config_from(None, Some(&env_vars)).await;
        assert!(config.rare_sampler_enabled());
    }

    #[tokio::test]
    async fn rare_sampler_env_var_overrides_yaml() {
        let env_vars = vec![("APM_ENABLE_RARE_SAMPLER".to_string(), "true".to_string())];
        let config = apm_config_from(
            Some(serde_json::json!({ "apm_config": { "enable_rare_sampler": false } })),
            Some(&env_vars),
        )
        .await;
        assert!(config.rare_sampler_enabled());
    }

    #[tokio::test]
    async fn ets_disabled_by_default() {
        let config = apm_config_from(None, None).await;
        assert!(!config.error_tracking_standalone_enabled());
    }

    #[tokio::test]
    async fn ets_enabled_via_yaml() {
        let config = apm_config_from(
            Some(serde_json::json!({ "apm_config": { "error_tracking_standalone": { "enabled": true } } })),
            None,
        )
        .await;
        assert!(config.error_tracking_standalone_enabled());
    }

    #[tokio::test]
    async fn ets_enabled_via_env_var() {
        let env_vars = vec![("APM_ERROR_TRACKING_STANDALONE_ENABLED".to_string(), "true".to_string())];
        let config = apm_config_from(None, Some(&env_vars)).await;
        assert!(config.error_tracking_standalone_enabled());
    }

    #[tokio::test]
    async fn ets_env_var_overrides_yaml() {
        let env_vars = vec![("APM_ERROR_TRACKING_STANDALONE_ENABLED".to_string(), "true".to_string())];
        let config = apm_config_from(
            Some(serde_json::json!({ "apm_config": { "error_tracking_standalone": { "enabled": false } } })),
            Some(&env_vars),
        )
        .await;
        assert!(config.error_tracking_standalone_enabled());
    }

    #[tokio::test]
    async fn rare_sampler_tuning_via_yaml() {
        let config = apm_config_from(
            Some(serde_json::json!({
                "apm_config": {
                    "rare_sampler": { "tps": 10, "cooldown": 60, "cardinality": 100 }
                }
            })),
            None,
        )
        .await;
        assert_eq!(config.rare_sampler_tps(), 10.0);
        assert_eq!(config.rare_sampler_cooldown_period_secs(), 60.0);
        assert_eq!(config.rare_sampler_cardinality(), 100);
    }
}

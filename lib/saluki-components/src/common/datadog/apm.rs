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
const fn default_error_tracking_standalone_enabled() -> bool {
    false
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

/// APM configuration.
///
/// This configuration mirrors the Agent's trace agent configuration..
#[derive(Clone, Debug, Deserialize)]
struct ApmConfiguration {
    #[serde(default)]
    apm_config: ApmConfig,

    /// Enables Error Tracking Standalone mode. Lives here (rather than nested within `apm_config`)
    /// so that the env var path (`DD_APM_ERROR_TRACKING_STANDALONE_ENABLED` → `apm_error_tracking_standalone_enabled`)
    /// can be remapped via ConfigurationLoader::with_key_aliases.
    #[serde(
        default = "default_error_tracking_standalone_enabled",
        rename = "apm_error_tracking_standalone_enabled"
    )]
    enable_error_tracking_standalone: bool,

    /// Obfuscation config read from flat `apm_obfuscation_*` keys.
    /// KEY_ALIASES in `crate::config` bridge the YAML nested paths to these flat keys.
    #[serde(default, flatten)]
    obfuscation: ObfuscationConfig,
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
    /// Defaults to `"none"`.
    #[serde(default = "default_env")]
    default_env: MetaString,

    /// Default hostname to use when traces don't provide one.
    ///
    /// Defaults to empty string (no fallback).
    #[serde(skip)]
    hostname: MetaString,

    /// Obfuscation configuration for trace data.
    ///
    /// Populated from `ApmConfiguration.obfuscation` in `from_configuration`; not read from serde directly.
    #[serde(skip)]
    obfuscation: ObfuscationConfig,
}

impl ApmConfig {
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let wrapper = config.as_typed::<ApmConfiguration>()?;
        let mut apm_config = wrapper.apm_config;
        apm_config.error_tracking_standalone = wrapper.enable_error_tracking_standalone;
        apm_config.obfuscation = wrapper.obfuscation;
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

    /// Sets the hostname if it's currently empty.
    pub fn set_hostname_if_empty(&mut self, hostname: impl Into<MetaString>) {
        if self.hostname.is_empty() {
            self.hostname = hostname.into();
        }
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
            error_tracking_standalone: default_error_tracking_standalone_enabled(),
            compute_stats_by_span_kind: default_compute_stats_by_span_kind(),
            peer_tags_aggregation: default_peer_tags_aggregation(),
            peer_tags: Vec::new(),
            default_env: default_env(),
            hostname: MetaString::default(),
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
}

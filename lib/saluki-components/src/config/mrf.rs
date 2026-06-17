//! Multi-region failover configuration.

use saluki_component_config::MrfConfig;

/// Multi-region failover configuration shared by signal-specific pipelines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MrfConfiguration {
    config: MrfConfig,
}

impl MrfConfiguration {
    /// Creates a new `MrfConfiguration` from native component config.
    pub fn from_native(config: MrfConfig) -> Self {
        Self { config }
    }

    /// Returns whether multi-region failover is enabled for this process.
    pub const fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Returns whether metrics forwarding to the failover region is requested by configuration.
    pub const fn is_metrics_forwarding_requested(&self) -> bool {
        self.config.enabled && self.config.failover_metrics
    }

    /// Returns the metric allowlist.
    pub fn metric_allowlist(&self) -> &[String] {
        &self.config.metric_allowlist
    }

    /// Returns the failover-region API key.
    pub fn api_key(&self) -> Option<&str> {
        self.config.api_key.as_deref()
    }

    /// Returns the failover-region metrics endpoint URL.
    pub fn metrics_endpoint_url(&self) -> Option<String> {
        self.config.endpoint.clone()
    }

    /// Returns the endpoint and API key override for the failover-region metrics forwarder.
    pub fn metrics_endpoint_override(&self) -> Option<(String, String)> {
        if !self.config.enabled {
            return None;
        }

        Some((self.metrics_endpoint_url()?, self.config.api_key.clone()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mrf_config_from(config: MrfConfig) -> MrfConfiguration {
        MrfConfiguration::from_native(config)
    }

    #[test]
    fn parses_mrf_configuration_keys() {
        let config = mrf_config_from(MrfConfig {
            enabled: true,
            failover_metrics: true,
            metric_allowlist: vec!["first.metric".to_string(), "second.metric".to_string()],
            api_key: Some("mrf-api-key".to_string()),
            endpoint: Some("https://app.mrf.datadoghq.eu".to_string()),
        });

        assert!(config.is_metrics_forwarding_requested());
        assert_eq!(config.metric_allowlist(), ["first.metric", "second.metric"]);
        assert_eq!(config.api_key(), Some("mrf-api-key"));
        assert_eq!(
            config.metrics_endpoint_url().as_deref(),
            Some("https://app.mrf.datadoghq.eu")
        );
    }

    #[test]
    fn metrics_endpoint_override_requires_api_key_and_endpoint() {
        let missing_api_key = mrf_config_from(MrfConfig {
            enabled: true,
            failover_metrics: true,
            endpoint: Some("https://app.mrf.datadoghq.eu".to_string()),
            ..MrfConfig::default()
        });
        assert_eq!(missing_api_key.metrics_endpoint_override(), None);

        let missing_endpoint = mrf_config_from(MrfConfig {
            enabled: true,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            ..MrfConfig::default()
        });
        assert_eq!(missing_endpoint.metrics_endpoint_override(), None);

        let disabled = mrf_config_from(MrfConfig {
            enabled: false,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            endpoint: Some("https://app.mrf.datadoghq.eu".to_string()),
            ..MrfConfig::default()
        });
        assert_eq!(disabled.metrics_endpoint_override(), None);
    }

    #[test]
    fn blank_endpoint_has_no_override() {
        let config = mrf_config_from(MrfConfig {
            enabled: true,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            endpoint: None,
            ..MrfConfig::default()
        });

        assert_eq!(config.metrics_endpoint_url(), None);
        assert_eq!(config.metrics_endpoint_override(), None);
    }
}

//! Multi-region failover configuration.

use agent_data_plane_config::domains::multi_region_failover;

/// Multi-region failover configuration shared by signal-specific pipelines.
#[derive(Debug, Eq, PartialEq)]
pub struct MrfConfiguration {
    enabled: bool,
    api_key: Option<String>,
    metrics_endpoint: Option<String>,
}

impl MrfConfiguration {
    /// Creates a new `MrfConfiguration` from the resolved multi-region failover configuration.
    pub fn from_configuration(mrf: &multi_region_failover::Domain) -> Self {
        Self {
            enabled: mrf.enabled,
            api_key: mrf.api_key.clone(),
            metrics_endpoint: mrf.metrics_endpoint_url(),
        }
    }

    /// Returns whether multi-region failover is enabled for this process.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the failover-region API key.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Returns the failover-region metrics endpoint URL.
    pub fn metrics_endpoint_url(&self) -> Option<&str> {
        self.metrics_endpoint.as_deref()
    }

    /// Returns the endpoint and API key override for the failover-region metrics forwarder.
    pub fn metrics_endpoint_override(&self) -> Option<(String, String)> {
        if !self.enabled {
            return None;
        }

        Some((self.metrics_endpoint.clone()?, self.api_key.clone()?))
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::multi_region_failover::MetricMirroring;

    use super::*;

    fn mrf_config(mrf: multi_region_failover::Domain) -> MrfConfiguration {
        MrfConfiguration::from_configuration(&mrf)
    }

    #[test]
    fn carries_the_resolved_multi_region_failover_configuration() {
        let config = mrf_config(multi_region_failover::Domain {
            enabled: true,
            api_key: Some("mrf-api-key".to_string()),
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
        });

        assert!(config.is_enabled());
        assert_eq!(Some("mrf-api-key"), config.api_key());
        assert_eq!(Some("https://app.mrf.datadoghq.eu"), config.metrics_endpoint_url());
    }

    #[test]
    fn metrics_endpoint_override_requires_api_key_and_endpoint() {
        let missing_api_key = mrf_config(multi_region_failover::Domain {
            enabled: true,
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
        });
        assert_eq!(None, missing_api_key.metrics_endpoint_override());

        let missing_endpoint = mrf_config(multi_region_failover::Domain {
            enabled: true,
            api_key: Some("mrf-api-key".to_string()),
            ..Default::default()
        });
        assert_eq!(None, missing_endpoint.metrics_endpoint_override());

        let ready = mrf_config(multi_region_failover::Domain {
            enabled: true,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        });
        assert_eq!(
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string())),
            ready.metrics_endpoint_override()
        );
    }

    #[test]
    fn metrics_endpoint_override_does_not_require_failover_metrics() {
        // The endpoint override is what wires the failover forwarder, and it is deliberately independent of
        // `failover_metrics`: that setting can turn metric mirroring on later, without a restart.
        let config = mrf_config(multi_region_failover::Domain {
            enabled: true,
            metric_mirroring: MetricMirroring {
                enabled: false,
                allowlist: Vec::new(),
            },
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        });

        assert_eq!(
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string())),
            config.metrics_endpoint_override()
        );
    }

    #[test]
    fn metrics_endpoint_override_is_none_when_disabled() {
        // Even with a fully-populated endpoint and API key, `metrics_endpoint_override` short-circuits to `None`
        // when multi-region failover is disabled (the `if !self.enabled` guard at the top of the method).
        let config = mrf_config(multi_region_failover::Domain {
            enabled: false,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        });

        assert!(!config.is_enabled());
        assert_eq!(None, config.metrics_endpoint_override());

        // The endpoint itself still resolves; only the override is gated on `enabled`, which confirms the `None`
        // above comes from the disabled short-circuit rather than a missing endpoint or API key.
        assert_eq!(Some("https://mrf.example.com"), config.metrics_endpoint_url());
        assert_eq!(Some("mrf-api-key"), config.api_key());
    }
}

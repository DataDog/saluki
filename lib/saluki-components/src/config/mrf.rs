//! Multi-region failover configuration.

use agent_data_plane_config::domains::multi_region_failover::Domain;
use saluki_error::GenericError;

const MRF_METRICS_ENDPOINT_PREFIX: &str = "https://app.mrf.";

/// Multi-region failover configuration shared by signal-specific pipelines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MrfConfiguration {
    enabled: bool,
    failover_metrics: bool,
    metric_allowlist: Vec<String>,
    api_key: Option<String>,
    site: Option<String>,
    dd_url: Option<String>,
}

impl MrfConfiguration {
    /// Creates a new `MrfConfiguration` from the resolved multi-region failover configuration.
    pub fn from_configuration(config: &Domain) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.enabled,
            failover_metrics: config.failover_metrics,
            metric_allowlist: config.metric_allowlist.clone(),
            api_key: config.api_key.clone(),
            site: config.site.clone(),
            dd_url: config.dd_url.clone(),
        })
    }

    /// Returns whether multi-region failover is enabled for this process.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns whether metrics forwarding to the failover region is requested by configuration.
    pub const fn is_metrics_forwarding_requested(&self) -> bool {
        self.enabled && self.failover_metrics
    }

    /// Updates whether metrics forwarding to the failover region is enabled.
    pub(crate) const fn set_failover_metrics(&mut self, failover_metrics: bool) {
        self.failover_metrics = failover_metrics;
    }

    /// Updates the metric allowlist.
    pub(crate) fn set_metric_allowlist(&mut self, metric_allowlist: Vec<String>) {
        self.metric_allowlist = metric_allowlist;
    }

    /// Returns the metric allowlist.
    pub fn metric_allowlist(&self) -> &[String] {
        &self.metric_allowlist
    }

    /// Returns the failover-region API key.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Returns the failover-region metrics endpoint URL.
    ///
    /// `multi_region_failover.dd_url` takes precedence and is used as provided. When only
    /// `multi_region_failover.site` is configured, the Datadog MRF metrics endpoint is derived from
    /// that site.
    pub fn metrics_endpoint_url(&self) -> Option<String> {
        self.dd_url.clone().or_else(|| {
            self.site
                .as_deref()
                .map(|site| format!("{MRF_METRICS_ENDPOINT_PREFIX}{site}"))
        })
    }

    /// Returns the endpoint and API key override for the failover-region metrics forwarder.
    pub fn metrics_endpoint_override(&self) -> Option<(String, String)> {
        if !self.enabled {
            return None;
        }

        Some((self.metrics_endpoint_url()?, self.api_key.clone()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mrf_config_from(config: Domain) -> MrfConfiguration {
        MrfConfiguration::from_configuration(&config).expect("MRF configuration should build")
    }

    #[test]
    fn builds_from_model_configuration() {
        let config = mrf_config_from(Domain {
            enabled: true,
            failover_metrics: true,
            metric_allowlist: vec!["first.metric".to_string(), "second.metric".to_string()],
            api_key: Some("mrf-api-key".to_string()),
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
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
        let missing_api_key = mrf_config_from(Domain {
            enabled: true,
            failover_metrics: true,
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
        });
        assert_eq!(missing_api_key.metrics_endpoint_override(), None);

        let missing_endpoint = mrf_config_from(Domain {
            enabled: true,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            ..Default::default()
        });
        assert_eq!(missing_endpoint.metrics_endpoint_override(), None);

        let ready = mrf_config_from(Domain {
            enabled: true,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        });
        assert_eq!(
            ready.metrics_endpoint_override(),
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string()))
        );
    }

    #[test]
    fn metrics_endpoint_override_does_not_require_failover_metrics() {
        let config = mrf_config_from(Domain {
            enabled: true,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        });

        assert!(!config.is_metrics_forwarding_requested());
        assert_eq!(
            config.metrics_endpoint_override(),
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string()))
        );
    }

    #[test]
    fn dd_url_takes_precedence_over_site() {
        let config = mrf_config_from(Domain {
            site: Some("datadoghq.eu".to_string()),
            dd_url: Some("https://custom-mrf.example.com".to_string()),
            ..Default::default()
        });

        assert_eq!(
            config.metrics_endpoint_url().as_deref(),
            Some("https://custom-mrf.example.com")
        );
    }
}

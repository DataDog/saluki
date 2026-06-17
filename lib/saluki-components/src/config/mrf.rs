//! Multi-region failover configuration.

use saluki_config_tools::GenericConfiguration;
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
    /// Creates a new `MrfConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.try_get_typed("multi_region_failover.enabled")?.unwrap_or(false),
            failover_metrics: config
                .try_get_typed("multi_region_failover.failover_metrics")?
                .unwrap_or(false),
            metric_allowlist: config
                .try_get_typed("multi_region_failover.metric_allowlist")?
                .unwrap_or_default(),
            api_key: get_non_empty_string(config, "multi_region_failover.api_key")?,
            site: get_non_empty_string(config, "multi_region_failover.site")?,
            dd_url: get_non_empty_string(config, "multi_region_failover.dd_url")?,
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

fn get_non_empty_string(config: &GenericConfiguration, key: &str) -> Result<Option<String>, GenericError> {
    Ok(config
        .try_get_typed::<String>(key)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::*;

    async fn mrf_config_from(value: serde_json::Value) -> MrfConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(Some(value), None, false).await;
        MrfConfiguration::from_configuration(&config).expect("MRF configuration should deserialize")
    }

    #[tokio::test]
    async fn parses_mrf_configuration_keys() {
        let config = mrf_config_from(json!({
            "multi_region_failover": {
                "enabled": true,
                "failover_metrics": true,
                "metric_allowlist": ["first.metric", "second.metric"],
                "api_key": "mrf-api-key",
                "site": "datadoghq.eu"
            }
        }))
        .await;

        assert!(config.is_metrics_forwarding_requested());
        assert_eq!(config.metric_allowlist(), ["first.metric", "second.metric"]);
        assert_eq!(config.api_key(), Some("mrf-api-key"));
        assert_eq!(
            config.metrics_endpoint_url().as_deref(),
            Some("https://app.mrf.datadoghq.eu")
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_override_requires_api_key_and_endpoint() {
        let missing_api_key = mrf_config_from(json!({
            "multi_region_failover": {
                "enabled": true,
                "failover_metrics": true,
                "site": "datadoghq.eu"
            }
        }))
        .await;
        assert_eq!(missing_api_key.metrics_endpoint_override(), None);

        let missing_endpoint = mrf_config_from(json!({
            "multi_region_failover": {
                "enabled": true,
                "failover_metrics": true,
                "api_key": "mrf-api-key"
            }
        }))
        .await;
        assert_eq!(missing_endpoint.metrics_endpoint_override(), None);

        let ready = mrf_config_from(json!({
            "multi_region_failover": {
                "enabled": true,
                "failover_metrics": true,
                "api_key": "mrf-api-key",
                "dd_url": "https://mrf.example.com"
            }
        }))
        .await;
        assert_eq!(
            ready.metrics_endpoint_override(),
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string()))
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_override_does_not_require_failover_metrics() {
        let config = mrf_config_from(json!({
            "multi_region_failover": {
                "enabled": true,
                "failover_metrics": false,
                "api_key": "mrf-api-key",
                "dd_url": "https://mrf.example.com"
            }
        }))
        .await;

        assert!(!config.is_metrics_forwarding_requested());
        assert_eq!(
            config.metrics_endpoint_override(),
            Some(("https://mrf.example.com".to_string(), "mrf-api-key".to_string()))
        );
    }

    #[tokio::test]
    async fn dd_url_takes_precedence_over_site() {
        let config = mrf_config_from(json!({
            "multi_region_failover": {
                "site": "datadoghq.eu",
                "dd_url": "https://custom-mrf.example.com"
            }
        }))
        .await;

        assert_eq!(
            config.metrics_endpoint_url().as_deref(),
            Some("https://custom-mrf.example.com")
        );
    }
}

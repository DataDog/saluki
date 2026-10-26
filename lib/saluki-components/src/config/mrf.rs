//! Multi-region failover configuration.

/// Multi-region failover configuration shared by signal-specific pipelines.
#[derive(Debug, Eq, PartialEq)]
pub struct MrfConfiguration {
    enabled: bool,
    api_key: Option<String>,
    metrics_endpoint: Option<String>,
}

impl MrfConfiguration {
    /// Creates a new `MrfConfiguration`.
    ///
    /// `api_key` and `metrics_endpoint` are the failover region's API key and metrics intake URL, if configured.
    pub fn new(enabled: bool, api_key: Option<String>, metrics_endpoint: Option<String>) -> Self {
        Self {
            enabled,
            api_key,
            metrics_endpoint,
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
    use super::*;

    const API_KEY: &str = "mrf-api-key";
    const ENDPOINT: &str = "https://mrf.example.com";

    fn mrf_config(enabled: bool, api_key: Option<&str>, metrics_endpoint: Option<&str>) -> MrfConfiguration {
        MrfConfiguration::new(enabled, api_key.map(String::from), metrics_endpoint.map(String::from))
    }

    #[test]
    fn carries_the_failover_region_settings() {
        let config = mrf_config(true, Some(API_KEY), Some(ENDPOINT));

        assert!(config.is_enabled());
        assert_eq!(Some(API_KEY), config.api_key());
        assert_eq!(Some(ENDPOINT), config.metrics_endpoint_url());
    }

    #[test]
    fn metrics_endpoint_override_requires_api_key_and_endpoint() {
        assert_eq!(None, mrf_config(true, None, Some(ENDPOINT)).metrics_endpoint_override());
        assert_eq!(None, mrf_config(true, Some(API_KEY), None).metrics_endpoint_override());
        assert_eq!(
            Some((ENDPOINT.to_string(), API_KEY.to_string())),
            mrf_config(true, Some(API_KEY), Some(ENDPOINT)).metrics_endpoint_override()
        );
    }

    #[test]
    fn metrics_endpoint_override_is_none_when_disabled() {
        // Even with a fully-populated endpoint and API key, `metrics_endpoint_override` short-circuits to `None`
        // when multi-region failover is disabled (the `if !self.enabled` guard at the top of the method).
        let config = mrf_config(false, Some(API_KEY), Some(ENDPOINT));

        assert!(!config.is_enabled());
        assert_eq!(None, config.metrics_endpoint_override());

        // The endpoint itself is still carried; only the override is gated on `enabled`, which confirms the `None`
        // above comes from the disabled short-circuit rather than a missing endpoint or API key.
        assert_eq!(Some(ENDPOINT), config.metrics_endpoint_url());
        assert_eq!(Some(API_KEY), config.api_key());
    }
}

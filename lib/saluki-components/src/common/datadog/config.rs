use std::time::Duration;

use facet::Facet;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde::Deserialize;

use super::{
    endpoints::EndpointConfiguration, protocol::V3ApiConfig, proxy::ProxyConfiguration, retry::RetryConfiguration,
};

const fn default_endpoint_concurrency() -> usize {
    1
}

const fn default_request_timeout_secs() -> u64 {
    20
}

const fn default_endpoint_buffer_size() -> usize {
    16
}

const fn default_forwarder_connection_reset_interval() -> u64 {
    0
}
/// Forwarder configuration based on the Datadog Agent's forwarder configuration.
///
/// This adapter provides a simple way to utilize the existing configuration values that are passed to the Datadog
/// Agent, which are used to control the behavior of its forwarder, such as retries and concurrency, in conjunction with
/// with existing primitives, as such retry policies in [`saluki_io::util::retry`].
#[derive(Clone, Deserialize, Facet)]
pub struct ForwarderConfiguration {
    /// Maximum number of concurrent requests for an individual endpoint.
    ///
    /// Defaults to 1.
    #[serde(default = "default_endpoint_concurrency", rename = "forwarder_num_workers")]
    endpoint_concurrency: usize,

    /// Request timeout, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    ///
    /// Defaults to 16.
    #[serde(default = "default_endpoint_buffer_size", rename = "forwarder_high_prio_buffer_size")]
    endpoint_buffer_size: usize,

    /// Endpoint configuration.
    #[serde(flatten)]
    pub(crate) endpoint: EndpointConfiguration,

    /// Retry configuration.
    #[serde(flatten)]
    retry: RetryConfiguration,

    /// Proxy configuration.
    #[serde(flatten)]
    proxy: Option<ProxyConfiguration>,

    /// Connection reset interval, in seconds.
    ///
    /// Defaults to 0.
    #[serde(
        default = "default_forwarder_connection_reset_interval",
        rename = "forwarder_connection_reset_interval"
    )]
    connection_reset_interval_secs: u64,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// This is read from the encoder configuration and used by the I/O layer to filter payloads
    /// based on endpoint URL matching.
    #[serde(rename = "serializer_experimental_use_v3_api", default)]
    v3_api: V3ApiConfig,
}

impl ForwarderConfiguration {
    /// Creates a new `ForwarderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut forwarder_config = config.as_typed::<Self>()?;

        // Handle fixing up the forwarder storage path if it's empty.
        forwarder_config.retry.fix_empty_storage_path(config);

        Ok(forwarder_config)
    }

    /// Returns the maximum number of concurrent requests for an individual endpoint.
    pub const fn endpoint_concurrency(&self) -> usize {
        self.endpoint_concurrency
    }

    /// Returns the request timeout.
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    /// Returns the maximum number of pending requests for an individual endpoint.
    pub const fn endpoint_buffer_size(&self) -> usize {
        self.endpoint_buffer_size
    }

    /// Returns a reference to the endpoint configuration.
    pub const fn endpoint(&self) -> &EndpointConfiguration {
        &self.endpoint
    }

    /// Returns a mutable reference to the endpoint configuration.
    pub fn endpoint_mut(&mut self) -> &mut EndpointConfiguration {
        &mut self.endpoint
    }

    /// Returns a reference to the retry configuration.
    pub const fn retry(&self) -> &RetryConfiguration {
        &self.retry
    }

    /// Returns a reference to the proxy configuration.
    pub const fn proxy(&self) -> &Option<ProxyConfiguration> {
        &self.proxy
    }

    /// Returns the connection reset interval.
    pub const fn connection_reset_interval(&self) -> Duration {
        Duration::from_secs(self.connection_reset_interval_secs)
    }

    /// Returns a reference to the V3 API configuration.
    pub fn v3_api(&self) -> &V3ApiConfig {
        &self.v3_api
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;

    use super::*;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    // Two distinct proxy URLs to verify which one wins in precedence tests.
    const PROXY_A: &str = "http://proxy-a.example.com:3128";
    const PROXY_A_URI: &str = "http://proxy-a.example.com:3128/";
    const PROXY_B: &str = "http://proxy-b.example.com:3128";
    const PROXY_B_URI: &str = "http://proxy-b.example.com:3128/";

    fn base_config() -> serde_json::Value {
        serde_json::json!({ "api_key": "test-api-key" })
    }

    fn config_with(extra: serde_json::Value) -> serde_json::Value {
        let mut base = base_config();
        if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                base_obj.insert(k.clone(), v.clone());
            }
        }
        base
    }

    async fn forwarder_config_from(
        file_values: serde_json::Value, env_vars: Option<&[(String, String)]>,
    ) -> ForwarderConfiguration {
        let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
            Some(file_values),
            env_vars,
            false,
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await;
        ForwarderConfiguration::from_configuration(&cfg).expect("ForwarderConfiguration should deserialize")
    }

    // Precedence chain: YAML (proxy.http nested) < HTTP_PROXY < DD_PROXY_HTTP
    //
    // The test helper exposes the DD_-prefix tier via PROXY_HTTP: it sets both PROXY_HTTP (raw)
    // and TEST_PROXY_HTTP, and from_environment("TEST") reads TEST_PROXY_HTTP → proxy_http,
    // mirroring how DD_PROXY_HTTP → proxy_http works in production.

    #[tokio::test]
    async fn proxy_set_via_yaml_nested_config() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "proxy": { "http": PROXY_A } })), None).await;

        let proxies = config.proxy().as_ref().unwrap().build().unwrap();
        assert_eq!(proxies[0].uri().to_string(), PROXY_A_URI);
    }

    #[tokio::test]
    async fn http_proxy_env_var_overrides_yaml_nested_config() {
        let env_vars = vec![("HTTP_PROXY".to_string(), PROXY_B.to_string())];
        let config = forwarder_config_from(
            config_with(serde_json::json!({ "proxy": { "http": PROXY_A } })),
            Some(&env_vars),
        )
        .await;

        let proxies = config.proxy().as_ref().unwrap().build().unwrap();
        assert_eq!(proxies[0].uri().to_string(), PROXY_B_URI);
    }

    #[tokio::test]
    async fn dd_proxy_http_env_var_overrides_http_proxy() {
        // PROXY_HTTP simulates DD_PROXY_HTTP: the test helper sets TEST_PROXY_HTTP, which
        // from_environment("TEST") reads as proxy_http — the same path DD_PROXY_HTTP takes
        // in production.
        let env_vars = vec![
            ("HTTP_PROXY".to_string(), PROXY_A.to_string()),
            ("PROXY_HTTP".to_string(), PROXY_B.to_string()),
        ];
        let config = forwarder_config_from(base_config(), Some(&env_vars)).await;

        let proxies = config.proxy().as_ref().unwrap().build().unwrap();
        assert_eq!(proxies[0].uri().to_string(), PROXY_B_URI);
    }
}

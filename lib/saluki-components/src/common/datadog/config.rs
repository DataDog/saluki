use std::time::Duration;

use facet::Facet;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use serde::Deserialize;
use tracing::warn;

use super::{
    endpoints::{EndpointConfiguration, EndpointRoute, RoutableEndpoint},
    protocol::V3ApiConfig,
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
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

const MIN_TLS_VERSION_TLS10: &str = "tlsv1.0";
const MIN_TLS_VERSION_TLS11: &str = "tlsv1.1";
const MIN_TLS_VERSION_TLS12: &str = "tlsv1.2";
const MIN_TLS_VERSION_TLS13: &str = "tlsv1.3";

fn default_min_tls_version() -> String {
    MIN_TLS_VERSION_TLS12.to_string()
}

fn min_tls_version_from_config_value(value: &str) -> TlsMinimumVersion {
    let trimmed = value.trim();
    match trimmed.to_lowercase().as_str() {
        MIN_TLS_VERSION_TLS10 | MIN_TLS_VERSION_TLS11 => {
            warn!(
                config_key = "min_tls_version",
                value = trimmed,
                "Configured TLS minimum version is lower than rustls supports; using tlsv1.2."
            );
            TlsMinimumVersion::Tls12
        }
        "" | MIN_TLS_VERSION_TLS12 => TlsMinimumVersion::Tls12,
        MIN_TLS_VERSION_TLS13 => TlsMinimumVersion::Tls13,
        _ => {
            warn!(
                config_key = "min_tls_version",
                value = trimmed,
                "Invalid configured TLS minimum version; using tlsv1.2."
            );
            TlsMinimumVersion::Tls12
        }
    }
}

/// HTTP protocol selection for the Datadog forwarder.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Facet)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(serde::Serialize))]
pub enum ForwarderHttpProtocol {
    /// Automatically negotiate HTTP/2 with HTTP/1.1 fallback.
    #[default]
    Auto,

    /// Use HTTP/1.1 only.
    Http1,
}

impl From<ForwarderHttpProtocol> for HttpProtocol {
    fn from(protocol: ForwarderHttpProtocol) -> Self {
        match protocol {
            ForwarderHttpProtocol::Auto => Self::Auto,
            ForwarderHttpProtocol::Http1 => Self::Http1,
        }
    }
}

/// OPW metrics endpoint configuration.
#[derive(Clone, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub(crate) struct OpwMetricsConfiguration {
    /// Enables routing all metrics to Observability Pipelines Worker.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "observability_pipelines_worker_metrics_enabled")]
    observability_pipelines_worker_enabled: bool,

    /// Endpoint of the Observability Pipelines Worker instance to route metrics to.
    ///
    /// Defaults to unset.
    #[serde(default, rename = "observability_pipelines_worker_metrics_url")]
    observability_pipelines_worker_url: String,

    /// Enables routing all metrics to Vector.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.enabled`.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "vector_metrics_enabled")]
    vector_enabled: bool,

    /// Endpoint of the Vector instance to route metrics to.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.url`.
    ///
    /// Defaults to unset.
    #[serde(default, rename = "vector_metrics_url")]
    vector_url: String,
}

struct SelectedOpwMetricsEndpoint<'a> {
    enabled_key: &'static str,
    url_key: &'static str,
    url: &'a str,
}

impl OpwMetricsConfiguration {
    fn selected_endpoint(&self) -> Option<SelectedOpwMetricsEndpoint<'_>> {
        if self.observability_pipelines_worker_enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "observability_pipelines_worker.metrics.enabled",
                url_key: "observability_pipelines_worker.metrics.url",
                url: &self.observability_pipelines_worker_url,
            });
        }

        if self.vector_enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "vector.metrics.enabled",
                url_key: "vector.metrics.url",
                url: &self.vector_url,
            });
        }

        None
    }
}

/// Forwarder configuration based on the Datadog Agent's forwarder configuration.
///
/// This adapter provides a simple way to utilize the existing configuration values that are passed to the Datadog
/// Agent, which are used to control the behavior of its forwarder, such as retries and concurrency, in conjunction with
/// with existing primitives, as such retry policies in [`saluki_io::util::retry`].
#[derive(Clone, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
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

    /// OPW metrics routing configuration.
    #[serde(flatten)]
    opw_metrics: OpwMetricsConfiguration,

    /// HTTP protocol selection for outgoing forwarder requests.
    ///
    /// Defaults to `auto`, which negotiates HTTP/2 with HTTP/1.1 fallback. Set to `http1` to force HTTP/1.1 only.
    #[serde(default, rename = "forwarder_http_protocol")]
    http_protocol: ForwarderHttpProtocol,

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

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// Defaults to `false`. If set to `true`, HTTPS clients built for the shared Datadog forwarder accept invalid
    /// server certificates. Only deployments that intentionally route Datadog intake traffic through endpoints with
    /// invalid or self-signed certificates should enable this.
    #[serde(default)]
    skip_ssl_validation: bool,

    /// Minimum TLS protocol version for Datadog intake forwarding.
    ///
    /// Defaults to TLS 1.2. TLS 1.0 and TLS 1.1 are accepted for compatibility with core Agent configuration, but
    /// Saluki clamps them to TLS 1.2 because rustls does not support older protocol versions.
    #[serde(default = "default_min_tls_version")]
    min_tls_version: String,

    /// Parsed minimum TLS protocol version for Datadog intake forwarding.
    #[serde(skip)]
    #[facet(opaque)]
    parsed_min_tls_version: TlsMinimumVersion,

    /// Whether to signal that the backend should allow arbitrary tag values.
    ///
    /// Defaults to `false`. If set to `true`, the Datadog forwarder adds `Allow-Arbitrary-Tag-Value: true` to every
    /// outbound intake request. The data plane does not perform local tag validation based on this setting.
    #[serde(default)]
    allow_arbitrary_tags: bool,
}

impl ForwarderConfiguration {
    /// Creates a new `ForwarderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut forwarder_config = config.as_typed::<Self>()?;
        forwarder_config.parsed_min_tls_version = min_tls_version_from_config_value(&forwarder_config.min_tls_version);

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

    /// Returns the HTTP protocol selection for outgoing forwarder requests.
    pub fn http_protocol(&self) -> HttpProtocol {
        self.http_protocol.into()
    }

    /// Returns a mutable reference to the endpoint configuration.
    pub fn endpoint_mut(&mut self) -> &mut EndpointConfiguration {
        &mut self.endpoint
    }

    /// Clears the OPW metrics endpoint override.
    pub(crate) fn clear_opw_metrics_endpoint(&mut self) {
        self.opw_metrics = OpwMetricsConfiguration::default();
    }

    /// Builds resolved endpoints with routing metadata.
    ///
    /// The normal primary and OPW metrics primary endpoints share the same dynamic API key source.
    pub(crate) fn build_routable_endpoints(
        &self, configuration: Option<GenericConfiguration>,
    ) -> Result<Vec<RoutableEndpoint>, GenericError> {
        // Label each endpoint so the I/O loop can route metrics to OPW and non-metrics to the normal primary.
        let mut endpoints = Vec::new();
        endpoints.push(RoutableEndpoint::new(
            EndpointRoute::Primary,
            self.endpoint.build_primary_endpoint(configuration.clone())?,
        ));

        if let Some(selected) = self.opw_metrics.selected_endpoint() {
            let trimmed_url = selected.url.trim();
            if trimmed_url.is_empty() {
                warn!(
                    enabled_key = selected.enabled_key,
                    url_key = selected.url_key,
                    "OPW/Vector metrics override is enabled, but no override URL was provided: override will be \
                     disabled. Continuing.",
                );
            } else {
                match self
                    .endpoint
                    .build_primary_endpoint_override(trimmed_url, configuration.clone())
                {
                    Ok(endpoint) => {
                        endpoints.push(RoutableEndpoint::new(EndpointRoute::MetricsPrimary, endpoint));
                    }
                    Err(e) => {
                        warn!(
                            enabled_key = selected.enabled_key,
                            url_key = selected.url_key,
                            url = trimmed_url,
                            error = %e,
                            "Failed to configure OPW/Vector metrics override URL: override will be disabled. Continuing.",
                        );
                    }
                }
            }
        }

        endpoints.extend(
            self.endpoint
                .build_additional_endpoints()?
                .into_iter()
                .map(|endpoint| RoutableEndpoint::new(EndpointRoute::Additional, endpoint)),
        );

        Ok(endpoints)
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

    /// Returns whether TLS certificate validation is disabled for Datadog intake forwarding.
    pub const fn skip_ssl_validation(&self) -> bool {
        self.skip_ssl_validation
    }

    /// Returns the minimum TLS protocol version for Datadog intake forwarding.
    pub const fn min_tls_version(&self) -> TlsMinimumVersion {
        self.parsed_min_tls_version
    }

    /// Returns whether outbound intake requests should allow arbitrary tag values.
    pub const fn allow_arbitrary_tags(&self) -> bool {
        self.allow_arbitrary_tags
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
    const DATADOG_URL: &str = "http://datadog.example.com";
    const DATADOG_URI: &str = "http://datadog.example.com/";
    const OPW_URL: &str = "http://opw.example.com:8080";
    const OPW_URI: &str = "http://opw.example.com:8080/";
    const VECTOR_URL: &str = "http://vector.example.com:8080";
    const VECTOR_URI: &str = "http://vector.example.com:8080/";
    const ADDITIONAL_URI: &str = "http://additional.example.com/";

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

    async fn generic_config_from(
        file_values: serde_json::Value, env_vars: Option<&[(String, String)]>,
    ) -> GenericConfiguration {
        let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
            Some(file_values),
            env_vars,
            false,
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await;
        cfg
    }

    fn endpoint_urls_by_route(config: &ForwarderConfiguration, route: EndpointRoute) -> Vec<String> {
        config
            .build_routable_endpoints(None)
            .expect("endpoints should resolve")
            .into_iter()
            .filter_map(|endpoint| {
                let (endpoint_route, endpoint) = endpoint.into_parts();
                (endpoint_route == route).then(|| endpoint.endpoint().to_string())
            })
            .collect()
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

    #[tokio::test]
    async fn forwarder_http_protocol_defaults_to_auto() {
        let config = forwarder_config_from(base_config(), None).await;

        assert_eq!(config.http_protocol(), saluki_io::net::client::http::HttpProtocol::Auto);
    }

    #[tokio::test]
    async fn forwarder_http_protocol_accepts_auto() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({ "forwarder_http_protocol": "auto" })),
            None,
        )
        .await;

        assert_eq!(config.http_protocol(), saluki_io::net::client::http::HttpProtocol::Auto);
    }

    #[tokio::test]
    async fn forwarder_http_protocol_accepts_http1() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({ "forwarder_http_protocol": "http1" })),
            None,
        )
        .await;

        assert_eq!(
            config.http_protocol(),
            saluki_io::net::client::http::HttpProtocol::Http1
        );
    }

    #[tokio::test]
    #[should_panic(expected = "ForwarderConfiguration should deserialize")]
    async fn forwarder_http_protocol_rejects_unknown_values() {
        let _ = forwarder_config_from(
            config_with(serde_json::json!({ "forwarder_http_protocol": "http2" })),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn skip_ssl_validation_defaults_to_false() {
        let config = forwarder_config_from(base_config(), None).await;

        assert!(!config.skip_ssl_validation());
    }

    #[tokio::test]
    async fn allow_arbitrary_tags_defaults_to_false() {
        let config = forwarder_config_from(base_config(), None).await;

        assert!(!config.allow_arbitrary_tags());
    }

    #[tokio::test]
    async fn allow_arbitrary_tags_set_via_yaml() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "allow_arbitrary_tags": true })), None).await;

        assert!(config.allow_arbitrary_tags());
    }

    #[tokio::test]
    async fn allow_arbitrary_tags_set_via_env_var() {
        // ALLOW_ARBITRARY_TAGS simulates DD_ALLOW_ARBITRARY_TAGS: the test helper sets
        // TEST_ALLOW_ARBITRARY_TAGS, which from_environment("TEST") reads as allow_arbitrary_tags.
        let env_vars = vec![("ALLOW_ARBITRARY_TAGS".to_string(), "true".to_string())];
        let config = forwarder_config_from(base_config(), Some(&env_vars)).await;

        assert!(config.allow_arbitrary_tags());
    }

    #[tokio::test]
    async fn skip_ssl_validation_set_via_yaml() {
        let config = forwarder_config_from(config_with(serde_json::json!({ "skip_ssl_validation": true })), None).await;

        assert!(config.skip_ssl_validation());
    }

    #[tokio::test]
    async fn skip_ssl_validation_set_via_env_var() {
        // SKIP_SSL_VALIDATION simulates DD_SKIP_SSL_VALIDATION: the test helper sets
        // TEST_SKIP_SSL_VALIDATION, which from_environment("TEST") reads as skip_ssl_validation.
        let env_vars = vec![("SKIP_SSL_VALIDATION".to_string(), "true".to_string())];
        let config = forwarder_config_from(base_config(), Some(&env_vars)).await;

        assert!(config.skip_ssl_validation());
    }

    #[tokio::test]
    async fn min_tls_version_defaults_to_tls12() {
        let config = forwarder_config_from(base_config(), None).await;

        assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls12);
    }

    #[tokio::test]
    async fn min_tls_version_tls12_uses_tls12() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "min_tls_version": "tlsv1.2" })), None).await;

        assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls12);
    }

    #[tokio::test]
    async fn min_tls_version_tls13_uses_tls13() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "min_tls_version": "tlsv1.3" })), None).await;

        assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls13);
    }

    #[tokio::test]
    async fn min_tls_version_is_case_insensitive() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "min_tls_version": "TlSv1.3" })), None).await;

        assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls13);
    }

    #[tokio::test]
    async fn min_tls_version_tls10_and_tls11_clamp_to_tls12() {
        for min_tls_version in ["tlsv1.0", "tlsv1.1"] {
            let config = forwarder_config_from(
                config_with(serde_json::json!({
                    "min_tls_version": min_tls_version,
                })),
                None,
            )
            .await;

            assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls12);
        }
    }

    #[tokio::test]
    async fn min_tls_version_invalid_value_falls_back_to_tls12() {
        let config =
            forwarder_config_from(config_with(serde_json::json!({ "min_tls_version": "tlsv1.9" })), None).await;

        assert_eq!(config.min_tls_version(), TlsMinimumVersion::Tls12);
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_overrides_metric_primary() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": OPW_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Primary),
            vec![DATADOG_URI]
        );
        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![OPW_URI]
        );
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_disabled_does_not_override_metric_primary() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": false,
                        "url": OPW_URL,
                    }
                },
                "vector": {
                    "metrics": {
                        "enabled": false,
                        "url": VECTOR_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Primary),
            vec![DATADOG_URI]
        );
        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[tokio::test]
    async fn vector_metrics_endpoint_is_legacy_fallback() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "vector": {
                    "metrics": {
                        "enabled": true,
                        "url": VECTOR_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![VECTOR_URI]
        );
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_takes_precedence_over_vector() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": OPW_URL,
                    }
                },
                "vector": {
                    "metrics": {
                        "enabled": true,
                        "url": VECTOR_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![OPW_URI]
        );
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_does_not_fallback_to_vector_when_opw_url_empty() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": "",
                    }
                },
                "vector": {
                    "metrics": {
                        "enabled": true,
                        "url": VECTOR_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_does_not_fallback_to_vector_when_opw_url_invalid() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": "http://[::1",
                    }
                },
                "vector": {
                    "metrics": {
                        "enabled": true,
                        "url": VECTOR_URL,
                    }
                }
            })),
            None,
        )
        .await;

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_keeps_dynamic_api_key_configuration() {
        let generic_config = generic_config_from(
            config_with(serde_json::json!({
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": OPW_URL,
                    }
                }
            })),
            None,
        )
        .await;
        let config =
            ForwarderConfiguration::from_configuration(&generic_config).expect("ForwarderConfiguration should parse");

        let endpoints = config
            .build_routable_endpoints(Some(generic_config))
            .expect("endpoints should resolve");
        let opw_endpoint = endpoints
            .iter()
            .find(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary)
            .expect("OPW endpoint should exist");

        assert!(opw_endpoint.endpoint().has_configuration());
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_preserves_additional_endpoints() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "dd_url": DATADOG_URL,
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": OPW_URL,
                    }
                },
                "additional_endpoints": {
                    "http://additional.example.com": ["extra-api-key"]
                }
            })),
            None,
        )
        .await;

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Additional),
            vec![ADDITIONAL_URI]
        );
    }
}

#[cfg(test)]
mod config_smoke {
    use serde_json::json;

    use super::ForwarderConfiguration;
    use crate::config_registry::structs;
    use crate::config_registry::test_support::run_config_smoke_tests;

    #[tokio::test]
    async fn smoke_test() {
        // `api_key` has no serde default (EndpointConfiguration::api_key: String), so
        // deserialization panics on an empty config. Supply it via base_config so every
        // config load in the smoke test has a valid starting point.
        run_config_smoke_tests(
            structs::FORWARDER_CONFIGURATION,
            &[],
            json!({ "api_key": "smoke-test-api-key" }),
            |cfg| ForwarderConfiguration::from_configuration(&cfg).expect("ForwarderConfiguration should deserialize"),
        )
        .await
    }
}

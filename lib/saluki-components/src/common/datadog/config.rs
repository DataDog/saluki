use std::time::Duration;

use facet::Facet;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use serde::Deserialize;
use tracing::warn;

use super::{
    default_serializer_compressor_kind,
    endpoints::{EndpointConfiguration, EndpointRoute, RoutableEndpoint},
    protocol::{UseV3ApiSeriesConfig, V3ApiConfig},
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
};

const fn default_endpoint_concurrency() -> usize {
    10
}

const fn default_endpoint_concurrency_multiplier() -> usize {
    1
}

const fn default_request_timeout_secs() -> u64 {
    20
}

const fn default_endpoint_buffer_size() -> usize {
    100
}

const fn default_forwarder_connection_reset_interval() -> u64 {
    0
}

const fn default_api_key_validation_interval_mins() -> u64 {
    60
}

const fn default_api_key_validation_interval_config_mins() -> i64 {
    default_api_key_validation_interval_mins() as i64
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

    /// Enables V3 series metrics when routing to Observability Pipelines Worker.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "observability_pipelines_worker_metrics_use_v3_api_series")]
    observability_pipelines_worker_use_v3_api_series: bool,

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

    /// Enables V3 series metrics when routing to Vector.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.use_v3_api.series`.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "vector_metrics_use_v3_api_series")]
    vector_use_v3_api_series: bool,
}

struct SelectedOpwMetricsEndpoint<'a> {
    enabled_key: &'static str,
    url_key: &'static str,
    url: &'a str,
    use_v3_series: bool,
}

impl OpwMetricsConfiguration {
    fn selected_endpoint(&self) -> Option<SelectedOpwMetricsEndpoint<'_>> {
        if self.observability_pipelines_worker_enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "observability_pipelines_worker.metrics.enabled",
                url_key: "observability_pipelines_worker.metrics.url",
                url: &self.observability_pipelines_worker_url,
                use_v3_series: self.observability_pipelines_worker_use_v3_api_series,
            });
        }

        if self.vector_enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "vector.metrics.enabled",
                url_key: "vector.metrics.url",
                url: &self.vector_url,
                use_v3_series: self.vector_use_v3_api_series,
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
    /// Defaults to 10. If set to 0, request concurrency is clamped to 1.
    #[serde(
        default = "default_endpoint_concurrency",
        rename = "forwarder_max_concurrent_requests"
    )]
    endpoint_concurrency: usize,

    /// Multiplier for endpoint request concurrency.
    ///
    /// Defaults to 1. This value also sizes the HTTP idle connection pool. If set to 0, idle connection retention is
    /// disabled and the concurrency multiplier is treated as 1. This setting does not create worker tasks.
    #[serde(
        default = "default_endpoint_concurrency_multiplier",
        rename = "forwarder_num_workers"
    )]
    endpoint_concurrency_multiplier: usize,

    /// Request timeout, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    ///
    /// Defaults to 100.
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

    /// Agent-compatible V3 API configuration for series metrics.
    #[serde(flatten)]
    use_v3_api_series: UseV3ApiSeriesConfig,

    /// ADP safety gate for authoritative V3 series metrics.
    ///
    /// Defaults to `false`. This keeps ADP on V2 unless both Agent-compatible V3 config and this ADP-specific flag
    /// enable V3.
    #[serde(default, rename = "data_plane_metrics_v3_series_enabled")]
    data_plane_metrics_v3_series_enabled: bool,

    /// Payload compressor kind used by the metrics serializer.
    ///
    /// V3 metrics intake is incompatible with zlib/deflate, so the forwarder needs this setting to keep endpoint
    /// filtering aligned with the encoder when zlib forces metrics back to V2.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    serializer_compressor_kind: String,

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// Defaults to `false`. If set to `true`, HTTPS clients built for the shared Datadog forwarder accept invalid
    /// server certificates. Only deployments that intentionally route Datadog intake traffic through endpoints with
    /// invalid or self-signed certificates should enable this.
    #[serde(default)]
    skip_ssl_validation: bool,

    /// File path to write TLS key material to for all HTTPS connections to the
    /// Datadog backend.
    ///
    /// When non-empty, enables the logging of TLS key material to the given file path,
    /// in the [NSS Key Log][nss_key_log] format, which can be used for debugging TLS
    /// issues, as well as decrypting captured TLS traffic in tools such as Wireshark.
    ///
    /// Defaults to empty.
    ///
    /// [nss_key_log]: https://nss-crypto.org/reference/security/nss/legacy/key_log_format/index.html
    #[serde(default)]
    sslkeylogfile: String,

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

    /// API key validation interval, in minutes.
    ///
    /// All values that are less than or equal to zero will be ignored, and the default
    /// value will be used.
    ///
    /// Defaults to 60 minutes.
    #[serde(
        default = "default_api_key_validation_interval_config_mins",
        rename = "forwarder_apikey_validation_interval"
    )]
    api_key_validation_interval_mins: i64,
}

impl ForwarderConfiguration {
    /// Creates a new `ForwarderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut forwarder_config = config.as_typed::<Self>()?;
        forwarder_config.parsed_min_tls_version = min_tls_version_from_config_value(&forwarder_config.min_tls_version);

        if forwarder_config.api_key_validation_interval_mins <= 0 {
            warn!(
                config_key = "forwarder_apikey_validation_interval",
                fallback_minutes = default_api_key_validation_interval_mins(),
                "Configured API key validation interval is invalid; using default."
            );
            forwarder_config.api_key_validation_interval_mins = default_api_key_validation_interval_mins() as i64;
        }

        // Handle fixing up the forwarder storage path if it's empty.
        forwarder_config.retry.fix_empty_storage_path(config);

        Ok(forwarder_config)
    }

    /// Returns the maximum number of concurrent requests for an individual endpoint.
    pub const fn endpoint_concurrency(&self) -> usize {
        let endpoint_concurrency = if self.endpoint_concurrency == 0 {
            1
        } else {
            self.endpoint_concurrency
        };
        let endpoint_concurrency_multiplier = if self.endpoint_concurrency_multiplier == 0 {
            1
        } else {
            self.endpoint_concurrency_multiplier
        };

        endpoint_concurrency.saturating_mul(endpoint_concurrency_multiplier)
    }

    /// Returns the maximum number of idle HTTP connections per host.
    pub const fn max_idle_connections_per_host(&self) -> usize {
        self.endpoint_concurrency_multiplier
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
                .build_additional_endpoints(configuration.clone())?
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

    /// Returns the Agent-compatible V3 series configuration.
    pub(crate) const fn use_v3_api_series(&self) -> &UseV3ApiSeriesConfig {
        &self.use_v3_api_series
    }

    /// Returns true when the ADP V3 series safety gate is enabled.
    pub(crate) const fn data_plane_metrics_v3_series_enabled(&self) -> bool {
        self.data_plane_metrics_v3_series_enabled
    }

    /// Returns the OPW/Vector V3 series override for metrics-primary routing, if configured.
    pub(crate) fn opw_metrics_v3_series_override(&self) -> Option<bool> {
        self.opw_metrics
            .selected_endpoint()
            .map(|selected| selected.use_v3_series)
    }

    /// Returns the configured primary endpoint string without resolving or version-prefixing it.
    pub(crate) fn primary_configured_endpoint(&self) -> String {
        self.endpoint.configured_primary_endpoint()
    }

    /// Returns whether the configured metrics compressor is incompatible with Metrics V3.
    pub(crate) fn compressor_disables_metrics_v3(&self) -> bool {
        self.serializer_compressor_kind.trim().eq_ignore_ascii_case("zlib")
    }

    /// Returns whether TLS certificate validation is disabled for Datadog intake forwarding.
    pub const fn skip_ssl_validation(&self) -> bool {
        self.skip_ssl_validation
    }

    /// Returns the TLS key log file path, if configured.
    pub fn ssl_key_log_file_path(&self) -> Option<&str> {
        let trimmed = self.sslkeylogfile.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    /// Returns the minimum TLS protocol version for Datadog intake forwarding.
    pub const fn min_tls_version(&self) -> TlsMinimumVersion {
        self.parsed_min_tls_version
    }

    /// Returns whether outbound intake requests should allow arbitrary tag values.
    pub const fn allow_arbitrary_tags(&self) -> bool {
        self.allow_arbitrary_tags
    }

    /// Overrides whether outbound requests should signal support for arbitrary tag values.
    pub fn with_allow_arbitrary_tags(mut self, allow_arbitrary_tags: bool) -> Self {
        self.allow_arbitrary_tags = allow_arbitrary_tags;
        self
    }

    /// Returns the API key validation interval.
    pub const fn api_key_validation_interval(&self) -> Duration {
        Duration::from_mins(self.api_key_validation_interval_mins as u64)
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
    const SSL_KEY_LOG_FILE_PATH: &str = "/tmp/saluki-sslkeylogfile";

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
    async fn endpoint_concurrency_multiplies_and_clamps_zero_values_to_one() {
        // `endpoint_concurrency` multiplies the base concurrency by the multiplier, but a documented zero
        // for either field is clamped to 1 first (a zero multiplier "is treated as 1", a zero base
        // concurrency "is clamped to 1").
        let cases = [
            ("both configured", 3usize, 4usize, 12usize),
            ("zero base concurrency clamps to one", 0, 5, 5),
            ("zero multiplier clamps to one", 10, 0, 10),
            ("both zero clamp to one", 0, 0, 1),
        ];

        for (name, concurrency, multiplier, expected) in cases {
            let config = forwarder_config_from(
                config_with(serde_json::json!({
                    "forwarder_max_concurrent_requests": concurrency,
                    "forwarder_num_workers": multiplier,
                })),
                None,
            )
            .await;

            assert_eq!(config.endpoint_concurrency(), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn api_key_validation_interval_parsing() {
        let cases = [
            ("missing", serde_json::json!({}), Duration::from_mins(60)),
            (
                "positive",
                serde_json::json!({ "forwarder_apikey_validation_interval": 5i64 }),
                Duration::from_mins(5),
            ),
            (
                "zero",
                serde_json::json!({ "forwarder_apikey_validation_interval": 0i64 }),
                Duration::from_mins(60),
            ),
            (
                "negative",
                serde_json::json!({ "forwarder_apikey_validation_interval": -1i64 }),
                Duration::from_mins(60),
            ),
        ];

        for (case_name, extra_config, expected_interval) in cases {
            let config = forwarder_config_from(config_with(extra_config), None).await;
            assert_eq!(config.api_key_validation_interval(), expected_interval, "{case_name}");
        }
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
    async fn sslkeylogfile_set_via_yaml() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({ "sslkeylogfile": SSL_KEY_LOG_FILE_PATH })),
            None,
        )
        .await;

        assert_eq!(config.ssl_key_log_file_path(), Some(SSL_KEY_LOG_FILE_PATH));
    }

    #[tokio::test]
    async fn sslkeylogfile_set_via_env_var() {
        // SSLKEYLOGFILE simulates DD_SSLKEYLOGFILE: the test helper sets TEST_SSLKEYLOGFILE, which
        // from_environment("TEST") reads as sslkeylogfile.
        let env_vars = vec![("SSLKEYLOGFILE".to_string(), SSL_KEY_LOG_FILE_PATH.to_string())];
        let config = forwarder_config_from(base_config(), Some(&env_vars)).await;

        assert_eq!(config.ssl_key_log_file_path(), Some(SSL_KEY_LOG_FILE_PATH));
    }

    #[tokio::test]
    async fn sslkeylogfile_defaults_to_none_when_unset() {
        // The field defaults to empty, and `ssl_key_log_file_path` reports an empty path as "not configured".
        let config = forwarder_config_from(base_config(), None).await;

        assert_eq!(config.ssl_key_log_file_path(), None);
    }

    #[tokio::test]
    async fn sslkeylogfile_whitespace_only_is_treated_as_unset() {
        // `ssl_key_log_file_path` trims the configured value, so a whitespace-only path is reported as unset.
        let config = forwarder_config_from(config_with(serde_json::json!({ "sslkeylogfile": "   " })), None).await;

        assert_eq!(config.ssl_key_log_file_path(), None);
    }

    #[tokio::test]
    async fn min_tls_version_maps_configured_value() {
        // Documented mapping (see `min_tls_version_from_config_value`): the default and an explicit
        // tlsv1.2 map to TLS 1.2; tlsv1.3 maps to TLS 1.3 (case-insensitively); tlsv1.0/tlsv1.1 clamp up
        // to TLS 1.2 because rustls has no older support; and an empty string or any unrecognized value
        // falls back to TLS 1.2.
        let cases = [
            ("default when unset", None, TlsMinimumVersion::Tls12),
            ("explicit tlsv1.2", Some("tlsv1.2"), TlsMinimumVersion::Tls12),
            ("tlsv1.3", Some("tlsv1.3"), TlsMinimumVersion::Tls13),
            ("case-insensitive tlsv1.3", Some("TlSv1.3"), TlsMinimumVersion::Tls13),
            ("tlsv1.0 clamps up", Some("tlsv1.0"), TlsMinimumVersion::Tls12),
            ("tlsv1.1 clamps up", Some("tlsv1.1"), TlsMinimumVersion::Tls12),
            ("empty string falls back", Some(""), TlsMinimumVersion::Tls12),
            (
                "unrecognized value falls back",
                Some("tlsv1.9"),
                TlsMinimumVersion::Tls12,
            ),
        ];

        for (name, value, expected) in cases {
            let file_values = match value {
                Some(value) => config_with(serde_json::json!({ "min_tls_version": value })),
                None => base_config(),
            };
            let config = forwarder_config_from(file_values, None).await;

            assert_eq!(config.min_tls_version(), expected, "{name}");
        }
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
    async fn use_v3_api_series_config_loads_agent_nested_yaml_shape() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "use_v3_api": {
                    "series": {
                        "enabled": "datadog_only",
                        "endpoints": {
                            "http://datadog.example.com": false
                        }
                    }
                },
                "data_plane": {
                    "metrics": {
                        "v3": {
                            "series": {
                                "enabled": true
                            }
                        }
                    }
                }
            })),
            None,
        )
        .await;

        assert!(config.data_plane_metrics_v3_series_enabled());
        assert_eq!(config.use_v3_api_series().enabled, "datadog_only");
        assert_eq!(
            config.use_v3_api_series().endpoints.get(DATADOG_URL),
            Some(&"false".to_string())
        );
    }

    #[tokio::test]
    async fn serializer_compressor_kind_zlib_disables_metrics_v3() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "serializer_compressor_kind": "zlib",
            })),
            None,
        )
        .await;

        assert!(config.compressor_disables_metrics_v3());

        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "serializer_compressor_kind": "zstd",
            })),
            None,
        )
        .await;

        assert!(!config.compressor_disables_metrics_v3());
    }

    #[tokio::test]
    async fn opw_metrics_v3_series_override_loads_agent_nested_yaml_shape() {
        let config = forwarder_config_from(
            config_with(serde_json::json!({
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": OPW_URL,
                        "use_v3_api": {
                            "series": true
                        }
                    }
                }
            })),
            None,
        )
        .await;

        assert_eq!(config.opw_metrics_v3_series_override(), Some(true));
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
    async fn additional_endpoints_keep_dynamic_api_key_configuration() {
        let generic_config = generic_config_from(
            config_with(serde_json::json!({
                "additional_endpoints": {
                    "http://additional.example.com": ["extra-api-key"]
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
        let additional = endpoints
            .iter()
            .find(|e| e.route() == EndpointRoute::Additional)
            .expect("additional endpoint should exist");

        assert!(
            additional.endpoint().has_configuration(),
            "additional endpoint should hold a live config reference"
        );
        assert!(
            additional.endpoint().has_api_key_index(),
            "additional endpoint should have an api_key_index"
        );
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
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::ForwarderConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        // `api_key` has no serde default (EndpointConfiguration::api_key: String), so
        // deserialization panics on an empty config. Supply it via base_config so every
        // config load in the smoke test has a valid starting point.
        run_config_smoke_tests(
            structs::FORWARDER_CONFIGURATION,
            &[
                "serializer_experimental_use_v3_api.sketches.beta_route",
                "serializer_experimental_use_v3_api.sketches.shadow_sample_rate",
                "serializer_experimental_use_v3_api.sketches.shadow_sites",
                "serializer_experimental_use_v3_api.sketches.use_beta",
            ],
            json!({ "api_key": "smoke-test-api-key" }),
            |cfg| ForwarderConfiguration::from_configuration(&cfg).expect("ForwarderConfiguration should deserialize"),
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}

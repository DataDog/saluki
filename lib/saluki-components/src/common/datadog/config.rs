use std::time::Duration;

use agent_data_plane_config::{shared::SharedConfiguration, Live, SalukiConfiguration};
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use tracing::warn;

use super::{
    endpoints::{EndpointConfiguration, EndpointRoute, RoutableEndpoint},
    protocol::{UseV3ApiSeriesConfig, V3ApiConfig, V3ApiSettings},
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
};

/// Fallback used when the configured API key validation interval is non-positive.
const DEFAULT_API_KEY_VALIDATION_INTERVAL_MINS: i64 = 60;

const MIN_TLS_VERSION_TLS10: &str = "tlsv1.0";
const MIN_TLS_VERSION_TLS11: &str = "tlsv1.1";
const MIN_TLS_VERSION_TLS12: &str = "tlsv1.2";
const MIN_TLS_VERSION_TLS13: &str = "tlsv1.3";

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

fn v3_api_config_from_model(v3_api: &agent_data_plane_config::shared::V3ApiEncoding) -> V3ApiConfig {
    V3ApiConfig {
        series: v3_api_settings_from_model(&v3_api.series),
        sketches: v3_api_settings_from_model(&v3_api.sketches),
        compression_level: v3_api.compression_level,
    }
}

fn v3_api_settings_from_model(settings: &agent_data_plane_config::shared::V3ApiSettings) -> V3ApiSettings {
    V3ApiSettings {
        endpoints: settings.endpoints.clone(),
        validate: settings.validate,
        use_beta: settings.use_beta,
        beta_route: settings.beta_route.clone(),
        shadow_sample_rate: settings.shadow_sample_rate,
        shadow_sites: settings.shadow_sites.clone(),
    }
}

/// HTTP protocol selection for the Datadog forwarder.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
#[derive(Clone, Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct OpwMetricsConfiguration {
    /// Enables routing all metrics to Observability Pipelines Worker.
    observability_pipelines_worker_enabled: bool,

    /// Endpoint of the Observability Pipelines Worker instance to route metrics to.
    observability_pipelines_worker_url: String,

    /// Enables V3 series metrics when routing to Observability Pipelines Worker.
    observability_pipelines_worker_use_v3_api_series: bool,

    /// Enables routing all metrics to Vector.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.enabled`.
    vector_enabled: bool,

    /// Endpoint of the Vector instance to route metrics to.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.url`.
    vector_url: String,

    /// Enables V3 series metrics when routing to Vector.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.use_v3_api.series`.
    vector_use_v3_api_series: bool,
}

impl OpwMetricsConfiguration {
    fn from_model(
        opw: &agent_data_plane_config::shared::AltMetricsIntake,
        vector: &agent_data_plane_config::shared::AltMetricsIntake,
    ) -> Self {
        Self {
            observability_pipelines_worker_enabled: opw.enabled,
            observability_pipelines_worker_url: opw.url.clone(),
            observability_pipelines_worker_use_v3_api_series: opw.use_v3_series,
            vector_enabled: vector.enabled,
            vector_url: vector.url.clone(),
            vector_use_v3_api_series: vector.use_v3_series,
        }
    }
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
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct ForwarderConfiguration {
    /// Maximum number of concurrent requests for an individual endpoint. If set to 0, request
    /// concurrency is clamped to 1.
    endpoint_concurrency: usize,

    /// Multiplier for endpoint request concurrency. This value also sizes the HTTP idle connection
    /// pool. If set to 0, idle connection retention is disabled and the concurrency multiplier is
    /// treated as 1. This setting does not create worker tasks.
    endpoint_concurrency_multiplier: usize,

    /// Request timeout, in seconds.
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    endpoint_buffer_size: usize,

    /// Endpoint configuration.
    pub(crate) endpoint: EndpointConfiguration,

    /// Retry configuration.
    retry: RetryConfiguration,

    /// Proxy configuration.
    proxy: ProxyConfiguration,

    /// OPW metrics routing configuration.
    opw_metrics: OpwMetricsConfiguration,

    /// HTTP protocol selection for outgoing forwarder requests.
    http_protocol: ForwarderHttpProtocol,

    /// Connection reset interval, in seconds.
    connection_reset_interval_secs: u64,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// This is read from the encoder configuration and used by the I/O layer to filter payloads
    /// based on endpoint URL matching.
    v3_api: V3ApiConfig,

    /// Agent-compatible V3 API configuration for series metrics.
    use_v3_api_series: UseV3ApiSeriesConfig,

    /// ADP safety gate for authoritative V3 series metrics.
    ///
    /// This keeps ADP on V2 unless both Agent-compatible V3 config and this ADP-specific flag
    /// enable V3.
    data_plane_metrics_v3_series_enabled: bool,

    /// Payload compressor kind used by the metrics serializer.
    ///
    /// V3 metrics intake is incompatible with zlib/deflate, so the forwarder needs this setting to keep endpoint
    /// filtering aligned with the encoder when zlib forces metrics back to V2.
    serializer_compressor_kind: String,

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// If set to `true`, HTTPS clients built for the shared Datadog forwarder accept invalid
    /// server certificates. Only deployments that intentionally route Datadog intake traffic through endpoints with
    /// invalid or self-signed certificates should enable this.
    skip_ssl_validation: bool,

    /// File path to write TLS key material to for all HTTPS connections to the
    /// Datadog backend.
    ///
    /// When non-empty, enables the logging of TLS key material to the given file path,
    /// in the [NSS Key Log][nss_key_log] format, which can be used for debugging TLS
    /// issues, as well as decrypting captured TLS traffic in tools such as Wireshark.
    ///
    /// [nss_key_log]: https://nss-crypto.org/reference/security/nss/legacy/key_log_format/index.html
    sslkeylogfile: String,

    /// Parsed minimum TLS protocol version for Datadog intake forwarding.
    ///
    /// TLS 1.0 and TLS 1.1 are accepted for compatibility with core Agent configuration, but Saluki
    /// clamps them to TLS 1.2 because rustls does not support older protocol versions.
    parsed_min_tls_version: TlsMinimumVersion,

    /// Whether to signal that the backend should allow arbitrary tag values.
    ///
    /// If set to `true`, the Datadog forwarder adds `Allow-Arbitrary-Tag-Value: true` to every
    /// outbound intake request. The data plane does not perform local tag validation based on this setting.
    allow_arbitrary_tags: bool,

    /// API key validation interval, in minutes.
    api_key_validation_interval_mins: i64,
}

impl ForwarderConfiguration {
    /// Builds forwarder configuration from the shared configuration model.
    pub fn from_model(shared: &SharedConfiguration) -> Result<Self, GenericError> {
        let endpoints = &shared.endpoints;
        let forwarder = &endpoints.forwarder;
        let metrics_encoding = &shared.metrics_encoding;

        let api_key_validation_interval_mins = if forwarder.apikey_validation_interval <= 0 {
            warn!(
                config_key = "forwarder_apikey_validation_interval",
                fallback_minutes = DEFAULT_API_KEY_VALIDATION_INTERVAL_MINS,
                "Configured API key validation interval is invalid; using default."
            );
            DEFAULT_API_KEY_VALIDATION_INTERVAL_MINS
        } else {
            forwarder.apikey_validation_interval
        };

        let http_protocol = match forwarder.http_protocol {
            agent_data_plane_config::shared::ForwarderHttpProtocol::Auto => ForwarderHttpProtocol::Auto,
            agent_data_plane_config::shared::ForwarderHttpProtocol::Http1 => ForwarderHttpProtocol::Http1,
        };

        Ok(Self {
            endpoint_concurrency: forwarder.max_concurrent_requests,
            endpoint_concurrency_multiplier: forwarder.num_workers,
            request_timeout_secs: forwarder.timeout,
            endpoint_buffer_size: forwarder.high_prio_buffer_size,
            endpoint: EndpointConfiguration::from_model(endpoints),
            retry: RetryConfiguration::from_model(forwarder, &shared.run_path),
            proxy: ProxyConfiguration::from_model(&endpoints.proxy),
            opw_metrics: OpwMetricsConfiguration::from_model(&endpoints.opw_intake, &endpoints.vector_intake),
            http_protocol,
            connection_reset_interval_secs: forwarder.connection_reset_interval,
            v3_api: v3_api_config_from_model(&metrics_encoding.v3_api),
            use_v3_api_series: UseV3ApiSeriesConfig {
                enabled: metrics_encoding.v3_series_mode.mode.clone(),
                endpoints: metrics_encoding.v3_series_mode.endpoint_modes.clone(),
            },
            data_plane_metrics_v3_series_enabled: metrics_encoding.v3_series_enabled,
            serializer_compressor_kind: endpoints.compression.compressor_kind.clone(),
            skip_ssl_validation: endpoints.tls.skip_ssl_validation,
            sslkeylogfile: endpoints.tls.sslkeylogfile.clone(),
            parsed_min_tls_version: min_tls_version_from_config_value(&endpoints.tls.min_tls_version),
            allow_arbitrary_tags: endpoints.allow_arbitrary_tags,
            api_key_validation_interval_mins,
        })
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
        &self, live: Option<Live<SalukiConfiguration>>,
    ) -> Result<Vec<RoutableEndpoint>, GenericError> {
        // Label each endpoint so the I/O loop can route metrics to OPW and non-metrics to the normal primary.
        let mut endpoints = Vec::new();
        endpoints.push(RoutableEndpoint::new(
            EndpointRoute::Primary,
            self.endpoint.build_primary_endpoint(live.clone())?,
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
                match self.endpoint.build_primary_endpoint_override(trimmed_url, live.clone()) {
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
                .build_additional_endpoints(live)?
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
    pub const fn proxy(&self) -> &ProxyConfiguration {
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
    use agent_data_plane_config::shared::{ForwarderHttpProtocol as ModelHttpProtocol, SharedConfiguration};
    use agent_data_plane_config::{Live, SalukiConfiguration};

    use super::*;

    const DATADOG_URL: &str = "http://datadog.example.com";
    const DATADOG_URI: &str = "http://datadog.example.com/";
    const OPW_URL: &str = "http://opw.example.com:8080";
    const OPW_URI: &str = "http://opw.example.com:8080/";
    const VECTOR_URL: &str = "http://vector.example.com:8080";
    const VECTOR_URI: &str = "http://vector.example.com:8080/";
    const ADDITIONAL_URI: &str = "http://additional.example.com/";
    const SSL_KEY_LOG_FILE_PATH: &str = "/tmp/saluki-sslkeylogfile";

    /// Builds a forwarder config from a default shared model with a valid API key, after applying
    /// the given tweaks.
    fn forwarder_config(configure: impl FnOnce(&mut SharedConfiguration)) -> ForwarderConfiguration {
        let mut shared = SharedConfiguration::default();
        shared.endpoints.api_key = "test-api-key".to_string();
        configure(&mut shared);
        ForwarderConfiguration::from_model(&shared).expect("forwarder config should build")
    }

    fn base_config() -> ForwarderConfiguration {
        forwarder_config(|_| {})
    }

    /// A shared model with an API key, built into both a forwarder config and a fixed live view so
    /// resolved endpoints can be inspected for their runtime refresh wiring.
    fn config_and_live(
        configure: impl FnOnce(&mut SharedConfiguration),
    ) -> (ForwarderConfiguration, Live<SalukiConfiguration>) {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = "test-api-key".to_string();
        configure(&mut config.shared);
        let forwarder = ForwarderConfiguration::from_model(&config.shared).expect("forwarder config should build");
        (forwarder, Live::fixed(config))
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

    #[test]
    fn http_protocol_defaults_to_auto() {
        assert_eq!(base_config().http_protocol(), HttpProtocol::Auto);
    }

    #[test]
    fn http_protocol_maps_http1() {
        let config = forwarder_config(|shared| shared.endpoints.forwarder.http_protocol = ModelHttpProtocol::Http1);
        assert_eq!(config.http_protocol(), HttpProtocol::Http1);
    }

    #[test]
    fn endpoint_concurrency_uses_configured_multiplier() {
        let config = forwarder_config(|shared| {
            shared.endpoints.forwarder.max_concurrent_requests = 3;
            shared.endpoints.forwarder.num_workers = 4;
        });
        assert_eq!(config.endpoint_concurrency(), 12);
    }

    #[test]
    fn endpoint_concurrency_clamps_zero_to_one() {
        // Default model values are 0/0, which clamp to 1 each.
        assert_eq!(base_config().endpoint_concurrency(), 1);
    }

    #[test]
    fn api_key_validation_interval_clamps_non_positive() {
        // The default model value (0) is non-positive, so the fallback of 60 minutes is used.
        assert_eq!(base_config().api_key_validation_interval(), Duration::from_mins(60));

        let config = forwarder_config(|shared| shared.endpoints.forwarder.apikey_validation_interval = 5);
        assert_eq!(config.api_key_validation_interval(), Duration::from_mins(5));

        let config = forwarder_config(|shared| shared.endpoints.forwarder.apikey_validation_interval = -1);
        assert_eq!(config.api_key_validation_interval(), Duration::from_mins(60));
    }

    #[test]
    fn skip_ssl_validation_passthrough() {
        assert!(!base_config().skip_ssl_validation());
        assert!(forwarder_config(|shared| shared.endpoints.tls.skip_ssl_validation = true).skip_ssl_validation());
    }

    #[test]
    fn allow_arbitrary_tags_passthrough_and_override() {
        assert!(!base_config().allow_arbitrary_tags());
        assert!(forwarder_config(|shared| shared.endpoints.allow_arbitrary_tags = true).allow_arbitrary_tags());
        assert!(base_config().with_allow_arbitrary_tags(true).allow_arbitrary_tags());
        assert!(!base_config().with_allow_arbitrary_tags(false).allow_arbitrary_tags());
    }

    #[test]
    fn sslkeylogfile_passthrough() {
        assert_eq!(base_config().ssl_key_log_file_path(), None);
        let config = forwarder_config(|shared| shared.endpoints.tls.sslkeylogfile = SSL_KEY_LOG_FILE_PATH.to_string());
        assert_eq!(config.ssl_key_log_file_path(), Some(SSL_KEY_LOG_FILE_PATH));
    }

    #[test]
    fn min_tls_version_parsing() {
        fn min_tls(value: &str) -> TlsMinimumVersion {
            forwarder_config(|shared| shared.endpoints.tls.min_tls_version = value.to_string()).min_tls_version()
        }

        // The default model value is empty, which resolves to TLS 1.2.
        assert_eq!(base_config().min_tls_version(), TlsMinimumVersion::Tls12);
        assert_eq!(min_tls("tlsv1.2"), TlsMinimumVersion::Tls12);
        assert_eq!(min_tls("tlsv1.3"), TlsMinimumVersion::Tls13);
        assert_eq!(min_tls("TlSv1.3"), TlsMinimumVersion::Tls13);
        // TLS 1.0 and 1.1 clamp up to 1.2; invalid values fall back to 1.2.
        assert_eq!(min_tls("tlsv1.0"), TlsMinimumVersion::Tls12);
        assert_eq!(min_tls("tlsv1.1"), TlsMinimumVersion::Tls12);
        assert_eq!(min_tls("tlsv1.9"), TlsMinimumVersion::Tls12);
    }

    #[test]
    fn serializer_compressor_kind_zlib_disables_metrics_v3() {
        assert!(
            forwarder_config(|shared| shared.endpoints.compression.compressor_kind = "zlib".to_string())
                .compressor_disables_metrics_v3()
        );
        assert!(
            !forwarder_config(|shared| shared.endpoints.compression.compressor_kind = "zstd".to_string())
                .compressor_disables_metrics_v3()
        );
    }

    #[test]
    fn v3_series_config_maps_from_model() {
        let config = forwarder_config(|shared| {
            shared.metrics_encoding.v3_series_enabled = true;
            shared.metrics_encoding.v3_series_mode.mode = "datadog_only".to_string();
            shared.metrics_encoding.v3_series_mode.endpoint_modes =
                [(DATADOG_URL.to_string(), "false".to_string())].into_iter().collect();
        });

        assert!(config.data_plane_metrics_v3_series_enabled());
        assert_eq!(config.use_v3_api_series().enabled, "datadog_only");
        assert_eq!(
            config.use_v3_api_series().endpoints.get(DATADOG_URL),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn opw_metrics_v3_series_override_maps_from_model() {
        let config = forwarder_config(|shared| {
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
            shared.endpoints.opw_intake.use_v3_series = true;
        });
        assert_eq!(config.opw_metrics_v3_series_override(), Some(true));
    }

    #[test]
    fn opw_metrics_endpoint_overrides_metric_primary() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
        });

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Primary),
            vec![DATADOG_URI]
        );
        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![OPW_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_disabled_does_not_override_metric_primary() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
            shared.endpoints.vector_intake.url = VECTOR_URL.to_string();
        });

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Primary),
            vec![DATADOG_URI]
        );
        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn vector_metrics_endpoint_is_legacy_fallback() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.vector_intake.enabled = true;
            shared.endpoints.vector_intake.url = VECTOR_URL.to_string();
        });

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![VECTOR_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_takes_precedence_over_vector() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
            shared.endpoints.vector_intake.enabled = true;
            shared.endpoints.vector_intake.url = VECTOR_URL.to_string();
        });

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![OPW_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_does_not_override_when_url_empty() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.vector_intake.enabled = true;
            shared.endpoints.vector_intake.url = VECTOR_URL.to_string();
        });

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn opw_metrics_endpoint_does_not_override_when_url_invalid() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = "http://[::1".to_string();
            shared.endpoints.vector_intake.enabled = true;
            shared.endpoints.vector_intake.url = VECTOR_URL.to_string();
        });

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn opw_metrics_endpoint_preserves_additional_endpoints() {
        let config = forwarder_config(|shared| {
            shared.endpoints.dd_url = Some(DATADOG_URL.to_string());
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
            shared.endpoints.additional_endpoints = [(
                "http://additional.example.com".to_string(),
                vec!["extra-api-key".to_string()],
            )]
            .into_iter()
            .collect();
        });

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Additional),
            vec![ADDITIONAL_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_keeps_dynamic_api_key_configuration() {
        let (forwarder, live) = config_and_live(|shared| {
            shared.endpoints.opw_intake.enabled = true;
            shared.endpoints.opw_intake.url = OPW_URL.to_string();
        });

        let endpoints = forwarder
            .build_routable_endpoints(Some(live))
            .expect("endpoints should resolve");
        let opw = endpoints
            .iter()
            .find(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary)
            .expect("OPW endpoint should exist");

        assert!(opw.endpoint().has_configuration());
    }

    #[test]
    fn additional_endpoints_keep_dynamic_api_key_configuration() {
        let (forwarder, live) = config_and_live(|shared| {
            shared.endpoints.additional_endpoints = [(
                "http://additional.example.com".to_string(),
                vec!["extra-api-key".to_string()],
            )]
            .into_iter()
            .collect();
        });

        let endpoints = forwarder
            .build_routable_endpoints(Some(live))
            .expect("endpoints should resolve");
        let additional = endpoints
            .iter()
            .find(|endpoint| endpoint.route() == EndpointRoute::Additional)
            .expect("additional endpoint should exist");

        assert!(additional.endpoint().has_configuration());
        assert!(additional.endpoint().has_api_key_index());
    }
}

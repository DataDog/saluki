use std::{collections::HashMap, time::Duration};

use agent_data_plane_config::shared::{self, Endpoints, SharedConfiguration, V3SeriesMode};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use tracing::warn;

use super::{
    endpoints::{EndpointConfiguration, EndpointRoute, RoutableEndpoint, SingleDestination},
    protocol::{UseV3ApiConfig, UseV3ApiSeriesConfig, V3ApiConfig},
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
};

const fn default_api_key_validation_interval_mins() -> i64 {
    60
}

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

/// Returns the API key validation interval, falling back to the default for a non-positive value.
fn api_key_validation_interval(configured_mins: i64) -> Duration {
    if configured_mins <= 0 {
        warn!(
            config_key = "forwarder_apikey_validation_interval",
            fallback_minutes = default_api_key_validation_interval_mins(),
            "Configured API key validation interval is invalid; using default."
        );
        return Duration::from_mins(default_api_key_validation_interval_mins() as u64);
    }

    Duration::from_mins(configured_mins as u64)
}

/// HTTP protocol selection for the Datadog forwarder.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ForwarderHttpProtocol {
    /// Automatically negotiate HTTP/2 with HTTP/1.1 fallback.
    #[default]
    Auto,

    /// Use HTTP/1.1 only.
    Http1,
}

impl From<shared::ForwarderHttpProtocol> for ForwarderHttpProtocol {
    fn from(protocol: shared::ForwarderHttpProtocol) -> Self {
        match protocol {
            shared::ForwarderHttpProtocol::Auto => Self::Auto,
            shared::ForwarderHttpProtocol::Http1 => Self::Http1,
        }
    }
}

impl From<ForwarderHttpProtocol> for HttpProtocol {
    fn from(protocol: ForwarderHttpProtocol) -> Self {
        match protocol {
            ForwarderHttpProtocol::Auto => Self::Auto,
            ForwarderHttpProtocol::Http1 => Self::Http1,
        }
    }
}

/// Metrics routing to an alternate intake, which replaces the Datadog metrics intake when enabled.
///
/// Two alternate intakes exist, the Observability Pipelines Worker and its deprecated Vector
/// predecessor, and the Worker takes precedence when both are enabled.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct OpwMetricsConfiguration {
    /// Observability Pipelines Worker routing settings.
    observability_pipelines_worker: OpwMetricsSettings,

    /// Vector routing settings, deprecated in favor of the Observability Pipelines Worker.
    vector: OpwMetricsSettings,
}

/// One alternate intake's routing settings.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct OpwMetricsSettings {
    /// Whether all metrics route to this intake.
    enabled: bool,

    /// Endpoint of the instance to route metrics to.
    url: String,

    /// Whether series metrics routed to this intake use the V3 protocol.
    use_v3_series: bool,
}

impl OpwMetricsConfiguration {
    /// Creates a new `OpwMetricsConfiguration` from the resolved endpoint configuration.
    pub(crate) fn from_configuration(endpoints: &Endpoints) -> Self {
        Self {
            observability_pipelines_worker: OpwMetricsSettings {
                enabled: endpoints.opw_intake.enabled,
                url: endpoints.opw_intake.url.clone(),
                use_v3_series: endpoints.opw_intake.use_v3_series,
            },
            vector: OpwMetricsSettings {
                enabled: endpoints.vector_intake.enabled,
                url: endpoints.vector_intake.url.clone(),
                use_v3_series: endpoints.vector_intake.use_v3_series,
            },
        }
    }

    /// Disables routing to both alternate intakes.
    pub(crate) fn disable(&mut self) {
        self.observability_pipelines_worker.enabled = false;
        self.vector.enabled = false;
    }

    /// Clears each alternate intake's V3 series routing, leaving routing itself untouched.
    pub(crate) fn clear_v3_series_overrides(&mut self) {
        self.observability_pipelines_worker.use_v3_series = false;
        self.vector.use_v3_series = false;
    }
}

pub(crate) struct SelectedOpwMetricsEndpoint<'a> {
    enabled_key: &'static str,
    url_key: &'static str,
    pub(crate) url: &'a str,
    pub(crate) use_v3_series: bool,
}

impl OpwMetricsConfiguration {
    pub(crate) fn selected_endpoint(&self) -> Option<SelectedOpwMetricsEndpoint<'_>> {
        if self.observability_pipelines_worker.enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "observability_pipelines_worker.metrics.enabled",
                url_key: "observability_pipelines_worker.metrics.url",
                url: &self.observability_pipelines_worker.url,
                use_v3_series: self.observability_pipelines_worker.use_v3_series,
            });
        }

        if self.vector.enabled {
            return Some(SelectedOpwMetricsEndpoint {
                enabled_key: "vector.metrics.enabled",
                url_key: "vector.metrics.url",
                url: &self.vector.url,
                use_v3_series: self.vector.use_v3_series,
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
    /// Maximum number of concurrent requests for an individual endpoint.
    ///
    /// If set to 0, request concurrency is clamped to 1.
    endpoint_concurrency: usize,

    /// Multiplier for endpoint request concurrency.
    ///
    /// This value also sizes the HTTP idle connection pool. If set to 0, idle connection retention is
    /// disabled and the concurrency multiplier is treated as 1. This setting does not create worker tasks.
    endpoint_concurrency_multiplier: usize,

    /// Request timeout, in seconds.
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    endpoint_buffer_size: usize,

    /// Endpoints payloads are sent to.
    endpoint: EndpointConfiguration,

    /// Retry configuration.
    retry: RetryConfiguration,

    /// Proxy configuration.
    proxy: ProxyConfiguration,

    /// Metrics routing to an alternate intake.
    opw_metrics: OpwMetricsConfiguration,

    /// HTTP protocol selection for outgoing forwarder requests.
    ///
    /// `auto` negotiates HTTP/2 with HTTP/1.1 fallback; `http1` forces HTTP/1.1 only.
    http_protocol: ForwarderHttpProtocol,

    /// Connection reset interval, in seconds.
    connection_reset_interval_secs: u64,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// This is shared with the metrics encoder and used by the I/O layer to filter payloads based on
    /// endpoint URL matching.
    v3_api: V3ApiConfig,

    /// Agent-compatible V3 API configuration.
    use_v3_api: UseV3ApiConfig,

    /// Payload compressor kind used by the metrics serializer.
    ///
    /// V3 metrics intake is incompatible with zlib/deflate, so the forwarder needs this setting to keep endpoint
    /// filtering aligned with the encoder when zlib forces metrics back to V2.
    serializer_compressor_kind: String,

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// When set, HTTPS clients built for the shared Datadog forwarder accept invalid server certificates. Only
    /// deployments that intentionally route Datadog intake traffic through endpoints with invalid or self-signed
    /// certificates should enable this.
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

    /// Minimum TLS protocol version for Datadog intake forwarding.
    ///
    /// TLS 1.0 and TLS 1.1 are accepted for compatibility with core Agent configuration, but Saluki clamps them to
    /// TLS 1.2 because rustls does not support older protocol versions.
    min_tls_version: TlsMinimumVersion,

    /// Timeout for completing the TLS handshake after a connection is established, for Datadog intake forwarding.
    ///
    /// This bounds only the TLS handshake step, distinct from `forwarder_timeout`, which bounds the entire request. A
    /// value of `0` disables the handshake deadline entirely, matching the core Agent convention for this setting.
    tls_handshake_timeout: Duration,

    /// Whether to signal that the backend should allow arbitrary tag values.
    ///
    /// When set, the Datadog forwarder adds `Allow-Arbitrary-Tag-Value: true` to every outbound intake request. The
    /// data plane does not perform local tag validation based on this setting.
    allow_arbitrary_tags: bool,

    /// How often API keys are checked for validity against the intake.
    api_key_validation_interval: Duration,
}

/// The endpoint and V3 routing settings that depend on a forwarder's destination.
///
/// Every other forwarder setting is the same regardless of destination, so grouping these together
/// makes each constructor set all of them, and only these, explicitly.
struct ForwarderRouting {
    /// Endpoints payloads are sent to.
    endpoint: EndpointConfiguration,

    /// Metrics routing to an alternate intake.
    opw_metrics: OpwMetricsConfiguration,

    /// Per-endpoint V3 settings from the metrics serializer.
    v3_api: V3ApiConfig,

    /// Agent-compatible V3 series routing.
    use_v3_api: UseV3ApiConfig,
}

impl ForwarderConfiguration {
    /// Creates a new `ForwarderConfiguration` from the resolved shared configuration.
    pub fn from_configuration(shared: &SharedConfiguration, config: &GenericConfiguration) -> Self {
        let endpoints = &shared.endpoints;
        let routing = ForwarderRouting {
            endpoint: EndpointConfiguration::from_configuration(endpoints),
            opw_metrics: OpwMetricsConfiguration::from_configuration(endpoints),
            v3_api: (&shared.metrics_encoding.v3_api).into(),
            use_v3_api: UseV3ApiConfig {
                series: (&shared.metrics_encoding).into(),
            },
        };

        Self::from_routing(shared, config, routing)
    }

    /// Creates a new `ForwarderConfiguration` that forwards only to a single destination.
    ///
    /// The destination replaces the configured primary endpoint, and neither dual shipping nor the
    /// alternate metrics intakes apply to it. Because the destination is part of construction, no
    /// later step can overwrite it.
    pub(crate) fn for_single_destination(
        shared: &SharedConfiguration, config: &GenericConfiguration, destination: &SingleDestination,
    ) -> Self {
        let mut v3_api: V3ApiConfig = (&shared.metrics_encoding.v3_api).into();
        let mut series_mode: UseV3ApiSeriesConfig = (&shared.metrics_encoding).into();

        if !destination.accepts_v3_series {
            series_mode = UseV3ApiSeriesConfig {
                enabled: V3SeriesMode::Disabled,
                endpoints: HashMap::new(),
            };
            v3_api.series.endpoints.clear();
        }

        let routing = ForwarderRouting {
            endpoint: EndpointConfiguration::for_single_destination(destination),
            opw_metrics: OpwMetricsConfiguration::default(),
            v3_api,
            use_v3_api: UseV3ApiConfig { series: series_mode },
        };

        Self::from_routing(shared, config, routing)
    }

    /// Builds the forwarder configuration from its destination-specific routing plus the settings
    /// every forwarder reads the same way.
    fn from_routing(shared: &SharedConfiguration, config: &GenericConfiguration, routing: ForwarderRouting) -> Self {
        let endpoints = &shared.endpoints;
        let forwarder = &endpoints.forwarder;

        Self {
            endpoint_concurrency: forwarder.max_concurrent_requests,
            endpoint_concurrency_multiplier: forwarder.num_workers,
            request_timeout_secs: forwarder.timeout,
            endpoint_buffer_size: forwarder.high_prio_buffer_size,
            endpoint: routing.endpoint,
            retry: RetryConfiguration::from_configuration(forwarder, config),
            proxy: ProxyConfiguration::from_configuration(&endpoints.proxy),
            opw_metrics: routing.opw_metrics,
            http_protocol: forwarder.http_protocol.into(),
            connection_reset_interval_secs: forwarder.connection_reset_interval,
            v3_api: routing.v3_api,
            use_v3_api: routing.use_v3_api,
            serializer_compressor_kind: endpoints.compression.compressor_kind.clone(),
            skip_ssl_validation: endpoints.tls.skip_ssl_validation,
            sslkeylogfile: endpoints.tls.sslkeylogfile.clone(),
            min_tls_version: min_tls_version_from_config_value(&endpoints.tls.min_tls_version),
            tls_handshake_timeout: endpoints.tls.handshake_timeout,
            allow_arbitrary_tags: endpoints.allow_arbitrary_tags,
            api_key_validation_interval: api_key_validation_interval(forwarder.apikey_validation_interval),
        }
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

    /// Returns the TLS handshake timeout.
    pub const fn tls_handshake_timeout(&self) -> Duration {
        self.tls_handshake_timeout
    }

    /// Returns the maximum number of pending requests for an individual endpoint.
    pub const fn endpoint_buffer_size(&self) -> usize {
        self.endpoint_buffer_size
    }

    /// Returns the HTTP protocol selection for outgoing forwarder requests.
    pub fn http_protocol(&self) -> HttpProtocol {
        self.http_protocol.into()
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
        &self.use_v3_api.series
    }

    /// Returns the OPW/Vector V3 series override for metrics-primary routing, if configured.
    pub(crate) fn opw_metrics_v3_series_override(&self) -> Option<bool> {
        self.opw_metrics
            .selected_endpoint()
            .map(|selected| selected.use_v3_series)
    }

    /// Returns the configured primary endpoint string without resolving or version-prefixing it.
    pub(crate) fn primary_configured_endpoint(&self) -> &str {
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
        self.min_tls_version
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
        self.api_key_validation_interval
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_data_plane_config::{
        shared::{AltMetricsIntake, Proxy, Tls, V3ApiEncoding, V3SeriesMode},
        ConfigValue,
    };
    use saluki_config::ConfigurationLoader;

    use super::*;
    use crate::common::datadog::test_util::shared_configuration;

    const PROXY_URL: &str = "http://proxy.example.com:3128";
    const PROXY_URI: &str = "http://proxy.example.com:3128/";
    const DATADOG_URL: &str = "http://datadog.example.com";
    const DATADOG_URI: &str = "http://datadog.example.com/";
    const OPW_URL: &str = "http://opw.example.com:8080";
    const OPW_URI: &str = "http://opw.example.com:8080/";
    const VECTOR_URL: &str = "http://vector.example.com:8080";
    const VECTOR_URI: &str = "http://vector.example.com:8080/";
    const ADDITIONAL_URL: &str = "http://additional.example.com";
    const ADDITIONAL_URI: &str = "http://additional.example.com/";
    const SSL_KEY_LOG_FILE_PATH: &str = "/tmp/saluki-sslkeylogfile";

    async fn empty_config() -> GenericConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        config
    }

    async fn forwarder_config_from(shared: SharedConfiguration) -> ForwarderConfiguration {
        ForwarderConfiguration::from_configuration(&shared, &empty_config().await)
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

    #[tokio::test]
    async fn proxy_settings_come_from_resolved_configuration() {
        let mut shared = shared_configuration();
        shared.endpoints.proxy = Proxy {
            http: PROXY_URL.to_string(),
            ..Default::default()
        };
        let config = forwarder_config_from(shared).await;

        let proxies = config.proxy().build().expect("proxies should build");
        assert_eq!(1, proxies.len());
        assert_eq!(PROXY_URI, proxies[0].uri().to_string());
    }

    #[tokio::test]
    async fn forwarder_http_protocol_maps_from_resolved_configuration() {
        let cases = [
            (shared::ForwarderHttpProtocol::Auto, HttpProtocol::Auto),
            (shared::ForwarderHttpProtocol::Http1, HttpProtocol::Http1),
        ];

        for (configured, expected) in cases {
            let mut shared = shared_configuration();
            shared.endpoints.forwarder.http_protocol = configured;
            let config = forwarder_config_from(shared).await;

            assert_eq!(expected, config.http_protocol(), "{configured:?}");
        }
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
            let mut shared = shared_configuration();
            shared.endpoints.forwarder.max_concurrent_requests = concurrency;
            shared.endpoints.forwarder.num_workers = multiplier;
            let config = forwarder_config_from(shared).await;

            assert_eq!(expected, config.endpoint_concurrency(), "{name}");
        }
    }

    #[tokio::test]
    async fn api_key_validation_interval_falls_back_for_non_positive_values() {
        let cases = [
            ("positive", 5i64, Duration::from_mins(5)),
            ("zero", 0, Duration::from_mins(60)),
            ("negative", -1, Duration::from_mins(60)),
        ];

        for (name, configured, expected) in cases {
            let mut shared = shared_configuration();
            shared.endpoints.forwarder.apikey_validation_interval = configured;
            let config = forwarder_config_from(shared).await;

            assert_eq!(expected, config.api_key_validation_interval(), "{name}");
        }
    }

    #[tokio::test]
    async fn tls_settings_come_from_resolved_configuration() {
        let mut shared = shared_configuration();
        shared.endpoints.tls = Tls {
            skip_ssl_validation: true,
            min_tls_version: "tlsv1.3".to_string(),
            sslkeylogfile: SSL_KEY_LOG_FILE_PATH.to_string(),
            handshake_timeout: Duration::from_secs(3),
        };
        let config = forwarder_config_from(shared).await;

        assert!(config.skip_ssl_validation());
        assert_eq!(TlsMinimumVersion::Tls13, config.min_tls_version());
        assert_eq!(Some(SSL_KEY_LOG_FILE_PATH), config.ssl_key_log_file_path());
        assert_eq!(Duration::from_secs(3), config.tls_handshake_timeout());
    }

    #[tokio::test]
    async fn min_tls_version_maps_configured_value() {
        // Documented mapping (see `min_tls_version_from_config_value`): an explicit tlsv1.2 maps to
        // TLS 1.2; tlsv1.3 maps to TLS 1.3 (case-insensitively); tlsv1.0/tlsv1.1 clamp up to TLS 1.2
        // because rustls has no older support; and an empty string or any unrecognized value falls
        // back to TLS 1.2.
        let cases = [
            ("explicit tlsv1.2", "tlsv1.2", TlsMinimumVersion::Tls12),
            ("tlsv1.3", "tlsv1.3", TlsMinimumVersion::Tls13),
            ("case-insensitive tlsv1.3", "TlSv1.3", TlsMinimumVersion::Tls13),
            ("tlsv1.0 clamps up", "tlsv1.0", TlsMinimumVersion::Tls12),
            ("tlsv1.1 clamps up", "tlsv1.1", TlsMinimumVersion::Tls12),
            ("empty string falls back", "", TlsMinimumVersion::Tls12),
            ("unrecognized value falls back", "tlsv1.9", TlsMinimumVersion::Tls12),
        ];

        for (name, value, expected) in cases {
            let mut shared = shared_configuration();
            shared.endpoints.tls.min_tls_version = value.to_string();
            let config = forwarder_config_from(shared).await;

            assert_eq!(expected, config.min_tls_version(), "{name}");
        }
    }

    #[tokio::test]
    async fn sslkeylogfile_whitespace_only_is_treated_as_unset() {
        // `ssl_key_log_file_path` trims the configured value, so a whitespace-only path is reported as unset.
        let mut shared = shared_configuration();
        shared.endpoints.tls.sslkeylogfile = "   ".to_string();
        let config = forwarder_config_from(shared).await;

        assert_eq!(None, config.ssl_key_log_file_path());
    }

    #[tokio::test]
    async fn allow_arbitrary_tags_comes_from_resolved_configuration() {
        let mut shared = shared_configuration();
        shared.endpoints.allow_arbitrary_tags = true;
        let config = forwarder_config_from(shared).await;

        assert!(config.allow_arbitrary_tags());
        assert!(!config.with_allow_arbitrary_tags(false).allow_arbitrary_tags());
    }

    #[tokio::test]
    async fn an_explicit_dd_url_overrides_the_site_derived_endpoint() {
        let mut shared = shared_configuration();
        shared.endpoints.site = ConfigValue::explicit("datadoghq.eu".to_string());
        shared.endpoints.dd_url = ConfigValue::explicit(DATADOG_URL.to_string());
        let config = forwarder_config_from(shared).await;

        assert_eq!(DATADOG_URL, config.primary_configured_endpoint());
        assert_eq!(
            vec![DATADOG_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Primary)
        );
    }

    #[tokio::test]
    async fn a_defaulted_dd_url_leaves_the_endpoint_to_the_site() {
        // The Agent supplies `dd_url` at its schema default even when only `site` is configured.
        let mut shared = shared_configuration();
        shared.endpoints.site = ConfigValue::explicit("datadoghq.eu".to_string());
        let config = forwarder_config_from(shared).await;

        assert_eq!("https://app.datadoghq.eu", config.primary_configured_endpoint());
    }

    #[tokio::test]
    async fn opw_metrics_endpoint_overrides_metric_primary() {
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit(DATADOG_URL.to_string());
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: OPW_URL.to_string(),
            use_v3_series: false,
        };
        let config = forwarder_config_from(shared).await;

        assert_eq!(
            vec![DATADOG_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Primary)
        );
        assert_eq!(
            vec![OPW_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );
    }

    #[tokio::test]
    async fn a_disabled_alternate_intake_does_not_override_metric_primary() {
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit(DATADOG_URL.to_string());
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: false,
            url: OPW_URL.to_string(),
            use_v3_series: false,
        };
        shared.endpoints.vector_intake = AltMetricsIntake {
            enabled: false,
            url: VECTOR_URL.to_string(),
            use_v3_series: false,
        };
        let config = forwarder_config_from(shared).await;

        assert_eq!(
            vec![DATADOG_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Primary)
        );
        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[tokio::test]
    async fn the_vector_intake_is_a_legacy_fallback() {
        let mut shared = shared_configuration();
        shared.endpoints.vector_intake = AltMetricsIntake {
            enabled: true,
            url: VECTOR_URL.to_string(),
            use_v3_series: false,
        };
        let config = forwarder_config_from(shared.clone()).await;

        assert_eq!(
            vec![VECTOR_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );

        // The Observability Pipelines Worker takes precedence when both are enabled.
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: OPW_URL.to_string(),
            use_v3_series: false,
        };
        let config = forwarder_config_from(shared).await;

        assert_eq!(
            vec![OPW_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );
    }

    #[tokio::test]
    async fn an_alternate_intake_without_a_usable_url_is_disabled() {
        let cases = [("empty url", ""), ("invalid url", "http://[::1")];

        for (name, url) in cases {
            let mut shared = shared_configuration();
            shared.endpoints.opw_intake = AltMetricsIntake {
                enabled: true,
                url: url.to_string(),
                use_v3_series: false,
            };
            // The Vector intake is not a fallback for an unusable Worker URL.
            shared.endpoints.vector_intake = AltMetricsIntake {
                enabled: true,
                url: VECTOR_URL.to_string(),
                use_v3_series: false,
            };
            let config = forwarder_config_from(shared).await;

            assert!(
                endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn metrics_routing_comes_from_resolved_configuration() {
        let mut shared = shared_configuration();
        shared.endpoints.compression.compressor_kind = "zstd".to_string();
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: OPW_URL.to_string(),
            use_v3_series: true,
        };
        shared.metrics_encoding.v3_api = V3ApiEncoding {
            compression_level: 7,
            ..Default::default()
        };
        shared.metrics_encoding.v3_series_mode = V3SeriesMode::Disabled;
        shared.metrics_encoding.v3_series_endpoint_modes =
            HashMap::from([(DATADOG_URL.to_string(), V3SeriesMode::Enabled)]);
        let config = forwarder_config_from(shared).await;

        assert_eq!(7, config.v3_api().compression_level);
        assert_eq!(V3SeriesMode::Disabled, config.use_v3_api_series().enabled);
        assert_eq!(
            Some(&V3SeriesMode::Enabled),
            config.use_v3_api_series().endpoints.get(DATADOG_URL)
        );
        assert_eq!(Some(true), config.opw_metrics_v3_series_override());
        assert!(!config.compressor_disables_metrics_v3());
    }

    #[tokio::test]
    async fn serializer_compressor_kind_zlib_disables_metrics_v3() {
        let mut shared = shared_configuration();
        shared.endpoints.compression.compressor_kind = "zlib".to_string();
        let config = forwarder_config_from(shared).await;

        assert!(config.compressor_disables_metrics_v3());
    }

    #[tokio::test]
    async fn additional_endpoints_are_dual_shipped_alongside_the_primary() {
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit(DATADOG_URL.to_string());
        shared.endpoints.additional_endpoints =
            HashMap::from([(ADDITIONAL_URL.to_string(), vec!["extra-api-key".to_string()])]);
        let config = forwarder_config_from(shared).await;

        assert_eq!(
            vec![ADDITIONAL_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Additional)
        );
    }

    #[tokio::test]
    async fn primary_like_endpoints_keep_a_live_api_key_source() {
        let mut shared = shared_configuration();
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: OPW_URL.to_string(),
            use_v3_series: false,
        };
        shared.endpoints.additional_endpoints =
            HashMap::from([(ADDITIONAL_URL.to_string(), vec!["extra-api-key".to_string()])]);

        let live_config = empty_config().await;
        let config = ForwarderConfiguration::from_configuration(&shared, &live_config);
        let endpoints = config
            .build_routable_endpoints(Some(live_config))
            .expect("endpoints should resolve");

        for route in [
            EndpointRoute::Primary,
            EndpointRoute::MetricsPrimary,
            EndpointRoute::Additional,
        ] {
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.route() == route)
                .unwrap_or_else(|| panic!("{route:?} endpoint should exist"));
            assert!(
                endpoint.endpoint().has_configuration(),
                "{route:?} endpoint should hold a live config reference"
            );
        }

        let additional = endpoints
            .iter()
            .find(|endpoint| endpoint.route() == EndpointRoute::Additional)
            .expect("additional endpoint should exist");
        assert!(
            additional.endpoint().has_api_key_index(),
            "additional endpoint should have an api_key_index"
        );
    }

    #[tokio::test]
    async fn a_single_destination_replaces_every_configured_endpoint() {
        // A destination override must survive construction: the configured primary endpoint, the
        // additional endpoints, and the alternate metrics intake all conflict with it here.
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit(DATADOG_URL.to_string());
        shared.endpoints.additional_endpoints =
            HashMap::from([(ADDITIONAL_URL.to_string(), vec!["extra-api-key".to_string()])]);
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: OPW_URL.to_string(),
            use_v3_series: true,
        };
        shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled;
        shared.metrics_encoding.v3_series_endpoint_modes =
            HashMap::from([(DATADOG_URL.to_string(), V3SeriesMode::Enabled)]);

        let destination = SingleDestination {
            url: "https://only.example.com".to_string(),
            api_key: "destination-api-key".to_string(),
            api_key_refresh_config_path: Some("multi_region_failover.api_key"),
            accepts_v3_series: false,
        };
        let config = ForwarderConfiguration::for_single_destination(&shared, &empty_config().await, &destination);

        let endpoints = config.build_routable_endpoints(None).expect("endpoint should resolve");
        assert_eq!(1, endpoints.len());
        assert_eq!(EndpointRoute::Primary, endpoints[0].route());
        assert_eq!("https://only.example.com/", endpoints[0].endpoint().endpoint().as_str());
        assert_eq!("destination-api-key", endpoints[0].endpoint().cached_api_key());

        // A destination that does not accept V3 series payloads gets none of the configured V3 routing.
        assert_eq!(V3SeriesMode::Disabled, config.use_v3_api_series().enabled);
        assert!(config.use_v3_api_series().endpoints.is_empty());
        assert!(config.v3_api().series.endpoints.is_empty());
    }

    #[tokio::test]
    async fn a_single_destination_keeps_configured_v3_series_routing_when_it_accepts_v3() {
        let mut shared = shared_configuration();
        shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled;
        let destination = SingleDestination {
            url: "https://mrf.example.com".to_string(),
            api_key: "mrf-api-key".to_string(),
            api_key_refresh_config_path: Some("multi_region_failover.api_key"),
            accepts_v3_series: true,
        };
        let config = ForwarderConfiguration::for_single_destination(&shared, &empty_config().await, &destination);

        assert_eq!(V3SeriesMode::Enabled, config.use_v3_api_series().enabled);
    }
}

use std::time::Duration;

use saluki_component_config::forwarder as leaf;
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use tracing::warn;

use super::{
    endpoints::{EndpointConfiguration, EndpointRoute, LiveForwarderConfig, RoutableEndpoint},
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
};

const fn default_api_key_validation_interval_mins() -> u64 {
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

/// HTTP protocol selection for the Datadog forwarder.
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

impl From<leaf::ForwarderHttpProtocol> for ForwarderHttpProtocol {
    fn from(protocol: leaf::ForwarderHttpProtocol) -> Self {
        match protocol {
            leaf::ForwarderHttpProtocol::Auto => Self::Auto,
            leaf::ForwarderHttpProtocol::Http1 => Self::Http1,
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

    /// Enables routing all metrics to Vector. Deprecated.
    vector_enabled: bool,

    /// Endpoint of the Vector instance to route metrics to. Deprecated.
    vector_url: String,
}

impl OpwMetricsConfiguration {
    fn from_native(cfg: &leaf::OpwMetricsConfiguration) -> Self {
        Self {
            observability_pipelines_worker_enabled: cfg.observability_pipelines_worker_enabled,
            observability_pipelines_worker_url: cfg.observability_pipelines_worker_url.clone(),
            vector_enabled: cfg.vector_enabled,
            vector_url: cfg.vector_url.clone(),
        }
    }
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
/// Behavior-carrying runtime type built from its leaf mirror via [`ForwarderConfiguration::from_native`].
/// It controls the forwarder behavior, such as retries and concurrency, in conjunction with existing
/// primitives, such as retry policies in [`saluki_io::util::retry`].
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct ForwarderConfiguration {
    endpoint_concurrency: usize,

    endpoint_concurrency_multiplier: usize,

    request_timeout_secs: u64,

    endpoint_buffer_size: usize,

    /// Endpoint configuration.
    pub(crate) endpoint: EndpointConfiguration,

    /// Retry configuration.
    retry: RetryConfiguration,

    /// Proxy configuration.
    proxy: Option<ProxyConfiguration>,

    /// OPW metrics routing configuration.
    opw_metrics: OpwMetricsConfiguration,

    http_protocol: ForwarderHttpProtocol,

    connection_reset_interval_secs: u64,

    skip_ssl_validation: bool,

    sslkeylogfile: String,

    /// Parsed minimum TLS protocol version for Datadog intake forwarding.
    parsed_min_tls_version: TlsMinimumVersion,

    allow_arbitrary_tags: bool,

    api_key_validation_interval_mins: i64,
}

impl ForwarderConfiguration {
    /// Builds the runtime forwarder configuration from its leaf mirror.
    ///
    /// The leaf `min_tls_version` string is parsed into a [`TlsMinimumVersion`] here (clamping
    /// TLS 1.0/1.1 to 1.2), and a non-positive `api_key_validation_interval_mins` falls back to the
    /// default. A proxy sub-config is constructed only when at least one proxy server is set,
    /// mirroring the previous source-flatten semantics.
    pub fn from_native(cfg: &leaf::DatadogForwarderConfig) -> Self {
        let api_key_validation_interval_mins = if cfg.api_key_validation_interval_mins <= 0 {
            warn!(
                config_key = "forwarder_apikey_validation_interval",
                fallback_minutes = default_api_key_validation_interval_mins(),
                "Configured API key validation interval is invalid; using default."
            );
            default_api_key_validation_interval_mins() as i64
        } else {
            cfg.api_key_validation_interval_mins
        };

        // Only build a proxy sub-config when the leaf carries a proxy server; an all-default proxy
        // section (no servers) is equivalent to "no proxy", matching the previous flatten behavior.
        let proxy = cfg.proxy.as_ref().and_then(|proxy| {
            if proxy.http_server.is_none() && proxy.https_server.is_none() {
                None
            } else {
                Some(ProxyConfiguration::from_native(proxy))
            }
        });

        Self {
            endpoint_concurrency: cfg.endpoint_concurrency,
            endpoint_concurrency_multiplier: cfg.endpoint_concurrency_multiplier,
            request_timeout_secs: cfg.request_timeout_secs,
            endpoint_buffer_size: cfg.endpoint_buffer_size,
            endpoint: EndpointConfiguration::from_native(&cfg.endpoint),
            retry: RetryConfiguration::from_native(&cfg.retry),
            proxy,
            opw_metrics: OpwMetricsConfiguration::from_native(&cfg.opw_metrics),
            http_protocol: cfg.http_protocol.into(),
            connection_reset_interval_secs: cfg.connection_reset_interval_secs,
            skip_ssl_validation: cfg.skip_ssl_validation,
            sslkeylogfile: cfg.sslkeylogfile.clone(),
            parsed_min_tls_version: min_tls_version_from_config_value(&cfg.min_tls_version),
            allow_arbitrary_tags: cfg.allow_arbitrary_tags,
            api_key_validation_interval_mins,
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
        &self, live_config: Option<LiveForwarderConfig>,
    ) -> Result<Vec<RoutableEndpoint>, GenericError> {
        // Label each endpoint so the I/O loop can route metrics to OPW and non-metrics to the normal primary.
        let mut endpoints = Vec::new();
        endpoints.push(RoutableEndpoint::new(
            EndpointRoute::Primary,
            self.endpoint.build_primary_endpoint(live_config.clone())?,
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
                    .build_primary_endpoint_override(trimmed_url, live_config.clone())
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
                .build_additional_endpoints(live_config.clone())?
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
    use saluki_component_config::forwarder as leaf;

    use super::*;

    const DATADOG_URL: &str = "http://datadog.example.com";
    const DATADOG_URI: &str = "http://datadog.example.com/";
    const OPW_URL: &str = "http://opw.example.com:8080";
    const OPW_URI: &str = "http://opw.example.com:8080/";
    const VECTOR_URL: &str = "http://vector.example.com:8080";
    const VECTOR_URI: &str = "http://vector.example.com:8080/";
    const ADDITIONAL_URI: &str = "http://additional.example.com/";

    fn base_leaf() -> leaf::DatadogForwarderConfig {
        leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                api_key: "test-api-key".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
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
    fn http_protocol_maps_from_leaf() {
        let config = ForwarderConfiguration::from_native(&base_leaf());
        assert_eq!(config.http_protocol(), HttpProtocol::Auto);

        let mut leaf_cfg = base_leaf();
        leaf_cfg.http_protocol = leaf::ForwarderHttpProtocol::Http1;
        let config = ForwarderConfiguration::from_native(&leaf_cfg);
        assert_eq!(config.http_protocol(), HttpProtocol::Http1);
    }

    #[test]
    fn endpoint_concurrency_uses_configured_multiplier() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint_concurrency = 3;
        leaf_cfg.endpoint_concurrency_multiplier = 4;
        let config = ForwarderConfiguration::from_native(&leaf_cfg);
        assert_eq!(config.endpoint_concurrency(), 12);
    }

    #[test]
    fn api_key_validation_interval_parsing() {
        let cases = [
            ("default", 60i64, Duration::from_mins(60)),
            ("positive", 5, Duration::from_mins(5)),
            ("zero", 0, Duration::from_mins(60)),
            ("negative", -1, Duration::from_mins(60)),
        ];

        for (case_name, configured, expected_interval) in cases {
            let mut leaf_cfg = base_leaf();
            leaf_cfg.api_key_validation_interval_mins = configured;
            let config = ForwarderConfiguration::from_native(&leaf_cfg);
            assert_eq!(config.api_key_validation_interval(), expected_interval, "{case_name}");
        }
    }

    #[test]
    fn ssl_and_arbitrary_tags_flags_map_through() {
        let config = ForwarderConfiguration::from_native(&base_leaf());
        assert!(!config.skip_ssl_validation());
        assert!(!config.allow_arbitrary_tags());
        assert_eq!(config.ssl_key_log_file_path(), None);

        let mut leaf_cfg = base_leaf();
        leaf_cfg.skip_ssl_validation = true;
        leaf_cfg.allow_arbitrary_tags = true;
        leaf_cfg.sslkeylogfile = "/tmp/saluki-sslkeylogfile".to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);
        assert!(config.skip_ssl_validation());
        assert!(config.allow_arbitrary_tags());
        assert_eq!(config.ssl_key_log_file_path(), Some("/tmp/saluki-sslkeylogfile"));
    }

    #[test]
    fn min_tls_version_parsing_and_clamping() {
        let cases = [
            ("tls1.2", "tlsv1.2", TlsMinimumVersion::Tls12),
            ("tls1.3", "tlsv1.3", TlsMinimumVersion::Tls13),
            ("case insensitive", "TlSv1.3", TlsMinimumVersion::Tls13),
            ("tls1.0 clamps", "tlsv1.0", TlsMinimumVersion::Tls12),
            ("tls1.1 clamps", "tlsv1.1", TlsMinimumVersion::Tls12),
            ("invalid falls back", "tlsv1.9", TlsMinimumVersion::Tls12),
        ];

        for (case_name, value, expected) in cases {
            let mut leaf_cfg = base_leaf();
            leaf_cfg.min_tls_version = value.to_string();
            let config = ForwarderConfiguration::from_native(&leaf_cfg);
            assert_eq!(config.min_tls_version(), expected, "{case_name}");
        }
    }

    #[test]
    fn opw_metrics_endpoint_overrides_metric_primary() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_enabled = true;
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = OPW_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

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
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = OPW_URL.to_string();
        leaf_cfg.opw_metrics.vector_url = VECTOR_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Primary),
            vec![DATADOG_URI]
        );
        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn vector_metrics_endpoint_is_legacy_fallback() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.vector_enabled = true;
        leaf_cfg.opw_metrics.vector_url = VECTOR_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![VECTOR_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_takes_precedence_over_vector() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_enabled = true;
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = OPW_URL.to_string();
        leaf_cfg.opw_metrics.vector_enabled = true;
        leaf_cfg.opw_metrics.vector_url = VECTOR_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary),
            vec![OPW_URI]
        );
    }

    #[test]
    fn opw_metrics_endpoint_does_not_fallback_to_vector_when_opw_url_empty() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_enabled = true;
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = String::new();
        leaf_cfg.opw_metrics.vector_enabled = true;
        leaf_cfg.opw_metrics.vector_url = VECTOR_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn opw_metrics_endpoint_does_not_fallback_to_vector_when_opw_url_invalid() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_enabled = true;
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = "http://[::1".to_string();
        leaf_cfg.opw_metrics.vector_enabled = true;
        leaf_cfg.opw_metrics.vector_url = VECTOR_URL.to_string();
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn opw_metrics_endpoint_preserves_additional_endpoints() {
        let mut leaf_cfg = base_leaf();
        leaf_cfg.endpoint.dd_url = Some(DATADOG_URL.to_string());
        leaf_cfg.opw_metrics.observability_pipelines_worker_enabled = true;
        leaf_cfg.opw_metrics.observability_pipelines_worker_url = OPW_URL.to_string();
        leaf_cfg.endpoint.additional_endpoints = leaf::AdditionalEndpoints(
            [(
                "http://additional.example.com".to_string(),
                vec!["extra-api-key".to_string()],
            )]
            .into_iter()
            .collect(),
        );
        let config = ForwarderConfiguration::from_native(&leaf_cfg);

        assert_eq!(
            endpoint_urls_by_route(&config, EndpointRoute::Additional),
            vec![ADDITIONAL_URI]
        );
    }
}

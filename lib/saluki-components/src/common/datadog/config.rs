use std::{collections::HashMap, sync::Arc, time::Duration};

use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use tracing::warn;

use super::{
    endpoints::{EndpointConfiguration, EndpointRoute, RoutableEndpoint, SingleDestination},
    protocol::{UseV3ApiConfig, UseV3ApiSeriesConfig, V3ApiConfig, V3SeriesMode},
    proxy::ProxyConfiguration,
    retry::RetryConfiguration,
    routing::RoutingTargetCatalog,
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpwMetricsConfiguration {
    /// Observability Pipelines Worker routing settings.
    pub observability_pipelines_worker: OpwMetricsSettings,

    /// Vector routing settings, deprecated in favor of the Observability Pipelines Worker.
    pub vector: OpwMetricsSettings,
}

/// One alternate intake's routing settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpwMetricsSettings {
    /// Whether all metrics route to this intake.
    pub enabled: bool,

    /// Endpoint of the instance to route metrics to.
    pub url: String,

    /// Whether series metrics routed to this intake use the V3 protocol.
    pub use_v3_series: bool,
}

impl OpwMetricsConfiguration {
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

/// Forwarder settings, such as retries and concurrency, mirroring those of the Datadog Agent's forwarder.
///
/// They are used with existing primitives, such as the retry policies in [`saluki_io::net::util::retry`].
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct ForwarderConfiguration {
    /// Maximum number of concurrent requests for an individual endpoint.
    ///
    /// If set to 0, request concurrency is clamped to 1.
    pub endpoint_concurrency: usize,

    /// Multiplier for endpoint request concurrency.
    ///
    /// This value also sizes the HTTP idle connection pool. If set to 0, idle connection retention is
    /// disabled and the concurrency multiplier is treated as 1. This setting does not create worker tasks.
    pub endpoint_concurrency_multiplier: usize,

    /// Request timeout, in seconds.
    pub request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    pub endpoint_buffer_size: usize,

    /// Endpoints payloads are sent to.
    pub endpoint: EndpointConfiguration,

    /// Retry configuration.
    pub retry: RetryConfiguration,

    /// Proxy configuration.
    pub proxy: ProxyConfiguration,

    /// Metrics routing to an alternate intake.
    pub opw_metrics: OpwMetricsConfiguration,

    /// HTTP protocol selection for outgoing forwarder requests.
    ///
    /// `auto` negotiates HTTP/2 with HTTP/1.1 fallback; `http1` forces HTTP/1.1 only.
    pub http_protocol: ForwarderHttpProtocol,

    /// Connection reset interval, in seconds.
    pub connection_reset_interval_secs: u64,

    /// Whether in-flight requests are allowed to finish when the forwarder shuts down.
    ///
    /// When `false`, in-flight requests are aborted when shutdown begins and their transactions are requeued, so that
    /// they are persisted along with the other pending transactions.
    pub stop_wait_for_inflight: bool,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// This is shared with the metrics encoder and used by the I/O layer to filter payloads based on
    /// endpoint URL matching.
    pub v3_api: V3ApiConfig,

    /// Agent-compatible V3 API configuration.
    pub use_v3_api: UseV3ApiConfig,

    /// Payload compressor kind used by the metrics serializer.
    ///
    /// V3 metrics intake is incompatible with zlib/deflate, so the forwarder needs this setting to keep endpoint
    /// filtering aligned with the encoder when zlib forces metrics back to V2.
    pub serializer_compressor_kind: String,

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// When set, HTTPS clients built for the shared Datadog forwarder accept invalid server certificates. Only
    /// deployments that intentionally route Datadog intake traffic through endpoints with invalid or self-signed
    /// certificates should enable this.
    pub skip_ssl_validation: bool,

    /// File path to write TLS key material to for all HTTPS connections to the
    /// Datadog backend.
    ///
    /// When non-empty, enables the logging of TLS key material to the given file path,
    /// in the [NSS Key Log][nss_key_log] format, which can be used for debugging TLS
    /// issues, as well as decrypting captured TLS traffic in tools such as Wireshark.
    ///
    /// [nss_key_log]: https://nss-crypto.org/reference/security/nss/legacy/key_log_format/index.html
    pub sslkeylogfile: String,

    /// Minimum TLS protocol version for Datadog intake forwarding, such as `tlsv1.2`.
    ///
    /// TLS 1.0 and TLS 1.1 are accepted for compatibility with core Agent configuration, but Saluki clamps them to
    /// TLS 1.2 because rustls does not support older protocol versions. An empty value means TLS 1.2, and an invalid
    /// value falls back to it with a warning.
    pub min_tls_version: String,

    /// Timeout for completing the TLS handshake after a connection is established, for Datadog intake forwarding.
    ///
    /// This bounds only the TLS handshake step, distinct from `forwarder_timeout`, which bounds the entire request. A
    /// value of `0` disables the handshake deadline entirely, matching the core Agent convention for this setting.
    pub tls_handshake_timeout: Duration,

    /// Whether to signal that the backend should allow arbitrary tag values.
    ///
    /// When set, the Datadog forwarder adds `Allow-Arbitrary-Tag-Value: true` to every outbound intake request. The
    /// data plane does not perform local tag validation based on this setting.
    pub allow_arbitrary_tags: bool,

    /// How often API keys are checked for validity against the intake, in minutes.
    ///
    /// A non-positive value falls back to the default with a warning.
    pub api_key_validation_interval_mins: i64,

    /// Routing targets that metrics payloads may be addressed to.
    ///
    /// When set, each endpoint is labeled with its routing target so that a payload addressed to specific targets
    /// reaches only those. When unset, no endpoint is a target, and a payload addressed to specific targets reaches
    /// none of them.
    ///
    /// The catalog must be the one shared with every encoder that addresses payloads to this forwarder, and it must
    /// have been built from the same endpoint configuration as this forwarder.
    pub routing_targets: Option<Arc<RoutingTargetCatalog>>,
}

impl ForwarderConfiguration {
    /// Replaces every configured endpoint with a single destination.
    ///
    /// Neither dual shipping, the alternate metrics intakes, nor routing targets apply to the destination, and a
    /// destination that does not accept V3 series payloads gets none of the configured V3 series routing.
    pub(crate) fn for_single_destination(mut self, destination: &SingleDestination) -> Self {
        self.endpoint = EndpointConfiguration::for_single_destination(destination);
        self.opw_metrics = OpwMetricsConfiguration::default();
        self.routing_targets = None;
        if !destination.accepts_v3_series {
            self.use_v3_api.series = UseV3ApiSeriesConfig {
                enabled: V3SeriesMode::Disabled,
                endpoints: HashMap::new(),
            };
        }

        self
    }

    /// Returns the maximum number of concurrent requests for an individual endpoint, after clamping each factor.
    pub(crate) const fn effective_endpoint_concurrency(&self) -> usize {
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

    /// Returns the request timeout.
    pub(crate) const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    /// Builds resolved endpoints with routing metadata.
    ///
    /// Each endpoint starts with the key its configuration resolved to. A forwarder may bind it to a live
    /// configuration view through [`ApiKeyRefresher`][super::api_key::ApiKeyRefresher].
    ///
    /// When routing targets are set, each endpoint is labeled with the target it delivers for. An alternate metrics
    /// intake delivers for the primary target, because it replaces the primary endpoint for metrics.
    pub(crate) fn build_routable_endpoints(&self) -> Result<Vec<RoutableEndpoint>, GenericError> {
        let mut endpoints = self.build_unlabeled_endpoints()?;
        if let Some(catalog) = &self.routing_targets {
            endpoints = endpoints
                .into_iter()
                .map(|endpoint| {
                    let target = match endpoint.route() {
                        EndpointRoute::Primary | EndpointRoute::MetricsPrimary => Some(catalog.primary()),
                        EndpointRoute::Additional => endpoint
                            .endpoint()
                            .additional_endpoint_queue_key()
                            .and_then(|(url, index)| catalog.find_additional(url, index)),
                    };
                    endpoint.with_target(target)
                })
                .collect();
        }

        Ok(endpoints)
    }

    fn build_unlabeled_endpoints(&self) -> Result<Vec<RoutableEndpoint>, GenericError> {
        // Label each endpoint so the I/O loop can route metrics to OPW and non-metrics to the normal primary.
        let mut endpoints = Vec::new();
        endpoints.push(RoutableEndpoint::new(
            EndpointRoute::Primary,
            self.endpoint.build_primary_endpoint()?,
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
                match self.endpoint.build_primary_endpoint_override(trimmed_url) {
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

    /// Returns the connection reset interval.
    pub(crate) const fn connection_reset_interval(&self) -> Duration {
        Duration::from_secs(self.connection_reset_interval_secs)
    }

    /// Returns the OPW/Vector V3 series override for metrics-primary routing, if configured.
    pub(crate) fn opw_metrics_v3_series_override(&self) -> Option<bool> {
        self.opw_metrics
            .selected_endpoint()
            .map(|selected| selected.use_v3_series)
    }

    /// Returns whether the configured metrics compressor is incompatible with Metrics V3.
    pub(crate) fn compressor_disables_metrics_v3(&self) -> bool {
        self.serializer_compressor_kind.trim().eq_ignore_ascii_case("zlib")
    }

    /// Returns the TLS key log file path, if configured.
    pub(crate) fn ssl_key_log_file_path(&self) -> Option<&str> {
        let trimmed = self.sslkeylogfile.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    /// Returns the minimum TLS protocol version for Datadog intake forwarding, warning if it is unsupported.
    pub(crate) fn effective_min_tls_version(&self) -> TlsMinimumVersion {
        min_tls_version_from_config_value(&self.min_tls_version)
    }

    /// Returns the API key validation interval, warning if the configured interval is not positive.
    pub(crate) fn effective_api_key_validation_interval(&self) -> Duration {
        api_key_validation_interval(self.api_key_validation_interval_mins)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::common::datadog::{
        config::OpwMetricsSettings,
        test_util::{forwarder_configuration, TEST_API_KEY},
    };

    const DATADOG_URL: &str = "http://datadog.example.com";
    const DATADOG_URI: &str = "http://datadog.example.com/";
    const OPW_URL: &str = "http://opw.example.com:8080";
    const OPW_URI: &str = "http://opw.example.com:8080/";
    const VECTOR_URL: &str = "http://vector.example.com:8080";
    const VECTOR_URI: &str = "http://vector.example.com:8080/";
    const ADDITIONAL_URL: &str = "http://additional.example.com";
    const ADDITIONAL_URI: &str = "http://additional.example.com/";
    const SSL_KEY_LOG_FILE_PATH: &str = "/tmp/saluki-sslkeylogfile";

    /// Returns a forwarder configuration whose primary endpoint is `primary_endpoint`.
    fn forwarder_config_for(
        primary_endpoint: &str, additional_endpoints: HashMap<String, Vec<String>>,
    ) -> ForwarderConfiguration {
        ForwarderConfiguration {
            endpoint: EndpointConfiguration::new(
                TEST_API_KEY.to_string(),
                primary_endpoint.to_string(),
                additional_endpoints,
            ),
            ..forwarder_configuration()
        }
    }

    fn alternate_intake(enabled: bool, url: &str, use_v3_series: bool) -> OpwMetricsSettings {
        OpwMetricsSettings {
            enabled,
            url: url.to_string(),
            use_v3_series,
        }
    }

    fn endpoint_urls_by_route(config: &ForwarderConfiguration, route: EndpointRoute) -> Vec<String> {
        config
            .build_routable_endpoints()
            .expect("endpoints should resolve")
            .into_iter()
            .filter_map(|endpoint| {
                let (endpoint_route, endpoint, _) = endpoint.into_parts();
                (endpoint_route == route).then(|| endpoint.endpoint().to_string())
            })
            .collect()
    }

    #[test]
    fn forwarder_http_protocol_maps_to_the_client_protocol() {
        assert_eq!(HttpProtocol::Auto, ForwarderHttpProtocol::Auto.into());
        assert_eq!(HttpProtocol::Http1, ForwarderHttpProtocol::Http1.into());
    }

    #[test]
    fn endpoint_concurrency_multiplies_and_clamps_zero_values_to_one() {
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
            let config = ForwarderConfiguration {
                endpoint_concurrency: concurrency,
                endpoint_concurrency_multiplier: multiplier,
                ..forwarder_configuration()
            };

            assert_eq!(expected, config.effective_endpoint_concurrency(), "{name}");
        }
    }

    #[test]
    fn api_key_validation_interval_falls_back_for_non_positive_values() {
        let cases = [
            ("positive", 5i64, Duration::from_mins(5)),
            ("zero", 0, Duration::from_mins(60)),
            ("negative", -1, Duration::from_mins(60)),
        ];

        for (name, configured, expected) in cases {
            let config = ForwarderConfiguration {
                api_key_validation_interval_mins: configured,
                ..forwarder_configuration()
            };

            assert_eq!(expected, config.effective_api_key_validation_interval(), "{name}");
        }
    }

    #[test]
    fn min_tls_version_maps_configured_value() {
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
            let config = ForwarderConfiguration {
                min_tls_version: value.to_string(),
                ..forwarder_configuration()
            };

            assert_eq!(expected, config.effective_min_tls_version(), "{name}");
        }
    }

    #[test]
    fn sslkeylogfile_is_trimmed_and_whitespace_only_is_treated_as_unset() {
        // `ssl_key_log_file_path` trims the configured value, so a whitespace-only path is reported as unset.
        let config = ForwarderConfiguration {
            sslkeylogfile: format!(" {SSL_KEY_LOG_FILE_PATH} "),
            ..forwarder_configuration()
        };
        assert_eq!(Some(SSL_KEY_LOG_FILE_PATH), config.ssl_key_log_file_path());

        let config = ForwarderConfiguration {
            sslkeylogfile: "   ".to_string(),
            ..forwarder_configuration()
        };
        assert_eq!(None, config.ssl_key_log_file_path());
    }

    #[test]
    fn opw_metrics_endpoint_overrides_metric_primary() {
        let mut config = forwarder_config_for(DATADOG_URL, HashMap::new());
        config.opw_metrics.observability_pipelines_worker = alternate_intake(true, OPW_URL, false);

        assert_eq!(
            vec![DATADOG_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Primary)
        );
        assert_eq!(
            vec![OPW_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );
    }

    #[test]
    fn a_disabled_alternate_intake_does_not_override_metric_primary() {
        let mut config = forwarder_config_for(DATADOG_URL, HashMap::new());
        config.opw_metrics = OpwMetricsConfiguration {
            observability_pipelines_worker: alternate_intake(false, OPW_URL, false),
            vector: alternate_intake(false, VECTOR_URL, false),
        };

        assert_eq!(
            vec![DATADOG_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Primary)
        );
        assert!(endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty());
    }

    #[test]
    fn the_vector_intake_is_a_legacy_fallback() {
        let mut config = forwarder_configuration();
        config.opw_metrics.vector = alternate_intake(true, VECTOR_URL, false);

        assert_eq!(
            vec![VECTOR_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );

        // The Observability Pipelines Worker takes precedence when both are enabled.
        config.opw_metrics.observability_pipelines_worker = alternate_intake(true, OPW_URL, false);

        assert_eq!(
            vec![OPW_URI],
            endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary)
        );
    }

    #[test]
    fn an_alternate_intake_without_a_usable_url_is_disabled() {
        let cases = [("empty url", ""), ("invalid url", "http://[::1")];

        for (name, url) in cases {
            let mut config = forwarder_configuration();
            config.opw_metrics = OpwMetricsConfiguration {
                observability_pipelines_worker: alternate_intake(true, url, false),
                // The Vector intake is not a fallback for an unusable Worker URL.
                vector: alternate_intake(true, VECTOR_URL, false),
            };

            assert!(
                endpoint_urls_by_route(&config, EndpointRoute::MetricsPrimary).is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn the_selected_alternate_intake_supplies_the_v3_series_override() {
        let mut config = forwarder_configuration();
        assert_eq!(None, config.opw_metrics_v3_series_override());

        config.opw_metrics.vector = alternate_intake(true, VECTOR_URL, false);
        assert_eq!(Some(false), config.opw_metrics_v3_series_override());

        config.opw_metrics.observability_pipelines_worker = alternate_intake(true, OPW_URL, true);
        assert_eq!(Some(true), config.opw_metrics_v3_series_override());
    }

    #[test]
    fn serializer_compressor_kind_zlib_disables_metrics_v3() {
        for (kind, expected) in [("zstd", false), ("zlib", true), (" ZLIB ", true)] {
            let config = ForwarderConfiguration {
                serializer_compressor_kind: kind.to_string(),
                ..forwarder_configuration()
            };

            assert_eq!(expected, config.compressor_disables_metrics_v3(), "{kind}");
        }
    }

    #[test]
    fn additional_endpoints_are_dual_shipped_alongside_the_primary() {
        let config = forwarder_config_for(
            DATADOG_URL,
            HashMap::from([(ADDITIONAL_URL.to_string(), vec!["extra-api-key".to_string()])]),
        );

        assert_eq!(
            vec![ADDITIONAL_URI],
            endpoint_urls_by_route(&config, EndpointRoute::Additional)
        );
    }

    #[test]
    fn a_single_destination_replaces_every_configured_endpoint() {
        // A destination override must survive construction: the configured primary endpoint, the
        // additional endpoints, and the alternate metrics intake all conflict with it here.
        let mut config = forwarder_config_for(
            DATADOG_URL,
            HashMap::from([(ADDITIONAL_URL.to_string(), vec!["extra-api-key".to_string()])]),
        );
        config.opw_metrics.observability_pipelines_worker = alternate_intake(true, OPW_URL, true);
        config.use_v3_api.series = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::Enabled,
            endpoints: HashMap::from([(DATADOG_URL.to_string(), V3SeriesMode::Enabled)]),
        };

        let destination = SingleDestination {
            url: "https://only.example.com".to_string(),
            api_key: "destination-api-key".to_string(),
            accepts_v3_series: false,
        };
        let config = config.for_single_destination(&destination);

        let endpoints = config.build_routable_endpoints().expect("endpoint should resolve");
        assert_eq!(1, endpoints.len());
        assert_eq!(EndpointRoute::Primary, endpoints[0].route());
        assert_eq!("https://only.example.com/", endpoints[0].endpoint().endpoint().as_str());
        assert_eq!("destination-api-key", &*endpoints[0].endpoint().api_key());
        assert_eq!(None, config.opw_metrics_v3_series_override());

        // A destination that does not accept V3 series payloads gets none of the configured V3 routing.
        assert_eq!(V3SeriesMode::Disabled, config.use_v3_api.series.enabled);
        assert!(config.use_v3_api.series.endpoints.is_empty());
    }

    #[test]
    fn a_single_destination_keeps_configured_v3_series_routing_when_it_accepts_v3() {
        let mut config = forwarder_configuration();
        config.use_v3_api.series.enabled = V3SeriesMode::Enabled;
        let destination = SingleDestination {
            url: "https://mrf.example.com".to_string(),
            api_key: "mrf-api-key".to_string(),
            accepts_v3_series: true,
        };
        let config = config.for_single_destination(&destination);

        assert_eq!(V3SeriesMode::Enabled, config.use_v3_api.series.enabled);
    }
}

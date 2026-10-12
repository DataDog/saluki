use std::{collections::HashMap, path::PathBuf, time::Duration};

use agent_data_plane_config::{
    shared::{self, Endpoints, SharedConfiguration, V3SeriesMode},
    ConfigValue,
};
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpProtocol, TlsMinimumVersion};
use tracing::warn;

use super::{
    delivery::{
        DeliverySettings, IntakeAuthorities, PayloadClasses, RecipientProtocol, RecipientSpec, RetrySettings,
        SpillPolicy, V3Acceptance,
    },
    endpoints::{
        EndpointConfiguration, EndpointRoute, EndpointV3Settings, MetricsRecipient, MetricsRecipientRoute,
        ResolvedEndpoint, RoutableEndpoint, SingleDestination, V3EndpointConfig,
    },
    io::should_route_to_endpoint,
    protocol::{MetricsEndpointRouting, UseV3ApiConfig, UseV3ApiSeriesConfig, V3ApiConfig},
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
/// Agent, which are used to control the behavior of its forwarder, such as retries and concurrency, in conjunction
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

    /// The forwarder settings this configuration was resolved from, which a hosted recipient's settings start from.
    forwarder: shared::Forwarder,

    /// The run path the retry queue's storage path is derived from when none is configured.
    run_path: Option<PathBuf>,

    /// The V3 acceptance of a recipient a delivery host runs, which replaces what V3 configuration resolves for its
    /// endpoint.
    v3_acceptance: Option<V3Acceptance>,
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
    pub fn from_configuration(shared: &SharedConfiguration) -> Self {
        let endpoints = &shared.endpoints;
        let routing = ForwarderRouting {
            endpoint: EndpointConfiguration::from_configuration(endpoints),
            opw_metrics: OpwMetricsConfiguration::from_configuration(endpoints),
            v3_api: (&shared.metrics_encoding.v3_api).into(),
            use_v3_api: UseV3ApiConfig {
                series: (&shared.metrics_encoding).into(),
            },
        };

        Self::from_routing(shared, routing)
    }

    /// Creates a new `ForwarderConfiguration` that forwards only to a single destination.
    ///
    /// The destination replaces the configured primary endpoint, and neither dual shipping nor the
    /// alternate metrics intakes apply to it. Because the destination is part of construction, no
    /// later step can overwrite it.
    pub(crate) fn for_single_destination(shared: &SharedConfiguration, destination: &SingleDestination) -> Self {
        let series_mode = if destination.accepts_v3_series {
            (&shared.metrics_encoding).into()
        } else {
            UseV3ApiSeriesConfig {
                enabled: V3SeriesMode::Disabled,
                endpoints: HashMap::new(),
            }
        };

        let routing = ForwarderRouting {
            endpoint: EndpointConfiguration::for_single_destination(destination),
            opw_metrics: OpwMetricsConfiguration::default(),
            v3_api: (&shared.metrics_encoding.v3_api).into(),
            use_v3_api: UseV3ApiConfig { series: series_mode },
        };

        Self::from_routing(shared, routing)
    }

    /// Builds the forwarder configuration from its destination-specific routing plus the settings
    /// every forwarder reads the same way.
    fn from_routing(shared: &SharedConfiguration, routing: ForwarderRouting) -> Self {
        let endpoints = &shared.endpoints;
        let forwarder = &endpoints.forwarder;

        Self {
            endpoint_concurrency: forwarder.max_concurrent_requests,
            endpoint_concurrency_multiplier: forwarder.num_workers,
            request_timeout_secs: forwarder.timeout,
            endpoint_buffer_size: forwarder.high_prio_buffer_size,
            endpoint: routing.endpoint,
            retry: RetryConfiguration::from_configuration(forwarder, shared.run_path.as_deref()),
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
            forwarder: forwarder.clone(),
            run_path: shared.run_path.clone(),
            v3_acceptance: None,
        }
    }

    /// Returns the settings a delivery host runs with when it follows this configuration: its client profile, and
    /// its retry and concurrency settings.
    ///
    /// Each proxy appears as its URL without user information and the reference of the credential that holds the
    /// user information ([`ProxySettings::NATIVE_HTTP_CREDENTIAL`][super::delivery::ProxySettings], and the `https`
    /// one).
    pub(crate) fn delivery_settings(&self) -> DeliverySettings {
        let forwarder = &self.forwarder;
        DeliverySettings {
            endpoint_concurrency: self.endpoint_concurrency,
            endpoint_concurrency_multiplier: self.endpoint_concurrency_multiplier,
            endpoint_buffer_size: self.endpoint_buffer_size,
            request_timeout: self.request_timeout(),
            retry: RetrySettings {
                backoff_factor: forwarder.backoff_factor,
                backoff_base: forwarder.backoff_base,
                backoff_max: forwarder.backoff_max,
                recovery_error_decrease_factor: forwarder.recovery_interval,
                recovery_reset: forwarder.recovery_reset,
                queue_max_size_bytes: self.retry.queue_max_size_bytes(),
                storage_max_size_bytes: self.retry.storage_max_size_bytes(),
                flush_to_disk_mem_ratio: self.retry.flush_to_disk_mem_ratio(),
                storage_max_disk_ratio: self.retry.storage_max_disk_ratio(),
                outdated_file_in_days: self.retry.outdated_file_in_days(),
            },
            proxy: self.proxy.settings(),
            http_protocol: self.http_protocol,
            connection_reset_interval: self.connection_reset_interval(),
            skip_ssl_validation: self.skip_ssl_validation,
            min_tls_version: self.min_tls_version,
            tls_handshake_timeout: self.tls_handshake_timeout,
            api_key_validation_interval: self.api_key_validation_interval,
        }
    }

    /// Returns the forwarder settings of the I/O loop of one recipient a delivery host runs: `settings`, narrowed by
    /// the recipient's delivery policy, with what stays fixed while the process runs taken from this configuration
    /// (the retry queue's storage path, the TLS key log).
    ///
    /// The recipient's queue holds at most its policy's bound, never more than the host's; a recipient that never
    /// spills has no persisted queue. Its endpoint's V3 settings are those its protocol accepts, whatever V3
    /// configuration says of the endpoint, though the loop filters nothing by them: the host routes every payload by
    /// the recipient's current protocol. The loop adds no `Allow-Arbitrary-Tag-Value` header itself: the hosted
    /// request mapper adds it at dispatch while the recipient's live protocol asks for it.
    pub(crate) fn for_recipient(&self, settings: &DeliverySettings, spec: &RecipientSpec) -> Self {
        let retry = &settings.retry;
        let queue_max_size_bytes = spec.policy.queue_bytes.map_or(retry.queue_max_size_bytes, |bound| {
            bound.min(retry.queue_max_size_bytes)
        });
        let storage_max_size_bytes = match spec.policy.spill {
            SpillPolicy::Native => retry.storage_max_size_bytes,
            SpillPolicy::Never => 0,
        };
        let forwarder = shared::Forwarder {
            backoff_base: retry.backoff_base,
            backoff_factor: retry.backoff_factor,
            backoff_max: retry.backoff_max,
            recovery_interval: retry.recovery_error_decrease_factor,
            recovery_reset: retry.recovery_reset,
            retry_queue_payloads_max_size: ConfigValue::explicit(queue_max_size_bytes),
            storage_max_size_in_bytes: storage_max_size_bytes,
            flush_to_disk_mem_ratio: retry.flush_to_disk_mem_ratio,
            storage_max_disk_ratio: retry.storage_max_disk_ratio,
            outdated_file_in_days: retry.outdated_file_in_days,
            ..self.forwarder.clone()
        };
        Self {
            endpoint_concurrency: settings.endpoint_concurrency,
            endpoint_concurrency_multiplier: settings.endpoint_concurrency_multiplier,
            request_timeout_secs: settings.request_timeout.as_secs(),
            endpoint_buffer_size: settings.endpoint_buffer_size,
            retry: RetryConfiguration::from_configuration(&forwarder, self.run_path.as_deref()),
            http_protocol: settings.http_protocol,
            connection_reset_interval_secs: settings.connection_reset_interval.as_secs(),
            skip_ssl_validation: settings.skip_ssl_validation,
            min_tls_version: settings.min_tls_version,
            tls_handshake_timeout: settings.tls_handshake_timeout,
            allow_arbitrary_tags: false,
            api_key_validation_interval: settings.api_key_validation_interval,
            v3_acceptance: Some(spec.protocol.v3()),
            forwarder,
            ..self.clone()
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

    /// Returns the allowlist policy identity without changing the endpoint's delivery or protocol settings.
    pub(crate) fn metrics_policy_endpoint<'a>(
        &'a self, route: EndpointRoute, endpoint: &'a ResolvedEndpoint,
    ) -> &'a str {
        match route {
            EndpointRoute::MetricsPrimary => self.endpoint.primary_endpoint(),
            EndpointRoute::Primary | EndpointRoute::Additional => endpoint.configured_endpoint(),
        }
    }

    /// Builds resolved endpoints with routing metadata.
    ///
    /// Each endpoint starts with the key its configuration resolved to. A forwarder may bind it to a live
    /// configuration view through [`ApiKeyRefresher`][super::api_key::ApiKeyRefresher].
    pub(crate) fn build_routable_endpoints(&self) -> Result<Vec<RoutableEndpoint>, GenericError> {
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

    /// Returns the endpoints that receive metrics payloads carrying the given endpoint routing, in the order the
    /// forwarder's I/O loop sends to them.
    ///
    /// This applies the same decisions the I/O loop makes for each metrics request: the alternate metrics intake
    /// replaces the primary endpoint when one is configured, and the routing, when present, selects endpoints by their
    /// [policy identity][Self::metrics_policy_endpoint].
    ///
    /// # Errors
    ///
    /// Returns an error if an endpoint cannot be resolved, as building the forwarder does.
    pub(crate) fn metrics_recipients(
        &self, routing: Option<&MetricsEndpointRouting>,
    ) -> Result<Vec<MetricsRecipient>, GenericError> {
        let endpoints = self.build_routable_endpoints()?;
        let has_metrics_primary = endpoints
            .iter()
            .any(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary);
        Ok(endpoints
            .iter()
            .filter(|endpoint| should_route_to_endpoint(true, has_metrics_primary, endpoint.route()))
            .filter(|endpoint| {
                routing.is_none_or(|routing| {
                    routing.should_route_to(self.metrics_policy_endpoint(endpoint.route(), endpoint.endpoint()))
                })
            })
            .map(|endpoint| MetricsRecipient {
                route: match endpoint.route() {
                    EndpointRoute::Primary => MetricsRecipientRoute::Primary,
                    EndpointRoute::MetricsPrimary => MetricsRecipientRoute::MetricsOverride,
                    EndpointRoute::Additional => MetricsRecipientRoute::Additional,
                },
                configured_endpoint: endpoint.endpoint().configured_endpoint().to_string(),
                api_key_index: endpoint.endpoint().api_key_index(),
                classes: match endpoint.route() {
                    EndpointRoute::Primary | EndpointRoute::Additional => PayloadClasses::All,
                    EndpointRoute::MetricsPrimary => PayloadClasses::MetricsOnly,
                },
                protocol: self.datadog_intake_protocol(endpoint.route(), endpoint.endpoint()),
            })
            .collect())
    }

    /// Returns the protocol of a Datadog intake endpoint as this forwarder's I/O loop sends to it: its logs and traces
    /// authorities, whether requests allow arbitrary tag values, and which metrics variants it takes in their V3 form.
    pub(crate) fn datadog_intake_protocol(
        &self, route: EndpointRoute, endpoint: &ResolvedEndpoint,
    ) -> RecipientProtocol {
        let v3 = self.endpoint_v3_settings(route, endpoint.configured_endpoint());
        let authority = |authority: Option<&http::uri::Authority>| authority.map(|authority| authority.as_str().into());
        RecipientProtocol::DatadogIntake {
            authorities: IntakeAuthorities {
                logs: authority(endpoint.logs_authority()),
                traces: authority(endpoint.traces_authority()),
            },
            allow_arbitrary_tags: self.allow_arbitrary_tags,
            v3: V3Acceptance {
                series: v3.use_v3_series,
                sketches: v3.use_v3_sketches,
            },
        }
    }

    /// Returns which metrics variants an endpoint of the given route takes in their V3 form: those its protocol accepts
    /// for a recipient a delivery host runs; otherwise none when the serializer's compressor rules V3 out, the
    /// alternate intake's own setting for the alternate metrics intake, and the endpoint's series mode and whether the
    /// serializer lists it for V3 sketches.
    ///
    /// Matching uses the endpoint string from configuration, not the version-prefixed URL used for requests.
    pub(crate) fn endpoint_v3_settings(&self, route: EndpointRoute, configured_endpoint: &str) -> EndpointV3Settings {
        if let Some(v3) = self.v3_acceptance {
            return EndpointV3Settings {
                use_v3_series: v3.series,
                use_v3_sketches: v3.sketches,
            };
        }
        if self.compressor_disables_metrics_v3() {
            return EndpointV3Settings::disabled();
        }
        let metrics_primary_v3_override = (route == EndpointRoute::MetricsPrimary)
            .then(|| self.opw_metrics_v3_series_override())
            .flatten();
        EndpointV3Settings::from_v3_config(V3EndpointConfig {
            configured_endpoint,
            series_config: self.use_v3_api_series(),
            metrics_primary_v3_override,
            serializer_v3_sketches_endpoints: &self.v3_api().sketches.endpoints,
        })
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

    async fn forwarder_config_from(shared: SharedConfiguration) -> ForwarderConfiguration {
        ForwarderConfiguration::from_configuration(&shared)
    }

    fn endpoint_urls_by_route(config: &ForwarderConfiguration, route: EndpointRoute) -> Vec<String> {
        config
            .build_routable_endpoints()
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

        let primary = config
            .build_routable_endpoints()
            .expect("endpoint should resolve")
            .into_iter()
            .find(|endpoint| endpoint.route() == EndpointRoute::Primary)
            .expect("primary endpoint exists");
        assert_eq!("https://app.datadoghq.eu", primary.endpoint().configured_endpoint());
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
            accepts_v3_series: false,
        };
        let config = ForwarderConfiguration::for_single_destination(&shared, &destination);

        let endpoints = config.build_routable_endpoints().expect("endpoint should resolve");
        assert_eq!(1, endpoints.len());
        assert_eq!(EndpointRoute::Primary, endpoints[0].route());
        assert_eq!("https://only.example.com/", endpoints[0].endpoint().endpoint().as_str());
        assert_eq!("destination-api-key", &*endpoints[0].endpoint().api_key());

        // A destination that does not accept V3 series payloads gets none of the configured V3 routing.
        assert_eq!(V3SeriesMode::Disabled, config.use_v3_api_series().enabled);
        assert!(config.use_v3_api_series().endpoints.is_empty());
    }

    #[test]
    fn delivery_settings_are_the_native_settings_with_each_proxy_named_without_its_user_information() {
        let mut shared = shared_configuration();
        shared.endpoints.proxy = Proxy {
            http: "http://user:secret@proxy.example.com:3128".to_string(),
            https: "http://proxy.example.com:3129".to_string(),
            no_proxy: vec!["internal.example.com".to_string()],
            no_proxy_nonexact_match: true,
            use_proxy_for_cloud_metadata: false,
        };
        shared.endpoints.forwarder.storage_max_size_in_bytes = 4096;
        let config = ForwarderConfiguration::from_configuration(&shared);
        let settings = config.delivery_settings();

        let http = settings.proxy.http.expect("the http proxy is configured");
        assert_eq!(http.url, "http://proxy.example.com:3128/");
        assert_eq!(&*http.credential, "native:proxy.http");
        let https = settings.proxy.https.expect("the https proxy is configured");
        assert_eq!(https.url, "http://proxy.example.com:3129");
        assert_eq!(&*https.credential, "native:proxy.https");
        assert_eq!(settings.proxy.no_proxy, vec!["internal.example.com".to_string()]);
        assert!(settings.proxy.no_proxy_nonexact_match);
        assert_eq!(settings.endpoint_concurrency, 10);
        assert_eq!(settings.endpoint_buffer_size, 100);
        assert_eq!(settings.request_timeout, Duration::from_secs(20));
        assert_eq!(settings.retry.queue_max_size_bytes, 15 * 1024 * 1024);
        assert_eq!(settings.retry.storage_max_size_bytes, 4096);
        assert_eq!(settings.retry.backoff_max, 64.0);
        assert_eq!(settings.api_key_validation_interval, Duration::from_mins(60));
    }

    #[test]
    fn a_hosted_recipients_loop_runs_with_the_host_settings_narrowed_by_its_policy() {
        use crate::common::datadog::delivery::{
            RecipientOrigin, RecipientPolicy, RecipientProtocol, RecipientSpec, RetireClosure,
        };

        let mut shared = shared_configuration();
        shared.endpoints.forwarder.storage_max_size_in_bytes = 4096;
        shared.endpoints.allow_arbitrary_tags = true;
        shared.run_path = Some("/run/agent".into());
        let base = ForwarderConfiguration::from_configuration(&shared);
        let mut settings = base.delivery_settings();
        settings.endpoint_concurrency = 3;
        settings.request_timeout = Duration::from_secs(7);
        settings.retry.queue_max_size_bytes = 1000;
        let spec = |queue_bytes, spill| RecipientSpec {
            id: "dd_out/declared/team/http://team.example.com".into(),
            endpoint: "http://team.example.com".to_string(),
            credential: "credentials/team".into(),
            classes: PayloadClasses::MetricsOnly,
            protocol: RecipientProtocol::DatadogIntake {
                authorities: IntakeAuthorities::default(),
                allow_arbitrary_tags: true,
                v3: V3Acceptance {
                    series: true,
                    sketches: false,
                },
            },
            policy: RecipientPolicy {
                queue_bytes,
                spill,
                on_retire: RetireClosure::Discard,
            },
            origin: RecipientOrigin::Declared {
                name: "team".into(),
                credential: "team".into(),
            },
        };

        let narrowed = base.for_recipient(&settings, &spec(Some(400), SpillPolicy::Never));
        assert_eq!(narrowed.endpoint_concurrency(), 3);
        assert_eq!(narrowed.request_timeout(), Duration::from_secs(7));
        assert_eq!(narrowed.retry().queue_max_size_bytes(), 400);
        assert_eq!(
            narrowed.retry().storage_max_size_bytes(),
            0,
            "a recipient that never spills has no disk"
        );
        assert_eq!(narrowed.retry().storage_path(), base.retry().storage_path());
        assert!(
            !narrowed.allow_arbitrary_tags(),
            "the hosted request mapper adds the header"
        );
        // The protocol's V3 acceptance replaces what V3 configuration says of the endpoint.
        let v3 = narrowed.endpoint_v3_settings(EndpointRoute::Additional, "http://team.example.com");
        assert!(v3.use_v3_series);
        assert!(!v3.use_v3_sketches);

        // A policy never raises the host's bound, and a native spill keeps the host's disk.
        let capped = base.for_recipient(&settings, &spec(Some(5000), SpillPolicy::Native));
        assert_eq!(capped.retry().queue_max_size_bytes(), 1000);
        assert_eq!(capped.retry().storage_max_size_bytes(), 4096);
        let unbounded = base.for_recipient(&settings, &spec(None, SpillPolicy::Native));
        assert_eq!(unbounded.retry().queue_max_size_bytes(), 1000);
    }

    #[tokio::test]
    async fn a_single_destination_keeps_configured_v3_series_routing_when_it_accepts_v3() {
        let mut shared = shared_configuration();
        shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled;
        let destination = SingleDestination {
            url: "https://mrf.example.com".to_string(),
            api_key: "mrf-api-key".to_string(),
            accepts_v3_series: true,
        };
        let config = ForwarderConfiguration::for_single_destination(&shared, &destination);

        assert_eq!(V3SeriesMode::Enabled, config.use_v3_api_series().enabled);
    }
}

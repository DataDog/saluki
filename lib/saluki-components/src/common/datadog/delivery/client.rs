//! A delivery host's HTTP client: one per host, built from its client profile.

use std::time::Duration;

use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpClient, HttpProtocol, TlsMinimumVersion};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tracing::warn;

use super::{host::EndpointName, DeliverySettings, ProxySettings};
use crate::common::datadog::{
    config::{ForwarderConfiguration, ForwarderHttpProtocol},
    proxy::ProxyConfiguration,
};

/// Everything a delivery host's client is built from: the settings that shape its connections, and the user
/// information of its proxies.
///
/// A host builds a new client when its profile changes, from a publication or from a proxy credential it follows, and
/// keeps the client it has when the profile is unchanged.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ClientProfile {
    request_timeout: Duration,
    idle_connections_per_host: usize,
    min_tls_version: TlsMinimumVersion,
    tls_handshake_timeout: Duration,
    http_protocol: ForwarderHttpProtocol,
    connection_reset_interval: Duration,
    skip_ssl_validation: bool,
    proxy: ProxySettings,
    proxy_userinfo: [Option<MetaString>; 2],
}

impl ClientProfile {
    /// Returns the profile of a client for `settings`, whose `http` and `https` proxies carry `proxy_userinfo`.
    pub(super) fn new(settings: &DeliverySettings, proxy_userinfo: [Option<MetaString>; 2]) -> Self {
        Self {
            request_timeout: settings.request_timeout,
            idle_connections_per_host: settings.endpoint_concurrency_multiplier,
            min_tls_version: settings.min_tls_version,
            tls_handshake_timeout: settings.tls_handshake_timeout,
            http_protocol: settings.http_protocol,
            connection_reset_interval: settings.connection_reset_interval,
            skip_ssl_validation: settings.skip_ssl_validation,
            proxy: settings.proxy.clone(),
            proxy_userinfo,
        }
    }
}

/// Builds a client for `profile`, as a native forwarder builds its one client, with the TLS key log of `base`.
///
/// # Errors
///
/// Returns an error if a proxy is invalid or the TLS settings cannot be applied.
pub(super) fn build_client(
    profile: &ClientProfile, base: &ForwarderConfiguration, metrics_builder: &MetricsBuilder,
    endpoint_name: &EndpointName,
) -> Result<HttpClient, GenericError> {
    let endpoint_name = EndpointName::clone(endpoint_name);
    let [http_userinfo, https_userinfo] = &profile.proxy_userinfo;
    let proxies =
        ProxyConfiguration::from_settings(&profile.proxy, http_userinfo.as_deref(), https_userinfo.as_deref())?
            .build()?;
    let mut builder = HttpClient::builder()
        .with_request_timeout(profile.request_timeout)
        .with_max_idle_conns_per_host(profile.idle_connections_per_host)
        .with_min_tls_version(profile.min_tls_version)
        .with_tls_handshake_timeout(profile.tls_handshake_timeout)
        .with_http_protocol(HttpProtocol::from(profile.http_protocol))
        .with_endpoint_telemetry(metrics_builder.clone(), Some(move |uri: &http::Uri| endpoint_name(uri)))
        .with_proxies(proxies);
    if let Some(path) = base.ssl_key_log_file_path() {
        builder = builder.with_tls_config(|tls| tls.with_key_log_file(path));
    }
    if profile.connection_reset_interval > Duration::ZERO {
        builder = builder.with_connection_age_limit(profile.connection_reset_interval);
    }
    if profile.skip_ssl_validation {
        #[cfg(feature = "fips")]
        warn!(
            config_key = "skip_ssl_validation",
            "Disabling TLS certificate validation in a FIPS build means server certificates are no longer verified, \
             even though the underlying TLS cipher suites and key exchange remain FIPS-compliant."
        );
        #[cfg(not(feature = "fips"))]
        warn!(
            config_key = "skip_ssl_validation",
            "TLS certificate validation is disabled for Datadog intake forwarding."
        );
        builder = builder.with_tls_config(|tls| tls.danger_accept_invalid_certs());
    }
    builder.build()
}

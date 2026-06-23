use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use facet::Facet;
use http::uri::Authority;
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, OneOrMany, PickFirst};
use snafu::{ResultExt, Snafu};
use tracing::{debug, warn};
use url::Url;

use super::protocol::{MetricsPayloadInfo, MetricsProtocolVersion, UseV3ApiSeriesConfig};

static DD_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap());
static DD_SITE_FROM_HOSTNAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\.)([a-z]{2,}\d{1,2}\.)?(datad(?:oghq|0g)\.(?:com|eu)|ddog-gov\.com)\.?$").unwrap()
});

pub const DEFAULT_SITE: &str = "datadoghq.com";

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
}

/// Per-endpoint V3 protocol settings.
///
/// These settings control which protocol versions an endpoint will accept for metrics payloads.
/// Settings are derived from a global `V3ApiConfig` by matching the endpoint URL against the
/// configured V3 endpoint lists.
#[derive(Clone, Debug, Default)]
pub struct EndpointV3Settings {
    /// Whether this endpoint accepts V3 series payloads.
    pub use_v3_series: bool,

    /// Whether this endpoint accepts V3 sketches payloads.
    pub use_v3_sketches: bool,

    /// Whether validation mode is enabled for series (send both V2 and V3).
    pub series_validation_mode: bool,

    /// Whether validation mode is enabled for sketches (send both V2 and V3).
    pub sketches_validation_mode: bool,

    /// Whether this endpoint accepts sampled V3 beta series shadow payloads.
    pub series_shadow_mode: bool,
}

/// Inputs used to derive V3 settings for one endpoint.
pub(crate) struct V3EndpointConfig<'a> {
    /// Endpoint string as it appeared in configuration for this routed endpoint.
    pub(crate) configured_endpoint: &'a str,
    /// Resolved endpoint URL.
    pub(crate) resolved_endpoint: &'a Url,
    /// Optional primary endpoint name used by serializer V3 endpoint-list matching.
    pub(crate) serializer_v3_configured_endpoint: Option<&'a str>,
    /// Whether the ADP V3 series safety gate is enabled.
    pub(crate) data_plane_v3_series_enabled: bool,
    /// Agent-compatible V3 series config.
    pub(crate) series_config: &'a UseV3ApiSeriesConfig,
    /// OPW/Vector route-specific V3 override.
    pub(crate) metrics_primary_v3_override: Option<bool>,
    /// Serializer V3 series endpoint list.
    pub(crate) serializer_v3_series_endpoints: &'a [String],
    /// Serializer V3 sketches endpoint list.
    pub(crate) serializer_v3_sketches_endpoints: &'a [String],
    /// Whether series validation mode is enabled.
    pub(crate) series_validate: bool,
    /// Whether sketches validation mode is enabled.
    pub(crate) sketches_validate: bool,
    /// Sites eligible for V3 series shadow traffic.
    pub(crate) series_shadow_sites: &'a [String],
}

impl EndpointV3Settings {
    /// Returns endpoint settings with all V3 routing disabled.
    pub const fn disabled() -> Self {
        Self {
            use_v3_series: false,
            use_v3_sketches: false,
            series_validation_mode: false,
            sketches_validation_mode: false,
            series_shadow_mode: false,
        }
    }

    /// Creates V3 settings for a specific endpoint based on URL matching.
    ///
    /// The `v3_series_endpoints` and `v3_sketches_endpoints` are lists of configured endpoint names.
    /// If the endpoint name matches any entry, V3 is enabled for that metric type.
    #[cfg(test)]
    pub fn from_endpoint_url(
        configured_endpoint: &str, resolved_endpoint: &Url, v3_series_endpoints: &[String],
        v3_sketches_endpoints: &[String], series_validate: bool, sketches_validate: bool,
        series_shadow_sites: &[String],
    ) -> Self {
        let use_v3_series = serializer_v3_config_matches_endpoint(configured_endpoint, v3_series_endpoints);
        let use_v3_sketches = v3_sketches_endpoints.iter().any(|e| configured_endpoint == e);
        let series_shadow_mode = !use_v3_series
            && extract_site_from_url(resolved_endpoint.as_str())
                .is_some_and(|site| series_shadow_sites.iter().any(|shadow_site| shadow_site == &site));

        Self {
            use_v3_series,
            use_v3_sketches,
            series_validation_mode: use_v3_series && series_validate,
            sketches_validation_mode: use_v3_sketches && sketches_validate,
            series_shadow_mode,
        }
    }

    /// Creates V3 settings using Agent-compatible series V3 configuration plus the ADP safety gate.
    ///
    /// `V3EndpointConfig::serializer_v3_configured_endpoint` lets metrics-primary OPW/Vector routes match
    /// `serializer_experimental_use_v3_api.series.endpoints` against the normal primary endpoint name, matching the
    /// Core Agent resolver behavior.
    pub fn from_v3_config(config: V3EndpointConfig<'_>) -> Self {
        let serializer_use_v3_series =
            serializer_v3_config_matches_endpoint(config.configured_endpoint, config.serializer_v3_series_endpoints)
                || config.serializer_v3_configured_endpoint.is_some_and(|endpoint| {
                    serializer_v3_config_matches_endpoint(endpoint, config.serializer_v3_series_endpoints)
                });
        let use_v3_series = config.data_plane_v3_series_enabled
            && if serializer_use_v3_series {
                true
            } else if let Some(metrics_primary_use_v3) = config.metrics_primary_v3_override {
                metrics_primary_use_v3
            } else if let Some(endpoint_value) = config.series_config.endpoints.get(config.configured_endpoint) {
                evaluate_series_v3_mode(
                    "use_v3_api.series.endpoints",
                    endpoint_value,
                    config.configured_endpoint,
                    Some(config.resolved_endpoint),
                )
            } else {
                evaluate_series_v3_mode(
                    "use_v3_api.series.enabled",
                    &config.series_config.enabled,
                    config.configured_endpoint,
                    Some(config.resolved_endpoint),
                )
            };

        let use_v3_sketches = config
            .serializer_v3_sketches_endpoints
            .iter()
            .any(|e| config.configured_endpoint == e);
        let series_shadow_mode = config.data_plane_v3_series_enabled
            && !use_v3_series
            && extract_site_from_url(config.resolved_endpoint.as_str()).is_some_and(|site| {
                config
                    .series_shadow_sites
                    .iter()
                    .any(|shadow_site| shadow_site == &site)
            });

        Self {
            use_v3_series,
            use_v3_sketches,
            series_validation_mode: use_v3_series && config.series_validate,
            sketches_validation_mode: use_v3_sketches && config.sketches_validate,
            series_shadow_mode,
        }
    }

    /// Determines if this endpoint should receive a payload with the given payload info.
    ///
    /// Returns `true` if the endpoint should receive the payload, `false` otherwise.
    ///
    /// The logic is:
    /// - V2 series payload: accept if series V3 is disabled OR series validation mode is enabled
    /// - V2 sketches payload: accept if sketches V3 is disabled OR sketches validation mode is enabled
    /// - V3 series payload: accept if series V3 is enabled
    /// - V3 sketches payload: accept if sketches V3 is enabled
    /// - Non-metrics payloads (None): always accept
    pub fn should_receive_payload(&self, payload_info: Option<MetricsPayloadInfo>) -> bool {
        let Some(info) = payload_info else {
            // No payload info - this is a non-metrics payload or legacy payload, always accept.
            return true;
        };

        let is_sketch = info.is_sketch();

        match info.version {
            MetricsProtocolVersion::V2 => {
                if is_sketch {
                    // V2 sketches: accept if V3 sketches is disabled OR validation mode is enabled
                    !self.use_v3_sketches || self.sketches_validation_mode
                } else {
                    // V2 series: accept if V3 series is disabled OR validation mode is enabled
                    !self.use_v3_series || self.series_validation_mode
                }
            }

            MetricsProtocolVersion::V3 => {
                if is_sketch {
                    // V3 sketches: accept if V3 sketches is enabled
                    self.use_v3_sketches
                } else if info.is_shadow() {
                    // V3 shadow series: accept only when this V2-authoritative endpoint is shadow-enabled.
                    self.series_shadow_mode
                } else {
                    // V3 series: accept if V3 series is enabled.
                    self.use_v3_series
                }
            }
        }
    }

    /// Determines if this endpoint should receive metrics validation headers.
    ///
    /// Validation headers are endpoint-scoped: they should only be sent to endpoints that are
    /// receiving both V2 and V3 payloads for the payload's metric family.
    pub fn should_receive_validation_headers(&self, payload_info: Option<MetricsPayloadInfo>) -> bool {
        let Some(info) = payload_info else {
            return false;
        };

        if info.is_shadow() {
            self.series_shadow_mode
        } else if info.is_sketch() {
            self.sketches_validation_mode
        } else {
            self.series_validation_mode
        }
    }
}

fn serializer_v3_config_matches_endpoint(configured_endpoint: &str, v3_series_endpoints: &[String]) -> bool {
    v3_series_endpoints
        .iter()
        .any(|endpoint| configured_endpoint == endpoint)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SeriesV3Mode {
    Enabled,
    Disabled,
    DatadogOnly,
    Invalid,
}

fn parse_series_v3_mode(value: &str) -> SeriesV3Mode {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed == "1"
        || trimmed.eq_ignore_ascii_case("t")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
    {
        SeriesV3Mode::Enabled
    } else if trimmed.eq_ignore_ascii_case("false")
        || trimmed == "0"
        || trimmed.eq_ignore_ascii_case("f")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.is_empty()
    {
        SeriesV3Mode::Disabled
    } else if trimmed.eq_ignore_ascii_case("datadog_only") {
        SeriesV3Mode::DatadogOnly
    } else {
        SeriesV3Mode::Invalid
    }
}

pub(crate) fn evaluate_series_v3_mode(
    config_key: &'static str, value: &str, configured_endpoint: &str, resolved_endpoint: Option<&Url>,
) -> bool {
    match parse_series_v3_mode(value) {
        SeriesV3Mode::Enabled => true,
        SeriesV3Mode::Disabled => false,
        SeriesV3Mode::DatadogOnly => {
            Url::parse(configured_endpoint).is_ok_and(|url| is_datadog_url(&url))
                || resolved_endpoint.is_some_and(is_datadog_url)
        }
        SeriesV3Mode::Invalid => {
            warn!(
                config_key,
                value, "Invalid V3 series mode value. Expected true, false, or datadog_only; treating as false."
            );
            false
        }
    }
}

pub(crate) fn series_v3_config_can_enable_v3(series_config: &UseV3ApiSeriesConfig) -> bool {
    if series_config
        .endpoints
        .iter()
        .any(|(endpoint, value)| evaluate_series_v3_mode("use_v3_api.series.endpoints", value, endpoint, None))
    {
        return true;
    }

    match parse_series_v3_mode(&series_config.enabled) {
        SeriesV3Mode::Enabled | SeriesV3Mode::DatadogOnly => true,
        SeriesV3Mode::Disabled => false,
        SeriesV3Mode::Invalid => {
            warn!(
                config_key = "use_v3_api.series.enabled",
                value = series_config.enabled,
                "Invalid V3 series mode value. Expected true, false, or datadog_only; treating as false."
            );
            false
        }
    }
}

pub(crate) fn is_datadog_url(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        .is_some_and(|host| DD_URL_REGEX.is_match(&host))
}

pub(crate) fn extract_site_from_url(raw_url: &str) -> Option<String> {
    let url = Url::parse(raw_url).ok()?;
    let hostname = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let captures = DD_SITE_FROM_HOSTNAME_REGEX.captures(&hostname)?;
    let datacenter = captures.get(1).map_or("", |m| m.as_str());
    let domain = captures.get(2)?.as_str();
    Some(format!("{datacenter}{domain}"))
}

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub(crate) enum EndpointError {
    Parse { source: url::ParseError, endpoint: String },
}

#[serde_as]
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
struct APIKeys(#[serde_as(as = "OneOrMany<_>")] Vec<String>);

#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
struct MappedAPIKeys(HashMap<String, APIKeys>);

impl MappedAPIKeys {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn mappings(&self) -> impl Iterator<Item = (&str, &APIKeys)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl FromStr for MappedAPIKeys {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let inner = serde_json::from_str(s)?;
        Ok(Self(inner))
    }
}

#[cfg(test)]
impl std::fmt::Display for MappedAPIKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::to_string(&self.0).unwrap_or_default())
    }
}

/// A set of additional API endpoints to forward metrics to.
///
/// Each endpoint can be associated with multiple API keys. Requests will be forwarded to each unique endpoint/API key pair.
#[serde_as]
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub(crate) struct AdditionalEndpoints(#[serde_as(as = "PickFirst<(DisplayFromStr, _)>")] MappedAPIKeys);

impl AdditionalEndpoints {
    /// Returns true if no additional endpoints are configured.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the resolved endpoints from the additional endpoint configuration.
    ///
    /// This will generate a [`ResolvedEndpoint`] for each unique endpoint/API key pair, assigning
    /// each endpoint an `api_key_index` equal to the raw position of its key in the config's key
    /// list for that URL (the `enumerate()` index, not a post-dedup counter). Empty and duplicate
    /// keys are skipped; their positions are consumed but no endpoint is created.
    ///
    /// # Errors
    ///
    /// If any of the additional endpoints aren't valid URLs, or a valid URL couldn't be constructed after applying
    /// the necessary normalization / modifications, an error will be returned.
    pub fn resolved_endpoints(
        &self, configuration: Option<GenericConfiguration>,
    ) -> Result<Vec<ResolvedEndpoint>, EndpointError> {
        let mut resolved = Vec::new();

        for (raw_endpoint, api_keys) in self.0.mappings() {
            let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
            let logs_authority = compute_logs_authority(&endpoint);
            let traces_authority = compute_traces_authority(&endpoint);

            // Create a resolved endpoint for each unique, non-empty key. The index is the raw
            // position in the config list so that live lookups can use `vec[index]` directly.
            let mut seen = HashSet::new();
            for (index, api_key) in api_keys.0.iter().enumerate() {
                let trimmed_api_key = api_key.trim();
                if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                    continue;
                }

                seen.insert(trimmed_api_key);
                resolved.push(ResolvedEndpoint {
                    endpoint: endpoint.clone(),
                    configured_endpoint: raw_endpoint.to_string(),
                    api_key: trimmed_api_key.to_string(),
                    config: configuration.clone(),
                    api_key_refresh_config_path: None,
                    api_key_index: Some(index),
                    raw_additional_url: Some(raw_endpoint.to_string()),
                    logs_authority: logs_authority.clone(),
                    traces_authority: traces_authority.clone(),
                });
            }
        }

        Ok(resolved)
    }
}

/// Endpoint configuration for sending payloads to the Datadog platform.
#[derive(Clone, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct EndpointConfiguration {
    /// The API key to use.
    api_key: String,

    /// Config path used to refresh the API key for primary-like endpoints.
    #[serde(skip)]
    api_key_refresh_config_path: Option<&'static str>,

    /// The site to send metrics to.
    ///
    /// This is the base domain for the Datadog site in which the API key originates from. This will generally be a
    /// portion of the domain used to access the Datadog UI, such as `datadoghq.com` or `us5.datadoghq.com`.
    ///
    /// Defaults to `datadoghq.com`.
    #[serde(default = "default_site")]
    site: String,

    /// The full URL base to send metrics to.
    ///
    /// This takes precedence over `site`, and isn't altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (for example, `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    #[serde(default, alias = "url")]
    dd_url: Option<String>,

    /// Enables sending data to multiple endpoints and/or with multiple API keys via dual shipping.
    ///
    /// Defaults to empty.
    #[serde(default)]
    additional_endpoints: AdditionalEndpoints,
}

impl EndpointConfiguration {
    /// Sets the full URL base to send metrics to.
    pub fn set_dd_url(&mut self, url: String) {
        self.dd_url = Some(url);
    }

    /// Sets the API key to use.
    pub fn set_api_key(&mut self, api_key: String) {
        self.api_key = api_key;
    }

    /// Sets the config path used to refresh the API key.
    pub fn set_api_key_refresh_config_path(&mut self, path: &'static str) {
        self.api_key_refresh_config_path = Some(path);
    }

    /// Clears all additional endpoints.
    pub fn clear_additional_endpoints(&mut self) {
        self.additional_endpoints = AdditionalEndpoints::default();
    }

    /// Builds the resolved primary endpoint from `site`/`dd_url`.
    ///
    /// # Errors
    ///
    /// If the primary endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to the endpoint, an error will be returned.
    pub(crate) fn build_primary_endpoint(
        &self, configuration: Option<GenericConfiguration>,
    ) -> Result<ResolvedEndpoint, GenericError> {
        calculate_resolved_endpoint(self.dd_url.as_deref(), &self.site, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")
            .map(|endpoint| endpoint.with_configuration(configuration))
            .map(|endpoint| endpoint.with_api_key_refresh_config_path(self.api_key_refresh_config_path))
    }

    /// Returns the configured primary endpoint string without resolving or version-prefixing it.
    pub(crate) fn configured_primary_endpoint(&self) -> String {
        match self.dd_url.as_deref() {
            Some(url) => url.to_string(),
            None => {
                let base_domain = if self.site.is_empty() { DEFAULT_SITE } else { &self.site };
                format!("https://app.{base_domain}")
            }
        }
    }

    /// Builds the resolved primary endpoint from a URL override.
    pub(crate) fn build_primary_endpoint_override(
        &self, url: &str, configuration: Option<GenericConfiguration>,
    ) -> Result<ResolvedEndpoint, EndpointError> {
        calculate_resolved_endpoint(Some(url), &self.site, &self.api_key)
            .map(|endpoint| endpoint.with_configuration(configuration))
            .map(|endpoint| endpoint.with_api_key_refresh_config_path(self.api_key_refresh_config_path))
    }

    /// Builds the resolved additional endpoints.
    ///
    /// If a [`GenericConfiguration`] is supplied, each additional endpoint will hold a live
    /// reference to it and refresh its API key on every request via [`ResolvedEndpoint::api_key`].
    ///
    /// # Errors
    ///
    /// If any additional endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to a particular endpoint, an error will be returned.
    pub(crate) fn build_additional_endpoints(
        &self, configuration: Option<GenericConfiguration>,
    ) -> Result<Vec<ResolvedEndpoint>, GenericError> {
        self.additional_endpoints
            .resolved_endpoints(configuration)
            .error_context("Failed parsing/resolving the additional destination endpoints.")
    }
}

/// A single API endpoint and its associated API key.
///
/// An endpoint is defined as a unique, fully qualified domain name that metrics will be sent to, such as
/// `https://app.datadoghq.com`.
#[derive(Clone, Debug)]
pub struct ResolvedEndpoint {
    endpoint: Url,
    configured_endpoint: String,
    api_key: String,
    config: Option<GenericConfiguration>,
    /// Config path used to refresh the API key for primary-like endpoints. `None` uses `api_key`.
    api_key_refresh_config_path: Option<&'static str>,
    /// Position of this key in the `additional_endpoints` config key list for its URL (raw
    /// `enumerate()` index, not a post-dedup counter). `None` for primary and OPW endpoints.
    api_key_index: Option<usize>,
    /// The raw (pre-normalization) URL string from `additional_endpoints`, used as the HashMap
    /// key for live API key lookups. `None` for primary and OPW endpoints.
    raw_additional_url: Option<String>,
    /// Pre-computed logs intake authority (for example, `agent-http-intake.logs.datadoghq.com`).
    /// This is derived from the endpoint host when it contains `.agent.` marker.
    logs_authority: Option<Authority>,
    /// Pre-computed traces intake authority (for example, `trace.agent.datadoghq.com`).
    /// This is derived from the endpoint host when it contains `.agent.` marker.
    traces_authority: Option<Authority>,
}

/// Routing role for a resolved endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndpointRoute {
    /// The normal primary Datadog endpoint.
    Primary,
    /// The OPW metrics primary endpoint.
    MetricsPrimary,
    /// A configured dual-shipping endpoint.
    Additional,
}

/// A resolved endpoint with routing metadata.
#[derive(Clone, Debug)]
pub(crate) struct RoutableEndpoint {
    route: EndpointRoute,
    endpoint: ResolvedEndpoint,
}

impl RoutableEndpoint {
    /// Creates a new routable endpoint.
    pub(crate) const fn new(route: EndpointRoute, endpoint: ResolvedEndpoint) -> Self {
        Self { route, endpoint }
    }

    /// Returns the routing role.
    pub(crate) const fn route(&self) -> EndpointRoute {
        self.route
    }

    /// Returns the resolved endpoint.
    #[cfg(test)]
    pub(crate) const fn endpoint(&self) -> &ResolvedEndpoint {
        &self.endpoint
    }

    /// Consumes the routable endpoint and returns its parts.
    pub(crate) fn into_parts(self) -> (EndpointRoute, ResolvedEndpoint) {
        (self.route, self.endpoint)
    }
}

impl ResolvedEndpoint {
    /// Creates a new `ResolvedEndpoint` instance from the given endpoint and API key, normalizing and modifying the
    /// endpoint as necessary.
    ///
    /// # Errors
    ///
    /// If the given endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the necessary
    /// normalization / modifications, an error will be returned.
    pub(crate) fn from_raw_endpoint(raw_endpoint: &str, api_key: &str) -> Result<Self, EndpointError> {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        let logs_authority = compute_logs_authority(&endpoint);
        let traces_authority = compute_traces_authority(&endpoint);
        Ok(Self {
            endpoint,
            configured_endpoint: raw_endpoint.to_string(),
            api_key: api_key.to_string(),
            config: None,
            api_key_refresh_config_path: None,
            api_key_index: None,
            raw_additional_url: None,
            logs_authority,
            traces_authority,
        })
    }

    /// Creates a new  `ResolvedEndpoint` instance from an existing `ResolvedEndpoint`, adding an optional `GenericConfiguration` which can be used to fetch the up-to-date API key.
    pub fn with_configuration(mut self, config: Option<GenericConfiguration>) -> Self {
        self.config = config;
        self
    }

    /// Sets the config path used to refresh the API key for primary-like endpoints.
    pub(crate) fn with_api_key_refresh_config_path(
        mut self, api_key_refresh_config_path: Option<&'static str>,
    ) -> Self {
        self.api_key_refresh_config_path = api_key_refresh_config_path;
        self
    }

    /// Returns the endpoint of the resolver.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the endpoint string as it was provided by configuration.
    ///
    /// Unlike [`ResolvedEndpoint::endpoint`], this is not rewritten with the data plane version prefix.
    pub fn configured_endpoint(&self) -> &str {
        &self.configured_endpoint
    }

    /// Returns the API key associated with the endpoint.
    ///
    /// If a [`GenericConfiguration`] has been configured, the API key will be queried from the configuration and
    /// stored if it has been updated since the last time `api_key` was called.
    ///
    /// For additional endpoints (those with an [`api_key_index`][Self::api_key_index]), the key is
    /// looked up by position in the `additional_endpoints` config value. For the primary endpoint,
    /// the `api_key` config key is used directly.
    pub fn api_key(&mut self) -> &str {
        if let Some(config) = &self.config {
            if let (Some(index), Some(raw_url)) = (self.api_key_index, self.raw_additional_url.as_deref()) {
                // Additional endpoint: look up current key by raw index in this URL's key list.
                match lookup_additional_key(config, raw_url, index) {
                    Some(key) if key != self.api_key => {
                        debug!(endpoint = %self.endpoint, index, "Refreshed additional endpoint API key.");
                        self.api_key = key;
                    }
                    None => {
                        debug!(
                            endpoint = %self.endpoint,
                            index,
                            "Could not refresh additional endpoint key from config (index out of range or \
                             parse error). Continuing with last known valid API key."
                        );
                    }
                    _ => {}
                }
            } else {
                // Primary / OPW endpoint: refresh from the configured API key source.
                let api_key_refresh_config_path = self.api_key_refresh_config_path.unwrap_or("api_key");
                match config.try_get_typed::<String>(api_key_refresh_config_path) {
                    Ok(Some(api_key)) => {
                        if !api_key.is_empty() && self.api_key != api_key {
                            debug!(endpoint = %self.endpoint, key = api_key_refresh_config_path, "Refreshed API key.");
                            self.api_key = api_key;
                        }
                    }
                    Ok(None) | Err(_) => {
                        debug!(
                            key = api_key_refresh_config_path,
                            "Failed to retrieve API key from remote source (missing or wrong type). Continuing with \
                             last known valid API key."
                        );
                    }
                }
            }
        }
        self.api_key.as_str()
    }

    /// Returns the API key associated with the endpoint without refreshing it.
    #[cfg(test)]
    pub fn cached_api_key(&self) -> &str {
        self.api_key.as_str()
    }

    /// Returns the position of this endpoint's API key in the `additional_endpoints` config list for
    /// its URL. `None` for primary and OPW endpoints.
    #[cfg(test)]
    pub(crate) fn api_key_index(&self) -> Option<usize> {
        self.api_key_index
    }

    /// Returns the raw (pre-normalization) URL and key index for additional endpoints.
    ///
    /// Using the raw URL in queue IDs prevents collisions when two different raw URLs (for example,
    /// `app.datadoghq.com` and `https://app.datadoghq.com`) normalize to the same host.
    /// Returns `None` for primary and OPW endpoints.
    pub(crate) fn additional_endpoint_queue_key(&self) -> Option<(&str, usize)> {
        match (self.raw_additional_url.as_deref(), self.api_key_index) {
            (Some(raw_url), Some(index)) => Some((raw_url, index)),
            _ => None,
        }
    }

    /// Returns whether this endpoint can refresh its API key from dynamic configuration.
    #[cfg(test)]
    pub(crate) fn has_configuration(&self) -> bool {
        self.config.is_some()
    }

    /// Returns whether this endpoint has an `api_key_index` (that is, is an additional endpoint).
    #[cfg(test)]
    pub(crate) fn has_api_key_index(&self) -> bool {
        self.api_key_index.is_some()
    }

    /// Returns the pre-computed logs intake authority, if available.
    ///
    /// This authority is derived from the endpoint host when it contains the `.agent.` marker,
    /// and is used for routing log payloads to the appropriate logs intake host.
    pub fn logs_authority(&self) -> Option<&Authority> {
        self.logs_authority.as_ref()
    }

    /// Returns the pre-computed traces intake authority, if available.
    pub fn traces_authority(&self) -> Option<&Authority> {
        self.traces_authority.as_ref()
    }
}

fn endpoint_with_default_scheme(raw_endpoint: &str) -> String {
    if !raw_endpoint.starts_with("http://") && !raw_endpoint.starts_with("https://") {
        format!("https://{}", raw_endpoint)
    } else {
        raw_endpoint.to_string()
    }
}

fn parse_and_normalize_endpoint(raw_endpoint: &str) -> Result<Url, EndpointError> {
    // Start out by parsing the given domain/endpoint, which means ensuring first that it has a scheme.
    //
    // If no scheme is present, we assume HTTPS.
    let raw_endpoint = endpoint_with_default_scheme(raw_endpoint);

    let endpoint = Url::parse(&raw_endpoint).context(Parse { endpoint: raw_endpoint })?;

    // With our valid endpoint URL, we'll optionally prefix it with a subdomain that represents the data plane version,
    // which differentiates the traffic between different versions of the data plane application.
    //
    // This prefixing only occurs for official Datadog API endpoints.
    add_data_plane_version_prefix(endpoint)
}

/// Returns a specialized domain prefix based on the versioning of the current application.
///
/// This generates a prefix that's similar in format to the one generated by Datadog Agent for determining the endpoint
/// to send metrics to.
fn get_data_plane_version_prefix() -> String {
    let app_details = saluki_metadata::get_app_details();
    let version = app_details.version();
    format!(
        "{}-{}-{}-{}.agent",
        version.major(),
        version.minor(),
        version.patch(),
        app_details.identifier(),
    )
}

/// Prefixes the given API endpoint with the version of the data plane process.
///
/// If the given API endpoint doesn't include a scheme, `https` is assumed. As well, if the endpoint doesn't represent
/// an official Datadog API endpoint, it won't be modified.
///
/// # Errors
///
/// If the given API endpoint can't be parsed as a valid URL, an error will be returned.
fn add_data_plane_version_prefix(mut endpoint: Url) -> Result<Url, EndpointError> {
    let new_host = match endpoint.host_str() {
        Some(host) => {
            // Do not update non-official Datadog URLs.
            if !DD_URL_REGEX.is_match(host) {
                debug!("Configured endpoint '{}' appears to be a non-Datadog endpoint. Utilizing endpoint without modification.", host);
                return Ok(endpoint);
            }

            // We expect to be getting a domain that has at least one subdomain portion (i.e., `app.datadoghq.com`) if
            // not more. We're aiming to simply replace the leftmost subdomain portion with the version prefix.
            let leftmost_segment = host.split('.').next().unwrap_or("");
            let versioned_segment = get_data_plane_version_prefix();
            host.replacen(leftmost_segment, &versioned_segment, 1)
        }
        None => {
            return Err(EndpointError::Parse {
                source: url::ParseError::EmptyHost,
                endpoint: endpoint.to_string(),
            })
        }
    };

    // Update the host with the prefixed version.
    if let Err(e) = endpoint.set_host(Some(new_host.as_str())) {
        return Err(EndpointError::Parse {
            source: e,
            endpoint: endpoint.to_string(),
        });
    }

    Ok(endpoint)
}

/// Calculates the correct API endpoint to use based on the given override URL and site settings.
///
/// # Errors
///
/// If an override URL is provided and can't be parsed, or if a valid endpoint can't be constructed from the given
/// site, an error will be returned.
fn calculate_resolved_endpoint(
    override_url: Option<&str>, site: &str, api_key: &str,
) -> Result<ResolvedEndpoint, EndpointError> {
    let raw_endpoint = match override_url {
        // If an override URL is provided, use it directly.
        Some(url) => url.to_string(),
        None => {
            // When using the site, we'll provide the default US-based site if the site value is empty.
            //
            // We also do a little bit of prefixing to get it in the right shape before creating the resolved endpoint.
            let base_domain = if site.is_empty() { DEFAULT_SITE } else { site };
            format!("https://app.{}", base_domain)
        }
    };

    ResolvedEndpoint::from_raw_endpoint(&raw_endpoint, api_key)
}

/// Returns the API key at position `index` in `raw_url`'s key list from the live config.
///
/// `raw_url` is the pre-normalization URL string (for example `"app.datadoghq.eu"`) as it appears as a
/// key in the `additional_endpoints` config value. `index` is the raw `enumerate()` position of
/// the key in that URLs list (not a post-dedup counter).
///
/// Returns `None` if the URL is not present in the current config, if `index` is out of range, or
/// if the key at that position is empty.
fn lookup_additional_key(config: &GenericConfiguration, raw_url: &str, index: usize) -> Option<String> {
    let additional = config
        .try_get_typed::<AdditionalEndpoints>("additional_endpoints")
        .ok()??
        .0;
    let key = additional.0.get(raw_url)?.0.get(index)?.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// Computes the logs intake authority from a resolved endpoint URL.
///
/// If the endpoint host contains the `.agent.` marker (for example, `7-52-0-adp.agent.datadoghq.com`),
/// this extracts the site suffix and constructs the logs intake host in the form
/// `agent-http-intake.logs.{site}`.
///
/// Returns `None` if the host doesn't contain the marker or if the authority can't be parsed.
fn compute_logs_authority(endpoint: &Url) -> Option<Authority> {
    const AGENT_HOST_MARKER: &str = ".agent.";

    let host = endpoint.host_str()?;
    let idx = host.find(AGENT_HOST_MARKER)?;
    let site = &host[idx + AGENT_HOST_MARKER.len()..];
    let logs_host = format!("agent-http-intake.logs.{}", site);

    Authority::from_str(&logs_host).ok()
}

/// Computes the traces intake authority from a resolved endpoint URL.
/// Returns `None` if the host doesn't contain the marker or if the authority can't be parsed.
fn compute_traces_authority(endpoint: &Url) -> Option<Authority> {
    const AGENT_HOST_MARKER: &str = ".agent.";

    let host = endpoint.host_str()?;
    let idx = host.find(AGENT_HOST_MARKER)?;
    let site = &host[idx + AGENT_HOST_MARKER.len()..];
    let traces_host = format!("trace.agent.{}", site);

    Authority::from_str(&traces_host).ok()
}

#[cfg(test)]
mod tests {
    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};

    use super::*;

    fn additional_endpoints_to_sorted_strings(endpoints: &AdditionalEndpoints) -> Vec<String> {
        let mut flattened = endpoints
            .0
            .mappings()
            .flat_map(|(domain, api_keys)| api_keys.0.iter().map(move |api_key| format!("{}:{}", domain, api_key)))
            .collect::<Vec<String>>();
        flattened.sort();
        flattened
    }

    #[test]
    fn deser_additional_endpoints_json_direct_mapping() {
        let raw_input = r#""{\"app.datadoghq.com\":\"fake-api-key-1\",\"app.datadoghq.eu\":\"fake-api-key-2\"}""#;

        let result = serde_yaml::from_str::<AdditionalEndpoints>(raw_input)
            .expect("should not fail to deserialize AdditionalEndpoints from JSON string");

        let expected = vec!["app.datadoghq.com:fake-api-key-1", "app.datadoghq.eu:fake-api-key-2"];
        let actual = additional_endpoints_to_sorted_strings(&result);
        assert_eq!(expected, actual);
    }

    #[test]
    fn deser_additional_endpoints_json_multiple_api_keys() {
        let raw_input = r#""{\"app.datadoghq.com\":[\"fake-api-key-1a\",\"fake-api-key-1b\"],\"app.datadoghq.eu\":[\"fake-api-key-2a\",\"fake-api-key-2b\"]}""#;

        let result = serde_yaml::from_str::<AdditionalEndpoints>(raw_input)
            .expect("should not fail to deserialize AdditionalEndpoints from JSON string");

        let expected = vec![
            "app.datadoghq.com:fake-api-key-1a",
            "app.datadoghq.com:fake-api-key-1b",
            "app.datadoghq.eu:fake-api-key-2a",
            "app.datadoghq.eu:fake-api-key-2b",
        ];
        let actual = additional_endpoints_to_sorted_strings(&result);
        assert_eq!(expected, actual);
    }

    #[test]
    fn deser_additional_endpoints_direct_mapping() {
        let raw_input = "app.datadoghq.com: fake-api-key-1\napp.datadoghq.eu: fake-api-key-2";

        let result = serde_yaml::from_str::<AdditionalEndpoints>(raw_input)
            .expect("should not fail to deserialize AdditionalEndpoints from YAML string");

        let expected = vec!["app.datadoghq.com:fake-api-key-1", "app.datadoghq.eu:fake-api-key-2"];
        let actual = additional_endpoints_to_sorted_strings(&result);
        assert_eq!(expected, actual);
    }

    #[test]
    fn deser_additional_endpoints_multiple_api_keys() {
        let raw_input = "app.datadoghq.com:\n  - fake-api-key-1a\n  - fake-api-key-1b\napp.datadoghq.eu:\n  - fake-api-key-2a\n  - fake-api-key-2b";

        let result = serde_yaml::from_str::<AdditionalEndpoints>(raw_input)
            .expect("should not fail to deserialize AdditionalEndpoints from YAML string");

        let expected = vec![
            "app.datadoghq.com:fake-api-key-1a",
            "app.datadoghq.com:fake-api-key-1b",
            "app.datadoghq.eu:fake-api-key-2a",
            "app.datadoghq.eu:fake-api-key-2b",
        ];
        let actual = additional_endpoints_to_sorted_strings(&result);
        assert_eq!(expected, actual);
    }

    #[test]
    fn additional_endpoints_api_key_index_uses_raw_config_position() {
        // Keys at positions 0, 1 are valid; position 2 is empty (skipped); position 3 is a
        // duplicate of position 0 (skipped); position 4 is valid. Only positions 0, 1, 4 produce
        // ResolvedEndpoints, and their api_key_index should be 0, 1, 4 respectively.
        let raw_input = r#""app.datadoghq.com": ["key-a", "key-b", "", "key-a", "key-c"]"#;
        let raw_yaml = format!("{{{}}}", raw_input);
        let endpoints: AdditionalEndpoints = serde_yaml::from_str(&raw_yaml).expect("should deserialize");

        let resolved = endpoints.resolved_endpoints(None).expect("should resolve");

        assert_eq!(
            resolved.len(),
            3,
            "should have 3 endpoints (skipping empty and duplicate)"
        );
        assert_eq!(resolved[0].cached_api_key(), "key-a");
        assert_eq!(resolved[0].api_key_index(), Some(0));
        assert_eq!(resolved[1].cached_api_key(), "key-b");
        assert_eq!(resolved[1].api_key_index(), Some(1));
        assert_eq!(resolved[2].cached_api_key(), "key-c");
        assert_eq!(
            resolved[2].api_key_index(),
            Some(4),
            "index 4 — not 2 — because original positions are used"
        );

        // Two URLs have independent index spaces (both start from 0).
        let raw_yaml2 = r#"app.datadoghq.eu: [eu-key-a, eu-key-b]"#;
        let endpoints2: AdditionalEndpoints = serde_yaml::from_str(raw_yaml2).expect("should deserialize");
        let resolved2 = endpoints2.resolved_endpoints(None).expect("should resolve");
        assert_eq!(resolved2[0].api_key_index(), Some(0));
        assert_eq!(resolved2[1].api_key_index(), Some(1));
    }

    #[tokio::test]
    async fn api_key_dynamically_refreshes_from_additional_endpoints_config() {
        use std::time::{Duration, Instant};

        // No static initial values for additional_endpoints — all from dynamic config only.
        // This avoids figment's admerge concatenating the static array with the dynamic array,
        // which would leave the old key-1 in position 0.
        let (config, sender) = ConfigurationLoader::for_tests(None, None, true).await;
        let sender = sender.expect("dynamic configuration sender should be present");

        // Apply an initial snapshot with key-1 and wait for readiness.
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({
                "additional_endpoints": { "http://extra.example.com": ["key-1"] }
            })))
            .await
            .expect("should send initial snapshot");
        config.ready().await;

        // Build the additional endpoint with a live config reference.
        let raw = r#"http://extra.example.com: [key-1]"#;
        let additional: AdditionalEndpoints = serde_yaml::from_str(raw).expect("should deserialize");
        let mut endpoints = additional
            .resolved_endpoints(Some(config.clone()))
            .expect("should resolve");
        let endpoint = &mut endpoints[0];

        // Before the update, api_key() returns the original key.
        assert_eq!(endpoint.api_key(), "key-1");

        // Push a snapshot that rotates the key.
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({
                "additional_endpoints": { "http://extra.example.com": ["key-2"] }
            })))
            .await
            .expect("should send rotation snapshot");

        // Poll api_key() until it reflects the new value; api_key() re-reads from live config on
        // every call so no watcher or rebuild is needed.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if endpoint.api_key() == "key-2" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out — api_key() did not refresh after additional_endpoints rotation"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn add_version_prefix() {
        let input_urls = [
            "https://app.datadoghq.com",     // US
            "https://app.datadoghq.eu",      // EU
            "app.ddog-gov.com",              // Gov
            "app.us2.datadoghq.com",         // Additional Site
            "https://app.xx9.datadoghq.com", // Arbitrary site
        ];
        let expected_hosts = [
            "datadoghq.com",
            "datadoghq.eu",
            "ddog-gov.com",
            "us2.datadoghq.com",
            "xx9.datadoghq.com",
        ]
        .iter()
        .map(|s| format!("{}.{}", get_data_plane_version_prefix(), s))
        .collect::<Vec<_>>();

        for (input_url, expected_host) in input_urls.iter().zip(expected_hosts) {
            let resolved =
                ResolvedEndpoint::from_raw_endpoint(input_url, "fake_api_key").expect("error resolving endpoint");
            assert_eq!(
                expected_host,
                resolved.endpoint().host_str().expect("error getting host")
            );
        }
    }

    #[test]
    fn skip_version_prefix() {
        let input_urls = [
            "https://custom.datadoghq.com",       // Custom
            "https://custom.agent.datadoghq.com", // Custom with 'agent' subdomain
            "https://app.custom.datadoghq.com",   // Custom
            "https://app.datadoghq.internal",     // Custom top-level domain
            "https://app.myproxy.com",            // Proxy
        ];
        let expected_hosts = [
            "custom.datadoghq.com",
            "custom.agent.datadoghq.com",
            "app.custom.datadoghq.com",
            "app.datadoghq.internal",
            "app.myproxy.com",
        ];

        for (input_url, expected_host) in input_urls.iter().zip(expected_hosts) {
            let resolved =
                ResolvedEndpoint::from_raw_endpoint(input_url, "fake_api_key").expect("error resolving endpoint");
            assert_eq!(
                expected_host,
                resolved.endpoint().host_str().expect("error getting host")
            );
        }
    }

    #[test]
    fn calculate_api_endpoint_no_override_no_site() {
        let prefix = get_data_plane_version_prefix();
        let expected_endpoint = format!("https://{}.{}/", prefix, DEFAULT_SITE);

        let resolved = calculate_resolved_endpoint(None, "", "").expect("error calculating default API endpoint");
        assert_eq!(expected_endpoint, resolved.endpoint().to_string());
    }

    #[test]
    fn calculate_api_endpoint_no_override() {
        let site = "us3.datadoghq.com";
        let prefix = get_data_plane_version_prefix();
        let expected_endpoint = format!("https://{}.{}/", prefix, site);

        let resolved =
            calculate_resolved_endpoint(None, "us3.datadoghq.com", "").expect("error calculating custom API endpoint");
        assert_eq!(expected_endpoint, resolved.endpoint().to_string());
    }

    #[test]
    fn calculate_api_endpoint_no_site() {
        let override_url = "https://dogpound.io/";
        let expected_endpoint = override_url;

        let resolved =
            calculate_resolved_endpoint(Some(override_url), "", "").expect("error calculating override API endpoint");
        assert_eq!(expected_endpoint, resolved.endpoint().to_string());
    }

    #[test]
    fn calculate_api_endpoint_override_and_site() {
        let override_url = "https://dogpound.io/";
        let expected_endpoint = override_url;

        let resolved = calculate_resolved_endpoint(Some(override_url), "us3.datadoghq.com", "")
            .expect("error calculating override API endpoint");
        assert_eq!(expected_endpoint, resolved.endpoint().to_string());
    }

    #[test]
    fn validation_headers_are_scoped_to_payload_family() {
        let settings = EndpointV3Settings {
            use_v3_series: true,
            use_v3_sketches: false,
            series_validation_mode: true,
            sketches_validation_mode: false,
            series_shadow_mode: false,
        };

        assert!(settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v2_series())));
        assert!(settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v3_series())));
        assert!(!settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v2_sketches())));
        assert!(!settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v3_sketches())));
        assert!(!settings.should_receive_validation_headers(None));
    }

    #[test]
    fn extract_site_from_url_matches_datadog_domains() {
        assert_eq!(
            Some("datadoghq.com".to_string()),
            extract_site_from_url("https://1-2-3-agent.datadoghq.com/api/v2/series")
        );
        assert_eq!(
            Some("us3.datadoghq.com".to_string()),
            extract_site_from_url("https://intake.profile.us3.datadoghq.com/v1/input")
        );
        assert_eq!(None, extract_site_from_url("https://vector.example.test/api/v2/series"));
    }

    #[test]
    fn shadow_payloads_are_endpoint_scoped() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let settings = EndpointV3Settings::from_endpoint_url(
            resolved.configured_endpoint(),
            resolved.endpoint(),
            &[],
            &[],
            false,
            false,
            &["datadoghq.com".to_string()],
        );

        assert!(settings.should_receive_payload(Some(MetricsPayloadInfo::v2_shadow_series())));
        assert!(settings.should_receive_payload(Some(MetricsPayloadInfo::v3_shadow_series())));
        assert!(!settings.should_receive_payload(Some(MetricsPayloadInfo::v3_series())));
        assert!(settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v2_shadow_series())));
        assert!(settings.should_receive_validation_headers(Some(MetricsPayloadInfo::v3_shadow_series())));
    }

    #[test]
    fn shadow_payloads_require_allowed_site_and_v2_authoritative_endpoint() {
        let us3 = ResolvedEndpoint::from_raw_endpoint("https://app.us3.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let settings = EndpointV3Settings::from_endpoint_url(
            us3.configured_endpoint(),
            us3.endpoint(),
            &[],
            &[],
            false,
            false,
            &["datadoghq.com".to_string()],
        );
        assert!(!settings.should_receive_payload(Some(MetricsPayloadInfo::v3_shadow_series())));

        let settings = EndpointV3Settings::from_endpoint_url(
            us3.configured_endpoint(),
            us3.endpoint(),
            &[],
            &[],
            false,
            false,
            &["us3.datadoghq.com".to_string()],
        );
        assert!(settings.should_receive_payload(Some(MetricsPayloadInfo::v3_shadow_series())));

        let v3_series_endpoints = vec![us3.configured_endpoint().to_string()];
        let settings = EndpointV3Settings::from_endpoint_url(
            us3.configured_endpoint(),
            us3.endpoint(),
            &v3_series_endpoints,
            &[],
            false,
            false,
            &["us3.datadoghq.com".to_string()],
        );
        assert!(!settings.should_receive_payload(Some(MetricsPayloadInfo::v3_shadow_series())));
    }

    #[test]
    fn v3_endpoint_matching_uses_configured_endpoint_before_version_prefix() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");

        assert_eq!("https://app.datadoghq.com", resolved.configured_endpoint());
        assert_ne!("app.datadoghq.com", resolved.endpoint().host_str().unwrap());

        let v3_series_endpoints = vec!["https://app.datadoghq.com".to_string()];
        let settings = EndpointV3Settings::from_endpoint_url(
            resolved.configured_endpoint(),
            resolved.endpoint(),
            &v3_series_endpoints,
            &[],
            false,
            false,
            &["datadoghq.com".to_string()],
        );

        assert!(settings.use_v3_series);
    }

    fn v3_endpoint_config<'a>(
        endpoint: &'a ResolvedEndpoint, series_config: &'a UseV3ApiSeriesConfig,
    ) -> V3EndpointConfig<'a> {
        V3EndpointConfig {
            configured_endpoint: endpoint.configured_endpoint(),
            resolved_endpoint: endpoint.endpoint(),
            serializer_v3_configured_endpoint: None,
            data_plane_v3_series_enabled: true,
            series_config,
            metrics_primary_v3_override: None,
            serializer_v3_series_endpoints: &[],
            serializer_v3_sketches_endpoints: &[],
            series_validate: false,
            sketches_validate: false,
            series_shadow_sites: &[],
        }
    }

    #[test]
    fn agent_v3_default_requires_data_plane_gate() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig::default();

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            data_plane_v3_series_enabled: false,
            series_shadow_sites: &["datadoghq.com".to_string()],
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(!settings.use_v3_series);

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            series_shadow_sites: &["datadoghq.com".to_string()],
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(settings.use_v3_series);
    }

    #[test]
    fn agent_v3_endpoint_overrides_win_over_global_default() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let mut series_config = UseV3ApiSeriesConfig::default();
        series_config
            .endpoints
            .insert(resolved.configured_endpoint().to_string(), "false".to_string());

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            series_shadow_sites: &["datadoghq.com".to_string()],
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(!settings.use_v3_series);

        series_config = UseV3ApiSeriesConfig {
            enabled: "false".to_string(),
            ..Default::default()
        };
        series_config
            .endpoints
            .insert(resolved.configured_endpoint().to_string(), "true".to_string());

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            series_shadow_sites: &["datadoghq.com".to_string()],
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(settings.use_v3_series);
    }

    #[test]
    fn agent_v3_datadog_only_matches_datadog_intake_urls() {
        let datadog = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let custom = ResolvedEndpoint::from_raw_endpoint("https://example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig {
            enabled: "datadog_only".to_string(),
            endpoints: HashMap::new(),
        };

        let datadog_settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&datadog, &series_config));
        let custom_settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&custom, &series_config));

        assert!(datadog_settings.use_v3_series);
        assert!(!custom_settings.use_v3_series);
    }

    #[test]
    fn metrics_primary_v3_uses_route_specific_override() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://vector.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig::default();

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            metrics_primary_v3_override: Some(false),
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(!settings.use_v3_series);

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            metrics_primary_v3_override: Some(true),
            ..v3_endpoint_config(&resolved, &series_config)
        });
        assert!(settings.use_v3_series);
    }

    #[test]
    fn metrics_primary_serializer_v3_can_match_primary_endpoint_name() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://vector.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig::default();
        let serializer_v3_endpoints = vec!["https://app.datadoghq.com".to_string()];

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            serializer_v3_configured_endpoint: Some("https://app.datadoghq.com"),
            metrics_primary_v3_override: Some(false),
            serializer_v3_series_endpoints: &serializer_v3_endpoints,
            ..v3_endpoint_config(&resolved, &series_config)
        });

        assert!(settings.use_v3_series);
    }

    #[test]
    fn serializer_v3_endpoint_list_wins_when_data_plane_gate_enabled() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://vector.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig {
            enabled: "false".to_string(),
            ..Default::default()
        };
        let serializer_v3_endpoints = vec![resolved.configured_endpoint().to_string()];

        let settings = EndpointV3Settings::from_v3_config(V3EndpointConfig {
            metrics_primary_v3_override: Some(false),
            serializer_v3_series_endpoints: &serializer_v3_endpoints,
            ..v3_endpoint_config(&resolved, &series_config)
        });

        assert!(settings.use_v3_series);
    }

    #[test]
    fn v3_endpoint_matching_is_endpoint_based() {
        let v3_series_endpoints = vec!["https://app.us".to_string()];
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.us5.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let settings = EndpointV3Settings::from_endpoint_url(
            resolved.configured_endpoint(),
            resolved.endpoint(),
            &v3_series_endpoints,
            &[],
            false,
            false,
            &["datadoghq.com".to_string()],
        );

        assert!(!settings.use_v3_series);
    }

    #[test]
    fn v3_endpoint_matching_requires_exact_configured_endpoint() {
        let v3_series_endpoints = vec!["app.datadoghq.com/".to_string()];
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let settings = EndpointV3Settings::from_endpoint_url(
            resolved.configured_endpoint(),
            resolved.endpoint(),
            &v3_series_endpoints,
            &[],
            false,
            false,
            &["datadoghq.com".to_string()],
        );

        assert!(!settings.use_v3_series);
    }

    #[test]
    fn calculated_site_endpoint_uses_agent_configured_endpoint_shape() {
        let resolved =
            calculate_resolved_endpoint(None, "datadoghq.com", "").expect("error calculating default API endpoint");

        assert_eq!("https://app.datadoghq.com", resolved.configured_endpoint());
    }
}

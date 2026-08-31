use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use agent_data_plane_config::shared::{self, V3SeriesMode};
use agent_data_plane_config::Live;
use http::uri::Authority;
use regex::Regex;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata;
use snafu::{ResultExt, Snafu};
use tracing::debug;
use url::Url;

use super::protocol::{MetricsPayloadInfo, MetricsProtocolVersion, UseV3ApiSeriesConfig};

static DD_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^app(\.mrf)?\.([a-z]{2,}\d{1,2}\.)?(datad(?:oghq|0g)\.(?:com|eu)|ddog-gov\.com)$").unwrap()
});

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
}

/// Inputs used to derive V3 settings for one endpoint.
pub(crate) struct V3EndpointConfig<'a> {
    /// Endpoint string as it appeared in configuration for this routed endpoint.
    pub(crate) configured_endpoint: &'a str,
    /// Optional primary endpoint name used by serializer V3 endpoint-list matching.
    pub(crate) serializer_v3_configured_endpoint: Option<&'a str>,
    /// Agent-compatible V3 series config.
    pub(crate) series_config: &'a UseV3ApiSeriesConfig,
    /// OPW/Vector route-specific V3 override.
    pub(crate) metrics_primary_v3_override: Option<bool>,
    /// Serializer V3 series endpoint list.
    pub(crate) serializer_v3_series_endpoints: &'a [String],
    /// Serializer V3 sketches endpoint list.
    pub(crate) serializer_v3_sketches_endpoints: &'a [String],
}

impl EndpointV3Settings {
    /// Returns endpoint settings with all V3 routing disabled.
    pub const fn disabled() -> Self {
        Self {
            use_v3_series: false,
            use_v3_sketches: false,
        }
    }

    /// Creates V3 settings for a specific endpoint based on URL matching.
    ///
    /// The `v3_series_endpoints` and `v3_sketches_endpoints` are lists of configured endpoint names.
    /// If the endpoint name matches any entry, V3 is enabled for that metric type.
    #[cfg(test)]
    pub fn from_endpoint_url(
        configured_endpoint: &str, _resolved_endpoint: &Url, v3_series_endpoints: &[String],
        v3_sketches_endpoints: &[String],
    ) -> Self {
        let use_v3_series = serializer_v3_config_matches_endpoint(configured_endpoint, v3_series_endpoints);
        let use_v3_sketches = v3_sketches_endpoints.iter().any(|e| configured_endpoint == e);

        Self {
            use_v3_series,
            use_v3_sketches,
        }
    }

    /// Creates V3 settings using Agent-compatible series V3 configuration.
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
        let use_v3_series = if serializer_use_v3_series {
            true
        } else if let Some(metrics_primary_use_v3) = config.metrics_primary_v3_override {
            metrics_primary_use_v3
        } else if let Some(endpoint_mode) = config.series_config.endpoints.get(config.configured_endpoint) {
            evaluate_series_v3_mode(*endpoint_mode, config.configured_endpoint)
        } else {
            evaluate_series_v3_mode(config.series_config.enabled, config.configured_endpoint)
        };

        let use_v3_sketches = config
            .serializer_v3_sketches_endpoints
            .iter()
            .any(|e| config.configured_endpoint == e);

        Self {
            use_v3_series,
            use_v3_sketches,
        }
    }

    /// Determines if this endpoint should receive a payload with the given payload info.
    ///
    /// Returns `true` if the endpoint should receive the payload, `false` otherwise.
    ///
    /// The logic is:
    /// - V2 series payload: accept if series V3 is disabled
    /// - V2 sketches payload: accept if sketches V3 is disabled
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
                    // V2 sketches: accept if V3 sketches is disabled.
                    !self.use_v3_sketches
                } else {
                    // V2 series: accept if V3 series is disabled.
                    !self.use_v3_series
                }
            }

            MetricsProtocolVersion::V3 => {
                if is_sketch {
                    // V3 sketches: accept if V3 sketches is enabled.
                    self.use_v3_sketches
                } else {
                    // V3 series: accept if V3 series is enabled.
                    self.use_v3_series
                }
            }
        }
    }
}

fn serializer_v3_config_matches_endpoint(configured_endpoint: &str, v3_series_endpoints: &[String]) -> bool {
    v3_series_endpoints
        .iter()
        .any(|endpoint| configured_endpoint == endpoint)
}

fn configured_endpoint_is_datadog_url(configured_endpoint: &str) -> bool {
    Url::parse(configured_endpoint.trim()).is_ok_and(|url| is_datadog_url(&url))
}

/// Resolves a V3 series mode against the endpoint it applies to.
///
/// Only `datadog_only` depends on the endpoint, which is why the mode cannot be reduced to a boolean
/// when it is read.
pub(crate) fn evaluate_series_v3_mode(mode: V3SeriesMode, configured_endpoint: &str) -> bool {
    match mode {
        V3SeriesMode::Enabled => true,
        V3SeriesMode::Disabled => false,
        V3SeriesMode::DatadogOnly => configured_endpoint_is_datadog_url(configured_endpoint),
    }
}

pub(crate) fn series_v3_config_can_enable_v3(series_config: &UseV3ApiSeriesConfig) -> bool {
    if series_config
        .endpoints
        .iter()
        .any(|(endpoint, mode)| evaluate_series_v3_mode(*mode, endpoint))
    {
        return true;
    }

    match series_config.enabled {
        V3SeriesMode::Enabled | V3SeriesMode::DatadogOnly => true,
        V3SeriesMode::Disabled => false,
    }
}

fn is_datadog_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.bytes().any(|byte| byte.is_ascii_uppercase()) {
        DD_URL_REGEX.is_match(&host.to_ascii_lowercase())
    } else {
        DD_URL_REGEX.is_match(host)
    }
}

pub(crate) fn is_datadog_url(url: &Url) -> bool {
    url.host_str().is_some_and(is_datadog_host)
}

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub(crate) enum EndpointError {
    Parse { source: url::ParseError, endpoint: String },
}

/// A live view of one configured API key.
///
/// Configuration always resolves the primary intake's key to a string, while a failover region's key
/// can be left unset. The two shapes are kept apart so that neither view has to invent a value. A
/// forwarder supplies one of these, and building an endpoint from it produces the matching
/// [`ApiKeySource`] state.
#[derive(Clone, Debug)]
pub(crate) enum ApiKeyView {
    /// A key configuration always resolves, such as the primary intake's key.
    Required(Live<String>),

    /// A key configuration can leave unset, such as a failover region's key.
    Optional(Live<Option<String>>),
}

/// The live configuration views a forwarder's endpoints refresh their API keys from.
///
/// Both views are optional, and each forwarder supplies the combination it needs:
///
/// - The Datadog forwarder supplies both: its primary and metrics-primary endpoints follow the
///   primary key, and its additional endpoints follow their own configured lists.
/// - A failover forwarder supplies only `primary`, holding the failover region's key, because a
///   single destination does not dual-ship.
/// - The Cluster Agent forwarder supplies neither: it presents a bearer token, which is not a
///   configured API key, so nothing refreshes it.
#[derive(Clone, Debug, Default)]
pub(crate) struct LiveApiKeys {
    /// The key the primary and metrics-primary endpoints refresh from.
    pub(crate) primary: Option<ApiKeyView>,

    /// The configured additional endpoints, keyed by intake URL as configuration spells it.
    pub(crate) additional: Option<Live<HashMap<String, Vec<String>>>>,
}

/// Which configured key a resolved endpoint refreshes from, and where to find it.
///
/// The variant is fixed at construction. `Required` and `Optional` are one configured key, as the
/// primary endpoint, the metrics-primary endpoint, and a single-destination override use; they differ
/// in whether configuration can leave that key unset. `Additional` is one position in one configured
/// key list, as a dual-shipping endpoint uses. `Fixed` is a destination whose key is not
/// configuration's to change, and such an endpoint keeps the key it started with for its whole life.
#[derive(Clone, Debug)]
enum ApiKeySource {
    /// Nothing refreshes this endpoint's key.
    Fixed,

    /// One configured key that always resolves.
    Required(Live<String>),

    /// One configured key that configuration can leave unset.
    Optional(Live<Option<String>>),

    /// One position in one additional endpoint's configured key list.
    Additional {
        /// The configured additional endpoints, keyed by intake URL as configuration spells it.
        /// `None` when the endpoint was built without a live view, which still keeps `url` and
        /// `index` for the retry queue ID.
        endpoints: Option<Live<HashMap<String, Vec<String>>>>,

        /// Intake URL, as configuration spells it and before normalization.
        url: String,

        /// Position of this endpoint's key in the key list configured for that URL. This is the
        /// configured position, not a post-deduplication counter.
        index: usize,
    },
}

impl ApiKeySource {
    /// Returns whether configuration can change this endpoint's key.
    fn refreshes(&self) -> bool {
        match self {
            Self::Fixed => false,
            Self::Required(_) | Self::Optional(_) => true,
            Self::Additional { endpoints, .. } => endpoints.is_some(),
        }
    }

    /// Refreshes the live views and returns the currently configured key for this endpoint, trimmed.
    ///
    /// Returns `None` when nothing refreshes the key, when configuration supplies no key or a blank
    /// one, or, for an additional endpoint, when its URL or key position is gone. Every variant
    /// normalizes here so that a caller cannot install a key that only differs from the configured
    /// one by surrounding whitespace. The key is borrowed from the refreshed view, so a caller that
    /// reads it per request allocates nothing until the key actually changes.
    fn refresh(&mut self) -> Option<&str> {
        let key = match self {
            Self::Fixed => return None,
            Self::Required(view) => view.refresh().as_str(),
            Self::Optional(view) => view.refresh().as_deref()?,
            Self::Additional { endpoints, url, index } => {
                endpoints.as_mut()?.refresh().get(url.as_str())?.get(*index)?.as_str()
            }
        };

        let key = key.trim();
        (!key.is_empty()).then_some(key)
    }
}

/// Returns the resolved endpoints for each configured additional endpoint and API key.
///
/// This generates a [`ResolvedEndpoint`] for each unique endpoint/API key pair, assigning each
/// endpoint the URL as configured and the position of its key in the key list configured for that
/// URL (the `enumerate()` index, not a post-dedup counter). Empty and duplicate keys are skipped;
/// their positions are consumed but no endpoint is created.
///
/// # Errors
///
/// If any of the additional endpoints aren't valid URLs, or a valid URL couldn't be constructed after applying
/// the necessary normalization / modifications, an error will be returned.
pub(crate) fn resolve_additional_endpoints(
    additional_endpoints: &HashMap<String, Vec<String>>, live_endpoints: Option<&Live<HashMap<String, Vec<String>>>>,
) -> Result<Vec<ResolvedEndpoint>, EndpointError> {
    let mut resolved = Vec::new();

    for (raw_endpoint, api_keys) in additional_endpoints {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        let logs_authority = compute_logs_authority(&endpoint);
        let traces_authority = compute_traces_authority(&endpoint);

        // Create a resolved endpoint for each unique, non-empty key. The index is the configured
        // position in the key list, so a live lookup can read that position directly.
        let mut seen = HashSet::new();
        for (index, api_key) in api_keys.iter().enumerate() {
            let trimmed_api_key = api_key.trim();
            if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                continue;
            }

            seen.insert(trimmed_api_key);
            let api_key_source = ApiKeySource::Additional {
                endpoints: live_endpoints.cloned(),
                url: raw_endpoint.to_string(),
                index,
            };
            resolved.push(ResolvedEndpoint {
                endpoint: endpoint.clone(),
                configured_endpoint: raw_endpoint.to_string(),
                api_key: trimmed_api_key.to_string(),
                api_key_source,
                logs_authority: logs_authority.clone(),
                traces_authority: traces_authority.clone(),
            });
        }
    }

    Ok(resolved)
}

/// A single destination that replaces every configured Datadog intake endpoint.
///
/// Multi-Region Failover and the Cluster Agent each send to one destination that the operator did
/// not configure as a Datadog intake, so the configured primary endpoint, additional endpoints, and
/// OPW/Vector metrics override do not apply to them.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct SingleDestination {
    /// Endpoint URL, used as provided.
    pub(crate) url: String,

    /// API key or token presented to the destination.
    pub(crate) api_key: String,

    /// Whether the destination accepts V3 series payloads.
    pub(crate) accepts_v3_series: bool,
}

/// Endpoint configuration for sending payloads to the Datadog platform.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct EndpointConfiguration {
    /// The API key to use.
    api_key: String,

    /// The primary endpoint to send payloads to, as configured and not altered in any way.
    primary_endpoint: String,

    /// Additional endpoints to dual-ship to, keyed by endpoint URL with their API keys.
    additional_endpoints: HashMap<String, Vec<String>>,
}

impl EndpointConfiguration {
    /// Creates a new `EndpointConfiguration` from the resolved endpoint configuration.
    pub(crate) fn from_configuration(endpoints: &shared::Endpoints) -> Self {
        Self {
            api_key: endpoints.api_key.clone(),
            primary_endpoint: endpoints.primary_endpoint(),
            additional_endpoints: endpoints.additional_endpoints.clone(),
        }
    }

    /// Creates a new `EndpointConfiguration` that targets a single destination.
    ///
    /// The destination is the only endpoint: dual shipping does not apply to it.
    pub(crate) fn for_single_destination(destination: &SingleDestination) -> Self {
        Self {
            api_key: destination.api_key.clone(),
            primary_endpoint: destination.url.clone(),
            additional_endpoints: HashMap::new(),
        }
    }

    /// Builds the resolved primary endpoint.
    ///
    /// # Errors
    ///
    /// If the primary endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to the endpoint, an error will be returned.
    pub(crate) fn build_primary_endpoint(
        &self, api_key_view: Option<&ApiKeyView>,
    ) -> Result<ResolvedEndpoint, GenericError> {
        ResolvedEndpoint::from_raw_endpoint(&self.primary_endpoint, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")
            .map(|endpoint| endpoint.with_api_key_view(api_key_view))
    }

    /// Returns the configured primary endpoint string without resolving or version-prefixing it.
    pub(crate) fn configured_primary_endpoint(&self) -> &str {
        &self.primary_endpoint
    }

    /// Builds the resolved primary endpoint from a URL override.
    pub(crate) fn build_primary_endpoint_override(
        &self, url: &str, api_key_view: Option<&ApiKeyView>,
    ) -> Result<ResolvedEndpoint, EndpointError> {
        ResolvedEndpoint::from_raw_endpoint(url, &self.api_key).map(|endpoint| endpoint.with_api_key_view(api_key_view))
    }

    /// Builds the resolved additional endpoints.
    ///
    /// If a live view of the configured additional endpoints is supplied, each resolved endpoint holds it and refreshes
    /// its API key on every request via [`ResolvedEndpoint::api_key`].
    ///
    /// # Errors
    ///
    /// If any additional endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to a particular endpoint, an error will be returned.
    pub(crate) fn build_additional_endpoints(
        &self, live_endpoints: Option<&Live<HashMap<String, Vec<String>>>>,
    ) -> Result<Vec<ResolvedEndpoint>, GenericError> {
        resolve_additional_endpoints(&self.additional_endpoints, live_endpoints)
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
    /// Where the API key is refreshed from after startup.
    api_key_source: ApiKeySource,
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

    /// Returns the resolved endpoint mutably.
    pub(crate) const fn endpoint_mut(&mut self) -> &mut ResolvedEndpoint {
        &mut self.endpoint
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
            api_key_source: ApiKeySource::Fixed,
            logs_authority,
            traces_authority,
        })
    }

    /// Sets the live view this endpoint refreshes its single configured API key from.
    ///
    /// Passing `None` leaves the endpoint with the key it was built with. This applies to an endpoint
    /// built by [`from_raw_endpoint`][Self::from_raw_endpoint], whose key is a single configured key;
    /// an additional endpoint gets its source from
    /// [`resolve_additional_endpoints`] instead, which also carries the position that identifies its
    /// retry queue.
    pub(crate) fn with_api_key_view(mut self, api_key_view: Option<&ApiKeyView>) -> Self {
        self.api_key_source = match api_key_view {
            Some(ApiKeyView::Required(view)) => ApiKeySource::Required(view.clone()),
            Some(ApiKeyView::Optional(view)) => ApiKeySource::Optional(view.clone()),
            None => ApiKeySource::Fixed,
        };
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
    /// When the endpoint has a live view of its configured key, the key is read from configuration on every call, so a
    /// key an operator rotates while the process runs takes effect on the next request. A configuration that supplies
    /// no key, or a blank one, leaves the last usable key in place, because a request with no key cannot succeed.
    ///
    /// An endpoint built without such a view, such as a destination presenting a token that configuration does not
    /// own, returns the key it was built with.
    pub fn api_key(&mut self) -> &str {
        match self.api_key_source.refresh() {
            Some(api_key) if api_key != self.api_key => {
                self.api_key = api_key.to_string();
                debug!(endpoint = %self.endpoint, "Refreshed endpoint API key.");
            }
            Some(_) => {}
            None => {
                if self.api_key_source.refreshes() {
                    debug!(
                        endpoint = %self.endpoint,
                        "Configuration no longer supplies a usable API key for this endpoint. Continuing with the \
                         previously configured API key."
                    );
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

    /// Returns the raw (pre-normalization) URL and key position for additional endpoints.
    ///
    /// Using the raw URL in queue IDs prevents collisions when two different raw URLs (for example,
    /// `app.datadoghq.com` and `https://app.datadoghq.com`) normalize to the same host.
    /// Returns `None` for primary and OPW endpoints.
    pub(crate) fn additional_endpoint_queue_key(&self) -> Option<(&str, usize)> {
        match &self.api_key_source {
            ApiKeySource::Additional { url, index, .. } => Some((url.as_str(), *index)),
            _ => None,
        }
    }

    /// Returns whether this endpoint refreshes its API key from configuration.
    #[cfg(test)]
    pub(crate) fn refreshes_api_key(&self) -> bool {
        self.api_key_source.refreshes()
    }

    /// Returns the configured position of this endpoint's API key, for additional endpoints.
    #[cfg(test)]
    pub(crate) fn api_key_index(&self) -> Option<usize> {
        match &self.api_key_source {
            ApiKeySource::Additional { index, .. } => Some(*index),
            _ => None,
        }
    }

    /// Returns whether this endpoint takes its API key from a position in an additional endpoint's key list.
    #[cfg(test)]
    pub(crate) fn is_additional_endpoint(&self) -> bool {
        matches!(self.api_key_source, ApiKeySource::Additional { .. })
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
    use agent_data_plane_config::{ConfigValue, SalukiConfiguration};

    use super::*;
    use crate::common::datadog::test_util::LiveConfiguration;

    /// Returns the Agent-compatible V3 series settings a default configuration resolves to.
    fn agent_series_config() -> UseV3ApiSeriesConfig {
        (&shared::MetricsEncoding::default()).into()
    }

    fn additional_endpoints(endpoints: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        endpoints
            .iter()
            .map(|(url, api_keys)| {
                (
                    url.to_string(),
                    api_keys.iter().map(|api_key| api_key.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn additional_endpoints_api_key_index_uses_raw_config_position() {
        // Keys at positions 0, 1 are valid; position 2 is empty (skipped); position 3 is a
        // duplicate of position 0 (skipped); position 4 is valid. Only positions 0, 1, 4 produce
        // ResolvedEndpoints, and their api_key_index should be 0, 1, 4 respectively.
        let endpoints = additional_endpoints(&[("app.datadoghq.com", &["key-a", "key-b", "", "key-a", "key-c"])]);

        let resolved = resolve_additional_endpoints(&endpoints, None).expect("should resolve");

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
        let endpoints2 = additional_endpoints(&[("app.datadoghq.eu", &["eu-key-a", "eu-key-b"])]);
        let resolved2 = resolve_additional_endpoints(&endpoints2, None).expect("should resolve");
        assert_eq!(resolved2[0].api_key_index(), Some(0));
        assert_eq!(resolved2[1].api_key_index(), Some(1));
    }

    #[test]
    fn api_key_refreshes_from_the_live_primary_view() {
        // The primary endpoint's key always resolves to a string, so configuration signals "no usable
        // key" with an empty or blank one rather than by dropping the value.
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = "key-1".to_string();
        let live_config = LiveConfiguration::new(config.clone());

        let view = ApiKeyView::Required(live_config.live(|config| &config.shared.endpoints.api_key));
        let mut endpoint = ResolvedEndpoint::from_raw_endpoint("http://intake.example.com", "key-1")
            .expect("should resolve")
            .with_api_key_view(Some(&view));
        assert_eq!("key-1", endpoint.api_key());

        config.shared.endpoints.api_key = "key-2".to_string();
        live_config.store(config.clone());
        assert_eq!("key-2", endpoint.api_key());

        // A key that is only whitespace is not usable, so the last usable key stays in place.
        config.shared.endpoints.api_key = "   ".to_string();
        live_config.store(config.clone());
        assert_eq!("key-2", endpoint.api_key());

        // A padded key is trimmed rather than installed as-is, so it cannot reach a request header.
        config.shared.endpoints.api_key = "  key-3  ".to_string();
        live_config.store(config.clone());
        assert_eq!("key-3", endpoint.api_key());

        // Configuration that stops supplying a key resolves to the empty string, which leaves the
        // last usable key in place.
        config.shared.endpoints.api_key = String::new();
        live_config.store(config);
        assert_eq!("key-3", endpoint.api_key());
    }

    #[test]
    fn api_key_refreshes_from_the_live_additional_endpoints_view() {
        // The endpoint reads its key from configuration on every call, so a rotated key reaches the
        // next request without a rebuild.
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.additional_endpoints =
            additional_endpoints(&[("http://extra.example.com", &["key-1"])]);
        let live_config = LiveConfiguration::new(config.clone());
        let view = live_config.live(|config| &config.shared.endpoints.additional_endpoints);

        let additional = additional_endpoints(&[("http://extra.example.com", &["key-1"])]);
        let mut endpoints = resolve_additional_endpoints(&additional, Some(&view)).expect("should resolve");
        let endpoint = &mut endpoints[0];
        assert_eq!("key-1", endpoint.api_key());

        config.shared.endpoints.additional_endpoints =
            additional_endpoints(&[("http://extra.example.com", &["key-2"])]);
        live_config.store(config.clone());
        assert_eq!("key-2", endpoint.api_key());

        // A configuration that no longer supplies a key for this position leaves the last key that
        // worked in place, because a request without a key cannot succeed.
        config.shared.endpoints.additional_endpoints = HashMap::new();
        live_config.store(config);
        assert_eq!("key-2", endpoint.api_key());
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
    fn the_primary_endpoint_is_built_from_resolved_configuration() {
        // Endpoint resolution belongs to the configuration layer; the component version-prefixes the
        // resolved Datadog endpoint and uses a non-Datadog override verbatim.
        let prefix = get_data_plane_version_prefix();
        let cases = [
            (
                "site-derived endpoint",
                ConfigValue::explicit("us3.datadoghq.com".to_string()),
                ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
                format!("https://{prefix}.us3.datadoghq.com/"),
            ),
            (
                "explicit dd_url used verbatim",
                ConfigValue::explicit("us3.datadoghq.com".to_string()),
                ConfigValue::explicit("https://dogpound.io/".to_string()),
                "https://dogpound.io/".to_string(),
            ),
        ];

        for (name, site, dd_url, expected_endpoint) in cases {
            let endpoints = shared::Endpoints {
                api_key: "fake-api-key".to_string(),
                site,
                dd_url,
                ..Default::default()
            };
            let config = EndpointConfiguration::from_configuration(&endpoints);

            let resolved = config.build_primary_endpoint(None).expect(name);
            assert_eq!(expected_endpoint, resolved.endpoint().to_string(), "{name}");
            assert_eq!("fake-api-key", resolved.cached_api_key(), "{name}");
        }
    }

    #[test]
    fn a_single_destination_is_the_only_endpoint() {
        let destination = SingleDestination {
            url: "https://cluster-agent.example.com:5005".to_string(),
            api_key: "secret-token".to_string(),
            accepts_v3_series: false,
        };
        let config = EndpointConfiguration::for_single_destination(&destination);

        assert_eq!(
            "https://cluster-agent.example.com:5005",
            config.configured_primary_endpoint()
        );
        assert!(config
            .build_additional_endpoints(None)
            .expect("additional endpoints should resolve")
            .is_empty());
    }

    #[test]
    fn should_receive_payload_covers_all_documented_branches() {
        // Walks every branch enumerated in `should_receive_payload`'s doc comment:
        // - V2 series: accept if series V3 is disabled
        // - V2 sketches: accept if sketches V3 is disabled
        // - V3 series: accept if series V3 is enabled
        // - V3 sketches: accept if sketches V3 is enabled
        // - Non-metrics payloads (None): always accept
        let cases: [(&str, EndpointV3Settings, Option<MetricsPayloadInfo>, bool); 9] = [
            (
                "v2 series accepted when series v3 disabled",
                EndpointV3Settings::disabled(),
                Some(MetricsPayloadInfo::v2_series()),
                true,
            ),
            (
                "v2 series rejected when series v3 enabled",
                EndpointV3Settings {
                    use_v3_series: true,
                    ..EndpointV3Settings::disabled()
                },
                Some(MetricsPayloadInfo::v2_series()),
                false,
            ),
            (
                "v2 sketches accepted when sketches v3 disabled",
                EndpointV3Settings::disabled(),
                Some(MetricsPayloadInfo::v2_sketches()),
                true,
            ),
            (
                "v2 sketches rejected when sketches v3 enabled",
                EndpointV3Settings {
                    use_v3_sketches: true,
                    ..EndpointV3Settings::disabled()
                },
                Some(MetricsPayloadInfo::v2_sketches()),
                false,
            ),
            (
                "v3 series accepted when series v3 enabled",
                EndpointV3Settings {
                    use_v3_series: true,
                    ..EndpointV3Settings::disabled()
                },
                Some(MetricsPayloadInfo::v3_series()),
                true,
            ),
            (
                "v3 series rejected when series v3 disabled",
                EndpointV3Settings::disabled(),
                Some(MetricsPayloadInfo::v3_series()),
                false,
            ),
            (
                "v3 sketches accepted when sketches v3 enabled",
                EndpointV3Settings {
                    use_v3_sketches: true,
                    ..EndpointV3Settings::disabled()
                },
                Some(MetricsPayloadInfo::v3_sketches()),
                true,
            ),
            (
                "v3 sketches rejected when sketches v3 disabled",
                EndpointV3Settings::disabled(),
                Some(MetricsPayloadInfo::v3_sketches()),
                false,
            ),
            (
                "non-metrics payload always accepted",
                EndpointV3Settings {
                    use_v3_series: true,
                    use_v3_sketches: true,
                },
                None,
                true,
            ),
        ];

        for (name, settings, payload_info, expected) in cases {
            assert_eq!(settings.should_receive_payload(payload_info), expected, "{name}");
        }
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
        );

        assert!(settings.use_v3_series);
    }

    fn v3_endpoint_config<'a>(
        endpoint: &'a ResolvedEndpoint, series_config: &'a UseV3ApiSeriesConfig,
    ) -> V3EndpointConfig<'a> {
        V3EndpointConfig {
            configured_endpoint: endpoint.configured_endpoint(),
            serializer_v3_configured_endpoint: None,
            series_config,
            metrics_primary_v3_override: None,
            serializer_v3_series_endpoints: &[],
            serializer_v3_sketches_endpoints: &[],
        }
    }

    #[test]
    fn agent_v3_default_enables_authoritative_v3() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = agent_series_config();

        let settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&resolved, &series_config));
        assert!(settings.use_v3_series);
    }

    #[test]
    fn agent_v3_endpoint_overrides_win_over_global_default() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let mut series_config = agent_series_config();
        series_config
            .endpoints
            .insert(resolved.configured_endpoint().to_string(), V3SeriesMode::Disabled);

        let settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&resolved, &series_config));
        assert!(!settings.use_v3_series);

        series_config = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::Disabled,
            ..Default::default()
        };
        series_config
            .endpoints
            .insert(resolved.configured_endpoint().to_string(), V3SeriesMode::Enabled);

        let settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&resolved, &series_config));
        assert!(settings.use_v3_series);
    }

    #[test]
    fn agent_v3_datadog_only_matches_datadog_intake_urls() {
        let datadog = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let custom = ResolvedEndpoint::from_raw_endpoint("https://example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::DatadogOnly,
            endpoints: HashMap::new(),
        };

        let datadog_settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&datadog, &series_config));
        let custom_settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&custom, &series_config));

        assert!(datadog_settings.use_v3_series);
        assert!(!custom_settings.use_v3_series);
    }

    #[test]
    fn agent_v3_datadog_only_config_viability_matches_core_agent_url_rules() {
        let mut series_config = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::Disabled,
            endpoints: HashMap::new(),
        };
        for endpoint in [
            "https://app.datadoghq.com",
            "https://APP.DATADOGHQ.COM",
            "https://app.datadoghq.com.:443",
            "https://app.us12.datadoghq.com",
            "https://app.apne1.datadoghq.com",
        ] {
            series_config.endpoints = HashMap::from([(endpoint.to_string(), V3SeriesMode::DatadogOnly)]);
            assert!(series_v3_config_can_enable_v3(&series_config), "{endpoint}");
        }

        for endpoint in ["app.datadoghq.com", "app.datadoghq.com:443", "example.com"] {
            series_config.endpoints = HashMap::from([(endpoint.to_string(), V3SeriesMode::DatadogOnly)]);
            assert!(!series_v3_config_can_enable_v3(&series_config), "{endpoint}");
        }
    }

    #[test]
    fn agent_v3_datadog_only_endpoint_override_rejects_schemeless_host_port() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("app.datadoghq.com:443", "fake-api-key")
            .expect("endpoint should resolve");
        let mut series_config = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::Disabled,
            endpoints: HashMap::new(),
        };
        series_config
            .endpoints
            .insert(resolved.configured_endpoint().to_string(), V3SeriesMode::DatadogOnly);

        let settings = EndpointV3Settings::from_v3_config(v3_endpoint_config(&resolved, &series_config));

        assert!(!settings.use_v3_series);
    }

    #[test]
    fn metrics_primary_v3_uses_route_specific_override() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://vector.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = agent_series_config();

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
        let series_config = agent_series_config();
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
    fn serializer_v3_endpoint_list_wins_over_other_agent_settings() {
        let resolved = ResolvedEndpoint::from_raw_endpoint("https://vector.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let series_config = UseV3ApiSeriesConfig {
            enabled: V3SeriesMode::Disabled,
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
        );

        assert!(!settings.use_v3_series);
    }

    #[test]
    fn the_configured_endpoint_keeps_the_agent_endpoint_shape() {
        // The configured endpoint is matched against V3 endpoint lists, so it must stay in the shape
        // the Agent uses, before version prefixing.
        let endpoints = shared::Endpoints {
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };
        let config = EndpointConfiguration::from_configuration(&endpoints);

        assert_eq!("https://app.datadoghq.com", config.configured_primary_endpoint());
        assert_eq!(
            "https://app.datadoghq.com",
            config
                .build_primary_endpoint(None)
                .expect("endpoint should resolve")
                .configured_endpoint()
        );
    }

    #[test]
    fn an_explicit_dd_url_overrides_site_even_at_the_schema_default() {
        // The Agent sends `dd_url` at its schema default even when only `site` is configured, so the
        // URL alone cannot express an override: an operator who explicitly sets the default URL still
        // overrides `site`, and a defaulted URL does not.
        let cases = [
            (
                "explicit default URL overrides site",
                ConfigValue::explicit("https://app.datadoghq.com".to_string()),
                "https://app.datadoghq.com",
            ),
            (
                "defaulted URL leaves the endpoint to site",
                ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
                "https://app.datadoghq.eu",
            ),
        ];

        for (name, dd_url, expected) in cases {
            let endpoints = shared::Endpoints {
                site: ConfigValue::explicit("datadoghq.eu".to_string()),
                dd_url,
                ..Default::default()
            };
            let config = EndpointConfiguration::from_configuration(&endpoints);

            assert_eq!(expected, config.configured_primary_endpoint(), "{name}");
        }
    }
}

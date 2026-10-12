use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, LazyLock},
};

use agent_data_plane_config::shared::{self, V3SeriesMode};
use arc_swap::ArcSwap;
use http::uri::Authority;
use regex::Regex;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata;
use snafu::{ResultExt, Snafu};
use stringtheory::MetaString;
use tracing::{debug, error};
use url::Url;

use super::api_key::ApiKeyCell;
use super::delivery::{PayloadClasses, RecipientOrigin, RecipientProtocol, RecipientSpec};
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
    /// Agent-compatible V3 series config.
    pub(crate) series_config: &'a UseV3ApiSeriesConfig,
    /// OPW/Vector route-specific V3 override.
    pub(crate) metrics_primary_v3_override: Option<bool>,
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

    /// Creates V3 settings using Agent-compatible series V3 configuration.
    pub fn from_v3_config(config: V3EndpointConfig<'_>) -> Self {
        let use_v3_series = if let Some(metrics_primary_use_v3) = config.metrics_primary_v3_override {
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
    Parse {
        source: url::ParseError,
        endpoint: String,
    },

    #[snafu(display("API key contains characters that are invalid in HTTP headers."))]
    InvalidApiKey,
}

/// Returns the resolved endpoints for each configured additional endpoint and API key.
///
/// This generates a [`ResolvedEndpoint`] for each unique endpoint/API key pair, assigning each
/// endpoint an `api_key_index` equal to the position of its key in the configured key list for that
/// URL (the `enumerate()` index, not a post-dedup counter). Empty and duplicate keys are skipped;
/// their positions are consumed but no endpoint is created.
///
/// # Errors
///
/// Returns an error if an endpoint URL is invalid.
pub(crate) fn resolve_additional_endpoints(
    additional_endpoints: &HashMap<String, Vec<String>>,
) -> Result<Vec<ResolvedEndpoint>, EndpointError> {
    let mut resolved = Vec::new();

    for (raw_endpoint, api_keys) in additional_endpoints {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        let logs_authority = compute_logs_authority(&endpoint);
        let traces_authority = compute_traces_authority(&endpoint);

        // Create a resolved endpoint for each unique, non-empty key. The index is the configured
        // position in the key list so that live lookups can use `vec[index]` directly.
        let mut seen = HashSet::new();
        for (index, api_key) in api_keys.iter().enumerate() {
            let trimmed_api_key = api_key.trim();
            if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                continue;
            }

            seen.insert(trimmed_api_key);
            let api_key = match ApiKeyCell::new(trimmed_api_key) {
                Ok(api_key) => api_key,
                Err(_) => {
                    error!(
                        config_key = "additional_endpoints",
                        endpoint = raw_endpoint,
                        api_key_index = index,
                        "Skipping API key because it cannot be used as an HTTP header value."
                    );
                    continue;
                }
            };
            resolved.push(ResolvedEndpoint {
                endpoint: endpoint.clone(),
                configured_endpoint: raw_endpoint.to_string(),
                api_key,
                api_key_index: Some(index),
                raw_additional_url: Some(raw_endpoint.to_string()),
                logs_authority: logs_authority.clone(),
                traces_authority: traces_authority.clone(),
                protocol: None,
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
    /// Returns the configured primary endpoint identity, before any metrics-only override.
    pub(crate) fn primary_endpoint(&self) -> &str {
        &self.primary_endpoint
    }

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
    pub(crate) fn build_primary_endpoint(&self) -> Result<ResolvedEndpoint, GenericError> {
        ResolvedEndpoint::from_raw_endpoint(&self.primary_endpoint, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")
    }

    /// Builds the resolved primary endpoint from a URL override.
    pub(crate) fn build_primary_endpoint_override(&self, url: &str) -> Result<ResolvedEndpoint, EndpointError> {
        ResolvedEndpoint::from_raw_endpoint(url, &self.api_key)
    }

    /// Builds the resolved additional endpoints.
    ///
    /// # Errors
    ///
    /// If any additional endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to a particular endpoint, an error will be returned.
    pub(crate) fn build_additional_endpoints(&self) -> Result<Vec<ResolvedEndpoint>, GenericError> {
        resolve_additional_endpoints(&self.additional_endpoints)
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
    /// The key this endpoint presents. A forwarder may bind it to a live view through `ApiKeyRefresher`.
    api_key: ApiKeyCell,
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
    /// The live protocol of a recipient a delivery host runs, which its request mapper reads at every dispatch.
    /// `None` for an endpoint a static forwarder runs, whose protocol is fixed when it is built.
    protocol: Option<ProtocolCell>,
}

/// How a delivery host's recipient forms its requests, as its request mapper reads it at every dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DispatchProtocol {
    /// The Datadog intake.
    DatadogIntake {
        /// The authority logs payloads go to in place of the endpoint's own.
        logs_authority: Option<Authority>,

        /// The authority trace and APM statistics payloads go to in place of the endpoint's own.
        traces_authority: Option<Authority>,

        /// Whether every request carries `Allow-Arbitrary-Tag-Value: true`.
        allow_arbitrary_tags: bool,
    },

    /// The Cluster Agent.
    ClusterAgent,
}

impl DispatchProtocol {
    /// Returns how requests are formed for `protocol`.
    ///
    /// An authority that does not parse is left out, so requests of that class go to the endpoint's own authority,
    /// as natively when the endpoint's host names no intake authority.
    pub(crate) fn new(protocol: &RecipientProtocol) -> Self {
        let authority = |authority: &Option<MetaString>| {
            authority
                .as_deref()
                .and_then(|authority| Authority::from_str(authority).ok())
        };
        match protocol {
            RecipientProtocol::DatadogIntake {
                authorities,
                allow_arbitrary_tags,
                ..
            } => Self::DatadogIntake {
                logs_authority: authority(&authorities.logs),
                traces_authority: authority(&authorities.traces),
                allow_arbitrary_tags: *allow_arbitrary_tags,
            },
            RecipientProtocol::ClusterAgent => Self::ClusterAgent,
        }
    }
}

/// The live protocol of one recipient a delivery host runs: the host stores each protocol a publication gives the
/// recipient, and the recipient's request mapper reads it at every dispatch, so a change applies at the next request
/// boundary.
///
/// Cloning shares the cell.
#[derive(Clone, Debug)]
pub(crate) struct ProtocolCell {
    current: Arc<ArcSwap<DispatchProtocol>>,
}

impl ProtocolCell {
    fn new(protocol: &RecipientProtocol) -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(DispatchProtocol::new(protocol))),
        }
    }

    /// Returns the current protocol.
    pub(crate) fn load(&self) -> Arc<DispatchProtocol> {
        self.current.load_full()
    }

    /// Replaces the current protocol.
    pub(crate) fn store(&self, protocol: &RecipientProtocol) {
        self.current.store(Arc::new(DispatchProtocol::new(protocol)));
    }
}

/// How a forwarder came to send to one of its metrics recipients.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MetricsRecipientRoute {
    /// The primary endpoint, or the single destination of a forwarder that has one.
    Primary,

    /// The alternate metrics intake (the Observability Pipelines Worker, or Vector) that replaces the primary endpoint
    /// for metrics.
    MetricsOverride,

    /// An additional endpoint that payloads are dual-shipped to.
    Additional,
}

impl MetricsRecipientRoute {
    /// Returns the route's name: `primary`, `metrics_override`, or `additional`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::MetricsOverride => "metrics_override",
            Self::Additional => "additional",
        }
    }
}

/// One endpoint that a forwarder sends metrics payloads to: one endpoint I/O loop, with its own retry queue.
///
/// A forwarder has one recipient for its primary endpoint (or its alternate metrics intake, which replaces the primary
/// endpoint for metrics), and one for each distinct, non-empty, usable API key of each additional endpoint. Recipients
/// are identified by what names their queue directories ([`queue_endpoint`][Self::queue_endpoint]): two additional
/// endpoints whose URLs normalize to the same host are two recipients, and so is an additional endpoint that repeats
/// the primary endpoint, while two spellings of the primary endpoint are one.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MetricsRecipient {
    /// How the forwarder came to send to the endpoint.
    pub route: MetricsRecipientRoute,

    /// The endpoint as configured, before normalization.
    pub configured_endpoint: String,

    /// The position of the endpoint's API key in the configured key list of its additional endpoint, or `None` for a
    /// primary endpoint or alternate metrics intake, which have one key.
    pub api_key_index: Option<usize>,

    /// Which payloads the endpoint receives besides metrics payloads that name it: every class for the primary and
    /// additional endpoints, metrics only for the alternate metrics intake.
    pub classes: PayloadClasses,

    /// How the endpoint's requests are formed, and which metrics variants it takes in their V3 form.
    pub protocol: RecipientProtocol,
}

impl MetricsRecipient {
    /// Returns the endpoint the recipient's queue directory is named after, as an identity spells it
    /// ([`queue_endpoint`]).
    pub fn queue_endpoint(&self) -> String {
        queue_endpoint(self.route, &self.configured_endpoint)
    }
}

/// Returns the endpoint the queue directory of a recipient of `route` configured as `configured` is named after, as an
/// identity spells it.
///
/// An additional endpoint's directory hashes its URL as configured, with its key's position, so it is spelled as
/// configured. Any other recipient's hashes its URL as the forwarder resolves it, so it is spelled resolved: two
/// spellings of one intake, such as with and without a trailing slash or a default port, or a site and the URL it
/// implies, are one endpoint, and two configurations that give one endpoint share one directory. A resolved endpoint is
/// written without its root path, and with the data plane's version label, which the forwarder puts in place of a
/// Datadog intake's leading `app` label, written as that `app` label, so that it reads as configured and stays the same
/// from one version to the next. An endpoint the forwarder cannot resolve is spelled as configured.
pub fn queue_endpoint(route: MetricsRecipientRoute, configured: &str) -> String {
    if route == MetricsRecipientRoute::Additional {
        return configured.to_string();
    }
    let Ok(mut resolved) = parse_and_normalize_endpoint(configured) else {
        return configured.to_string();
    };
    let prefix = format!("{}.", get_data_plane_version_prefix());
    let configured_host = resolved
        .host_str()
        .and_then(|host| host.strip_prefix(&prefix))
        .map(|rest| format!("app.{}", rest))
        .filter(|host| DD_URL_REGEX.is_match(host));
    if let Some(host) = configured_host {
        // Only an `app` label the forwarder replaced is written back: a host that already reads `app` and matches is
        // always replaced, so no two resolved endpoints are written alike.
        let _ = resolved.set_host(Some(&host));
    }
    let mut spelled = resolved.to_string();
    // An HTTP URL always has a path, so the root path written as nothing is never another endpoint's.
    if resolved.path() == "/" && resolved.query().is_none() && resolved.fragment().is_none() {
        spelled.pop();
    }
    spelled
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
    /// Returns an error if the endpoint URL is invalid or the API key cannot be used as an HTTP header value.
    pub(crate) fn from_raw_endpoint(raw_endpoint: &str, api_key: &str) -> Result<Self, EndpointError> {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        let logs_authority = compute_logs_authority(&endpoint);
        let traces_authority = compute_traces_authority(&endpoint);
        Ok(Self {
            endpoint,
            configured_endpoint: raw_endpoint.to_string(),
            api_key: ApiKeyCell::new(api_key).map_err(|_| EndpointError::InvalidApiKey)?,
            api_key_index: None,
            raw_additional_url: None,
            logs_authority,
            traces_authority,
            protocol: None,
        })
    }

    /// Creates the endpoint of a recipient a delivery host runs, presenting `api_key` until its credential changes.
    ///
    /// The endpoint is resolved as natively from the recipient's endpoint as configured (or, for a declared
    /// recipient, in canonical form), and an additional endpoint keeps what names its queue directory, its raw URL and
    /// key position, as a static forwarder's does.
    /// Its request mapper reads the recipient's protocol from a live cell, which the host replaces when a publication
    /// changes the protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint URL is invalid or the API key cannot be used as an HTTP header value.
    pub(crate) fn for_recipient(spec: &RecipientSpec, api_key: &str) -> Result<Self, EndpointError> {
        let mut endpoint = Self::from_raw_endpoint(&spec.endpoint, api_key)?;
        if let RecipientOrigin::Native(recipient) = &spec.origin {
            if recipient.route == MetricsRecipientRoute::Additional {
                endpoint.raw_additional_url = Some(recipient.configured_endpoint.clone());
                endpoint.api_key_index = recipient.api_key_index;
            }
        }
        endpoint.protocol = Some(ProtocolCell::new(&spec.protocol));
        Ok(endpoint)
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
    /// The returned key is the one stored for this endpoint. After an
    /// [`ApiKeyRefresher`][super::api_key::ApiKeyRefresher] processes a configuration change, it replaces
    /// the stored key. An endpoint not bound to a refresher, such as one presenting a token that
    /// configuration does not own, keeps the key it was built with.
    pub fn api_key(&self) -> MetaString {
        self.api_key.load()
    }

    /// Returns the cell holding this endpoint's API key, for a refresher to write.
    pub(crate) const fn api_key_cell(&self) -> &ApiKeyCell {
        &self.api_key
    }

    /// Returns the position of this endpoint's API key in the `additional_endpoints` config list for
    /// its URL. `None` for primary and OPW endpoints.
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

    /// Returns the live protocol of a recipient a delivery host runs, or `None` for an endpoint a static forwarder
    /// runs.
    pub(crate) fn protocol_cell(&self) -> Option<&ProtocolCell> {
        self.protocol.as_ref()
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
pub(crate) fn get_data_plane_version_prefix() -> String {
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
    use agent_data_plane_config::ConfigValue;

    use super::*;

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

        let resolved = resolve_additional_endpoints(&endpoints).expect("should resolve");

        assert_eq!(
            resolved.len(),
            3,
            "should have 3 endpoints (skipping empty and duplicate)"
        );
        assert_eq!(&*resolved[0].api_key(), "key-a");
        assert_eq!(resolved[0].api_key_index(), Some(0));
        assert_eq!(&*resolved[1].api_key(), "key-b");
        assert_eq!(resolved[1].api_key_index(), Some(1));
        assert_eq!(&*resolved[2].api_key(), "key-c");
        assert_eq!(
            resolved[2].api_key_index(),
            Some(4),
            "index 4 — not 2 — because original positions are used"
        );

        // Two URLs have independent index spaces (both start from 0).
        let endpoints2 = additional_endpoints(&[("app.datadoghq.eu", &["eu-key-a", "eu-key-b"])]);
        let resolved2 = resolve_additional_endpoints(&endpoints2).expect("should resolve");
        assert_eq!(resolved2[0].api_key_index(), Some(0));
        assert_eq!(resolved2[1].api_key_index(), Some(1));
    }

    #[test]
    fn header_invalid_additional_api_keys_are_skipped() {
        let endpoints = additional_endpoints(&[("app.datadoghq.com", &["key-a", "key\nvalue", "key-b"])]);

        let resolved = resolve_additional_endpoints(&endpoints).expect("should resolve");

        assert_eq!(resolved.len(), 2);
        assert_eq!(&*resolved[0].api_key(), "key-a");
        assert_eq!(resolved[0].api_key_index(), Some(0));
        assert_eq!(&*resolved[1].api_key(), "key-b");
        assert_eq!(resolved[1].api_key_index(), Some(2));
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

            let resolved = config.build_primary_endpoint().expect(name);
            assert_eq!(expected_endpoint, resolved.endpoint().to_string(), "{name}");
            assert_eq!("fake-api-key", &*resolved.api_key(), "{name}");
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
            config
                .build_primary_endpoint()
                .expect("endpoint should resolve")
                .configured_endpoint()
        );
        assert!(config
            .build_additional_endpoints()
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

    fn v3_endpoint_config<'a>(
        endpoint: &'a ResolvedEndpoint, series_config: &'a UseV3ApiSeriesConfig,
    ) -> V3EndpointConfig<'a> {
        V3EndpointConfig {
            configured_endpoint: endpoint.configured_endpoint(),
            series_config,
            metrics_primary_v3_override: None,
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
    fn the_configured_endpoint_keeps_the_agent_endpoint_shape() {
        // The configured endpoint is matched against V3 endpoint lists, so it must stay in the shape
        // the Agent uses, before version prefixing.
        let endpoints = shared::Endpoints {
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };
        let config = EndpointConfiguration::from_configuration(&endpoints);

        assert_eq!(
            "https://app.datadoghq.com",
            config
                .build_primary_endpoint()
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

            assert_eq!(
                expected,
                config
                    .build_primary_endpoint()
                    .expect("endpoint should resolve")
                    .configured_endpoint(),
                "{name}"
            );
        }
    }
}

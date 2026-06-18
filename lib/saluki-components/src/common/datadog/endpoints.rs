use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use http::uri::Authority;
use regex::Regex;
use saluki_component_config::forwarder as leaf;
use saluki_component_config::ScopedConfig;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata;
use snafu::{ResultExt, Snafu};
use tracing::debug;
use url::Url;

/// Live forwarder configuration handle used to refresh API keys at request-build time.
///
/// A dynamic-capable forwarder reads `.current()` on this handle to pick up the latest API keys
/// (primary and additional) published by the configuration system. A fixed deployment simply wraps
/// the static configuration.
pub(crate) type LiveForwarderConfig = ScopedConfig<leaf::DatadogForwarderConfig>;

static DD_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap());

pub const DEFAULT_SITE: &str = "datadoghq.com";

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub(crate) enum EndpointError {
    Parse { source: url::ParseError, endpoint: String },
}

/// A set of additional API endpoints to forward metrics to.
///
/// Each endpoint can be associated with multiple API keys. Requests will be forwarded to each
/// unique endpoint/API key pair.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct AdditionalEndpoints(HashMap<String, Vec<String>>);

impl AdditionalEndpoints {
    /// Builds the runtime additional endpoints from their leaf mirror.
    pub(crate) fn from_native(cfg: &leaf::AdditionalEndpoints) -> Self {
        Self(cfg.0.clone())
    }

    fn mappings(&self) -> impl Iterator<Item = (&str, &Vec<String>)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
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
        &self, live_config: Option<LiveForwarderConfig>,
    ) -> Result<Vec<ResolvedEndpoint>, EndpointError> {
        let mut resolved = Vec::new();

        for (raw_endpoint, api_keys) in self.mappings() {
            let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
            let logs_authority = compute_logs_authority(&endpoint);
            let traces_authority = compute_traces_authority(&endpoint);

            // Create a resolved endpoint for each unique, non-empty key. The index is the raw
            // position in the config list so that live lookups can use `vec[index]` directly.
            let mut seen = HashSet::new();
            for (index, api_key) in api_keys.iter().enumerate() {
                let trimmed_api_key = api_key.trim();
                if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                    continue;
                }

                seen.insert(trimmed_api_key);
                resolved.push(ResolvedEndpoint {
                    endpoint: endpoint.clone(),
                    api_key: trimmed_api_key.to_string(),
                    live_config: live_config.clone(),
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
///
/// Behavior-carrying runtime type built from its leaf mirror via [`EndpointConfiguration::from_native`].
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct EndpointConfiguration {
    /// The API key to use.
    api_key: String,

    /// The site to send metrics to.
    ///
    /// This is the base domain for the Datadog site in which the API key originates from. This will generally be a
    /// portion of the domain used to access the Datadog UI, such as `datadoghq.com` or `us5.datadoghq.com`.
    ///
    /// Defaults to `datadoghq.com`.
    site: String,

    /// The full URL base to send metrics to.
    ///
    /// This takes precedence over `site`, and isn't altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (for example, `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    dd_url: Option<String>,

    /// Enables sending data to multiple endpoints and/or with multiple API keys via dual shipping.
    ///
    /// Defaults to empty.
    additional_endpoints: AdditionalEndpoints,
}

impl EndpointConfiguration {
    /// Builds the runtime endpoint configuration from its leaf mirror.
    pub fn from_native(cfg: &leaf::EndpointConfiguration) -> Self {
        Self {
            api_key: cfg.api_key.clone(),
            site: if cfg.site.is_empty() {
                DEFAULT_SITE.to_owned()
            } else {
                cfg.site.clone()
            },
            dd_url: cfg.dd_url.clone(),
            additional_endpoints: AdditionalEndpoints::from_native(&cfg.additional_endpoints),
        }
    }

    /// Sets the full URL base to send metrics to.
    pub fn set_dd_url(&mut self, url: String) {
        self.dd_url = Some(url);
    }

    /// Sets the API key to use.
    pub fn set_api_key(&mut self, api_key: String) {
        self.api_key = api_key;
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
        &self, live_config: Option<LiveForwarderConfig>,
    ) -> Result<ResolvedEndpoint, GenericError> {
        calculate_resolved_endpoint(self.dd_url.as_deref(), &self.site, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")
            .map(|endpoint| endpoint.with_live_config(live_config))
    }

    /// Builds the resolved primary endpoint from a URL override.
    pub(crate) fn build_primary_endpoint_override(
        &self, url: &str, live_config: Option<LiveForwarderConfig>,
    ) -> Result<ResolvedEndpoint, EndpointError> {
        calculate_resolved_endpoint(Some(url), &self.site, &self.api_key)
            .map(|endpoint| endpoint.with_live_config(live_config))
    }

    /// Builds the resolved additional endpoints.
    ///
    /// If a [`LiveForwarderConfig`] is supplied, each additional endpoint will hold a live
    /// reference to it and refresh its API key on every request via [`ResolvedEndpoint::api_key`].
    ///
    /// # Errors
    ///
    /// If any additional endpoint isn't a valid URL, or a valid URL couldn't be constructed after applying the
    /// necessary normalization / modifications to a particular endpoint, an error will be returned.
    pub(crate) fn build_additional_endpoints(
        &self, live_config: Option<LiveForwarderConfig>,
    ) -> Result<Vec<ResolvedEndpoint>, GenericError> {
        self.additional_endpoints
            .resolved_endpoints(live_config)
            .error_context("Failed parsing/resolving the additional destination endpoints.")
    }
}

/// A single API endpoint and its associated API key.
///
/// An endpoint is defined as a unique, fully qualified domain name that metrics will be sent to, such as
/// `https://app.datadoghq.com`.
#[derive(Clone)]
pub struct ResolvedEndpoint {
    endpoint: Url,
    api_key: String,
    live_config: Option<LiveForwarderConfig>,
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

impl std::fmt::Debug for ResolvedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEndpoint")
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key)
            .field("has_live_config", &self.live_config.is_some())
            .field("api_key_index", &self.api_key_index)
            .field("raw_additional_url", &self.raw_additional_url)
            .field("logs_authority", &self.logs_authority)
            .field("traces_authority", &self.traces_authority)
            .finish()
    }
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
            api_key: api_key.to_string(),
            live_config: None,
            api_key_index: None,
            raw_additional_url: None,
            logs_authority,
            traces_authority,
        })
    }

    /// Returns a copy of this endpoint with a live forwarder configuration handle attached.
    ///
    /// When present, the handle is consulted on each call to [`api_key`][Self::api_key] so that
    /// rotated API keys published by the configuration system take effect without rebuilding the
    /// endpoint.
    pub fn with_live_config(mut self, live_config: Option<LiveForwarderConfig>) -> Self {
        self.live_config = live_config;
        self
    }

    /// Returns the endpoint of the resolver.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the API key associated with the endpoint.
    ///
    /// If a [`LiveForwarderConfig`] has been configured, the API key is read from the latest
    /// configuration value and stored if it has changed since the last call.
    ///
    /// For additional endpoints (those with an [`api_key_index`][Self::api_key_index]), the key is
    /// looked up by position in the latest `additional_endpoints` configuration value. For the
    /// primary endpoint, the latest `endpoint.api_key` is used directly.
    pub fn api_key(&mut self) -> &str {
        if let Some(live_config) = &self.live_config {
            let current = live_config.current();
            if let (Some(index), Some(raw_url)) = (self.api_key_index, self.raw_additional_url.as_deref()) {
                // Additional endpoint: look up current key by raw index in this URL key list.
                match lookup_additional_key(&current, raw_url, index) {
                    Some(key) if key != self.api_key => {
                        debug!(endpoint = %self.endpoint, index, "Refreshed additional endpoint API key.");
                        self.api_key = key;
                    }
                    None => {
                        debug!(
                            endpoint = %self.endpoint,
                            index,
                            "Could not refresh additional endpoint key from config (index out of range or \
                             empty). Continuing with last known valid API key."
                        );
                    }
                    _ => {}
                }
            } else {
                // Primary / OPW / override endpoint: refresh from the current primary API key.
                let api_key = current.endpoint.api_key;
                if !api_key.is_empty() && self.api_key != api_key {
                    debug!(endpoint = %self.endpoint, "Refreshed API key.");
                    self.api_key = api_key;
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
    pub(crate) fn has_live_config(&self) -> bool {
        self.live_config.is_some()
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

fn parse_and_normalize_endpoint(raw_endpoint: &str) -> Result<Url, EndpointError> {
    // Start out by parsing the given domain/endpoint, which means ensuring first that it has a scheme.
    //
    // If no scheme is present, we assume HTTPS.
    let raw_endpoint = if !raw_endpoint.starts_with("http://") && !raw_endpoint.starts_with("https://") {
        format!("https://{}", raw_endpoint)
    } else {
        raw_endpoint.to_string()
    };

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
            format!("app.{}", base_domain)
        }
    };

    ResolvedEndpoint::from_raw_endpoint(&raw_endpoint, api_key)
}

/// Returns the API key at position `index` in `raw_url`'s key list from the latest configuration.
///
/// `raw_url` is the pre-normalization URL string (for example `"app.datadoghq.eu"`) as it appears as a
/// key in the `additional_endpoints` configuration value. `index` is the raw `enumerate()` position of
/// the key in that URL list (not a post-dedup counter).
///
/// Returns `None` if the URL is not present in the current config, if `index` is out of range, or
/// if the key at that position is empty.
fn lookup_additional_key(config: &leaf::DatadogForwarderConfig, raw_url: &str, index: usize) -> Option<String> {
    let key = config.endpoint.additional_endpoints.0.get(raw_url)?.get(index)?.trim();
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
    use saluki_component_config::forwarder as leaf;
    use saluki_component_config::ScopedConfig;
    use tokio::sync::watch;

    use super::*;

    fn additional_endpoints(entries: &[(&str, &[&str])]) -> AdditionalEndpoints {
        let map = entries
            .iter()
            .map(|(url, keys)| (url.to_string(), keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()))
            .collect();
        AdditionalEndpoints::from_native(&leaf::AdditionalEndpoints(map))
    }

    fn forwarder_config_with_additional(entries: &[(&str, &[&str])]) -> leaf::DatadogForwarderConfig {
        let map = entries
            .iter()
            .map(|(url, keys)| (url.to_string(), keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()))
            .collect();
        leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                additional_endpoints: leaf::AdditionalEndpoints(map),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn additional_endpoints_api_key_index_uses_raw_config_position() {
        // Keys at positions 0, 1 are valid; position 2 is empty (skipped); position 3 is a
        // duplicate of position 0 (skipped); position 4 is valid. Only positions 0, 1, 4 produce
        // ResolvedEndpoints, and their api_key_index should be 0, 1, 4 respectively.
        let endpoints = additional_endpoints(&[("app.datadoghq.com", &["key-a", "key-b", "", "key-a", "key-c"])]);

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
        let endpoints2 = additional_endpoints(&[("app.datadoghq.eu", &["eu-key-a", "eu-key-b"])]);
        let resolved2 = endpoints2.resolved_endpoints(None).expect("should resolve");
        assert_eq!(resolved2[0].api_key_index(), Some(0));
        assert_eq!(resolved2[1].api_key_index(), Some(1));
    }

    #[tokio::test]
    async fn api_key_dynamically_refreshes_from_additional_endpoints_config() {
        use std::time::{Duration, Instant};

        let initial = forwarder_config_with_additional(&[("http://extra.example.com", &["key-1"])]);
        let (tx, rx) = watch::channel(initial.clone());
        let live = ScopedConfig::live(initial, rx);

        // Build the additional endpoint with a live config reference.
        let additional = additional_endpoints(&[("http://extra.example.com", &["key-1"])]);
        let mut endpoints = additional.resolved_endpoints(Some(live)).expect("should resolve");
        let endpoint = &mut endpoints[0];

        // Before the update, api_key() returns the original key.
        assert_eq!(endpoint.api_key(), "key-1");

        // Publish a config that rotates the key.
        tx.send(forwarder_config_with_additional(&[(
            "http://extra.example.com",
            &["key-2"],
        )]))
        .expect("receiver alive");

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
}

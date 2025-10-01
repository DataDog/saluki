use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use http::uri::Authority;
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, OneOrMany, PickFirst};
use snafu::{ResultExt, Snafu};
use tracing::debug;
use url::Url;

static DD_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap());

pub const DEFAULT_SITE: &str = "datadoghq.com";

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
}

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub(crate) enum EndpointError {
    Parse { source: url::ParseError, endpoint: String },
}

#[serde_as]
#[derive(Clone, Debug, Default, Deserialize)]
struct APIKeys(#[serde_as(as = "OneOrMany<_>")] Vec<String>);

#[derive(Clone, Debug, Default, Deserialize)]
struct MappedAPIKeys(HashMap<String, APIKeys>);

impl MappedAPIKeys {
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

/// A set of additional API endpoints to forward metrics to.
///
/// Each endpoint can be associated with multiple API keys. Requests will be forwarded to each unique endpoint/API key pair.
#[serde_as]
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct AdditionalEndpoints(#[serde_as(as = "PickFirst<(DisplayFromStr, _)>")] MappedAPIKeys);

impl AdditionalEndpoints {
    /// Returns the resolved endpoints from the additional endpoint configuration.
    ///
    /// This will generate a `ResolvedEndpoint` for each unique endpoint/API key pair.
    ///
    /// # Errors
    ///
    /// If any of the additional endpoints are not valid URLs, or a valid URL could not be constructed after applying
    /// the necessary normalization / modifications, an error will be returned.
    pub fn resolved_endpoints(&self) -> Result<Vec<ResolvedEndpoint>, EndpointError> {
        let mut resolved = Vec::new();

        for (raw_endpoint, api_keys) in self.0.mappings() {
            let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
            let logs_authority = compute_logs_authority(&endpoint);

            // With our fully parsed and versioned endpoint, we'll now create a resolved version for each associated API
            // key attached to it.
            let mut seen = HashSet::new();
            for api_key in &api_keys.0 {
                // Filter out empty or duplicate API keys for this endpoint.
                let trimmed_api_key = api_key.trim();
                if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                    continue;
                }

                seen.insert(trimmed_api_key);
                resolved.push(ResolvedEndpoint {
                    endpoint: endpoint.clone(),
                    api_key: trimmed_api_key.to_string(),
                    config: None,
                    logs_authority: logs_authority.clone(),
                });
            }
        }

        Ok(resolved)
    }
}

/// Endpoint configuration for sending payloads to the Datadog platform.
#[derive(Clone, Deserialize)]
pub struct EndpointConfiguration {
    /// The API key to use.
    api_key: String,

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
    /// This takes precedence over `site`, and is not altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (e.g. `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    #[serde(default)]
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

    /// Clears all additional endpoints.
    pub fn clear_additional_endpoints(&mut self) {
        self.additional_endpoints = AdditionalEndpoints::default();
    }

    /// Builds the resolved endpoints from the endpoint configuration.
    ///
    /// This will generate a `ResolvedEndpoint` for each unique endpoint/API key pair, which includes the "primary"
    /// endpoint defined by `site`/`dd_url` and any additional endpoints defined in `additional_endpoints`.
    ///
    /// # Errors
    ///
    /// If any of the additional endpoints are not valid URLs, or a valid URL could not be constructed after applying
    /// the necessary normalization / modifications to a particular endpoint, an error will be returned.
    pub fn build_resolved_endpoints(
        &self, configuration: Option<GenericConfiguration>,
    ) -> Result<Vec<ResolvedEndpoint>, GenericError> {
        let primary_endpoint = calculate_resolved_endpoint(self.dd_url.as_deref(), &self.site, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")?
            .with_configuration(configuration);

        let additional_endpoints = self
            .additional_endpoints
            .resolved_endpoints()
            .error_context("Failed parsing/resolving the additional destination endpoints.")?;

        let mut endpoints = additional_endpoints;
        endpoints.insert(0, primary_endpoint);

        Ok(endpoints)
    }
}

/// A single API endpoint and its associated API key.
///
/// An endpoint is defined as a unique, fully-qualified domain name that metrics will be sent to, such as
/// `https://app.datadoghq.com`.
#[derive(Clone, Debug)]
pub struct ResolvedEndpoint {
    endpoint: Url,
    api_key: String,
    config: Option<GenericConfiguration>,
    /// Pre-computed logs intake authority (e.g., `agent-http-intake.logs.datadoghq.com`).
    /// This is derived from the endpoint host when it contains `.agent.` marker.
    logs_authority: Option<Authority>,
}

impl ResolvedEndpoint {
    /// Creates a new `ResolvedEndpoint` instance from the given endpoint and API key, normalizing and modifying the
    /// endpoint as necessary.
    ///
    /// # Errors
    ///
    /// If the given endpoint is not a valid URL, or a valid URL could not be constructed after applying the necessary
    /// normalization / modifications, an error will be returned.
    fn from_raw_endpoint(raw_endpoint: &str, api_key: &str) -> Result<Self, EndpointError> {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        let logs_authority = compute_logs_authority(&endpoint);
        Ok(Self {
            endpoint,
            api_key: api_key.to_string(),
            config: None,
            logs_authority,
        })
    }

    /// Creates a new  `ResolvedEndpoint` instance from an existing `ResolvedEndpoint`, adding an optional `GenericConfiguration` which can be used to fetch the up-to-date API key.
    pub fn with_configuration(self, config: Option<GenericConfiguration>) -> Self {
        Self {
            endpoint: self.endpoint,
            api_key: self.api_key,
            config,
            logs_authority: self.logs_authority,
        }
    }

    /// Returns the endpoint of the resolver.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the API key associated with the endpoint.
    ///
    /// If a [`GenericConfiguration`] has been configured, the API key will be queried from the configuration and
    /// stored if it has been updated since the last time `api_key` was called.
    pub fn api_key(&mut self) -> &str {
        if let Some(config) = &self.config {
            match config.try_get_typed::<String>("api_key") {
                Ok(Some(api_key)) => {
                    if !api_key.is_empty() && self.api_key != api_key {
                        debug!(endpoint = %self.endpoint, "Refreshed API key.");
                        self.api_key = api_key;
                    }
                }
                Ok(None) | Err(_) => {
                    debug!("Failed to retrieve API key from remote source (missing or wrong type). Continuing with last known valid API key.");
                }
            }
        }
        self.api_key.as_str()
    }

    /// Returns the API key associated with the endpoint without refreshing it.
    pub fn cached_api_key(&self) -> &str {
        self.api_key.as_str()
    }

    /// Returns the pre-computed logs intake authority, if available.
    ///
    /// This authority is derived from the endpoint host when it contains the `.agent.` marker,
    /// and is used for routing log payloads to the appropriate logs intake host.
    pub fn logs_authority(&self) -> Option<&Authority> {
        self.logs_authority.as_ref()
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
/// This generates a prefix that is similar in format to the one generated by Datadog Agent for determining the endpoint
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
/// If the given API endpoint does not include a scheme, `https` is assumed. As well, if the endpoint does not represent
/// an official Datadog API endpoint, it will not be modified.
///
/// # Errors
///
/// If the given API endpoint cannot be parsed as a valid URL, an error will be returned.
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
/// If an override URL is provided and cannot be parsed, or if a valid endpoint cannot be constructed from the given
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

/// Computes the logs intake authority from a resolved endpoint URL.
///
/// If the endpoint host contains the `.agent.` marker (e.g., `7-52-0-adp.agent.datadoghq.com`),
/// this extracts the site suffix and constructs the logs intake host in the form
/// `agent-http-intake.logs.{site}`.
///
/// Returns `None` if the host doesn't contain the marker or if the authority cannot be parsed.
fn compute_logs_authority(endpoint: &Url) -> Option<Authority> {
    const AGENT_HOST_MARKER: &str = ".agent.";

    let host = endpoint.host_str()?;
    let idx = host.find(AGENT_HOST_MARKER)?;
    let site = &host[idx + AGENT_HOST_MARKER.len()..];
    let logs_host = format!("agent-http-intake.logs.{}", site);

    Authority::from_str(&logs_host).ok()
}

#[cfg(test)]
mod tests {
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

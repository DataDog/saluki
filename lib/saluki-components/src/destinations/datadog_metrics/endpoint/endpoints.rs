use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use regex::Regex;
use saluki_metadata;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, OneOrMany, PickFirst};
use snafu::{ResultExt, Snafu};
use tracing::debug;
use url::Url;

use crate::destinations::datadog_metrics::DEFAULT_SITE;

static DD_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap());

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum EndpointError {
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
pub struct AdditionalEndpoints(#[serde_as(as = "PickFirst<(DisplayFromStr, _)>")] MappedAPIKeys);

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
                });
            }
        }

        Ok(resolved)
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
}

impl ResolvedEndpoint {
    /// Creates a new `ResolvedEndpoint` instance from the given endpoint and API key, normalizing and modifying the
    /// endpoint as necessary.
    ///
    /// # Errors
    ///
    /// If the given endpoint is not a valid URL, or a valid URL could not be constructed after applying the necessary
    /// normalization / modifications, an error will be returned.
    pub fn from_raw_endpoint(raw_endpoint: &str, api_key: &str) -> Result<Self, EndpointError> {
        let endpoint = parse_and_normalize_endpoint(raw_endpoint)?;
        Ok(Self {
            endpoint,
            api_key: api_key.to_string(),
        })
    }

    /// Returns the endpoint of the resolver.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the API key associated with the endpoint.
    pub fn api_key(&self) -> &str {
        self.api_key.as_str()
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
pub fn calculate_resolved_endpoint(
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

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use http::Uri;
use regex::Regex;
use saluki_error::GenericError;
use saluki_metadata;
use serde::Deserialize;
use snafu::{ResultExt, Snafu};
use tracing::debug;
use url::Url;

use crate::destinations::datadog_metrics::DEFAULT_SITE;

static DD_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap());

/// A set of additional API endpoints to forward metrics to.
///
/// Each endpoint can be associated with multiple API keys. Requests will be forwarded to each unique endpoint/API key pair.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AdditionalEndpoints(HashMap<String, Vec<String>>);

/// Error type for invalid endpoints.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum EndpointError {
    Parse { source: url::ParseError, domain: String },
}

/// A single API endpoint and its associated API keys.
#[derive(Default)]
pub struct SingleDomainResolver {
    domain: String,
    api_keys: Vec<String>,
}

impl SingleDomainResolver {
    /// Returns the domain of the resolver.
    pub fn domain(&self) -> &String {
        &self.domain
    }

    /// Returns the API keys associated with the domain.
    pub fn api_keys(&self) -> &Vec<String> {
        &self.api_keys
    }

    fn add_api_key(&mut self, api_key: String) {
        self.api_keys.push(api_key);
    }

    fn set_domain(&mut self, domain: String) {
        self.domain = domain;
    }
}

/// Converts [`AdditionalEndpoints`] into a list of [`SingleDomainResolver`]s.
pub fn create_single_domain_resolvers(
    endpoints: &AdditionalEndpoints,
) -> Result<Vec<SingleDomainResolver>, EndpointError> {
    let mut resolvers = Vec::new();
    for (domain, api_keys) in &endpoints.0 {
        let new_domain = add_adp_version_to_domain(domain.to_string())?;
        let mut seen = HashSet::new();
        let mut resolver = SingleDomainResolver::default();
        for api_key in api_keys {
            let trimmed_api_key = api_key.trim();
            if trimmed_api_key.is_empty() || seen.contains(trimmed_api_key) {
                continue;
            }
            seen.insert(trimmed_api_key);
            resolver.add_api_key(trimmed_api_key.to_string());
        }
        if !resolver.api_keys.is_empty() {
            resolver.set_domain(new_domain.trim_end_matches("/").to_string());
            resolvers.push(resolver);
        }
    }
    Ok(resolvers)
}

impl FromStr for AdditionalEndpoints {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let endpoints: HashMap<String, Vec<String>> = serde_json::from_str(s)?;
        Ok(AdditionalEndpoints(endpoints))
    }
}

/// Returns a specialized domain prefix based on the versioning of the current application.
///
/// This generates a prefix that is similar in format to the one generated by Datadog Agent for determining the endpoint
/// to send metrics to.
fn get_domain_prefix() -> String {
    let app_details = saluki_metadata::get_app_details();
    let version = app_details.version();
    format!(
        "{}-{}-{}-{}.agent",
        version.major(),
        version.minor(),
        version.patch(),
        app_details.short_name(),
    )
}

/// Prefixes the domain with the ADP version.
fn add_adp_version_to_domain(domain: String) -> Result<String, EndpointError> {
    // If we just have the base domain, turn it into a full URL first before parsing.
    let new_domain = if !domain.starts_with("https://") && !domain.starts_with("http://") {
        format!("https://{}", domain)
    } else {
        domain
    };

    let url = Url::parse(&new_domain).with_context(|_| Parse {
        domain: new_domain.clone(),
    })?;

    match url.host_str() {
        Some(host) => {
            // Do not update non-official Datadog URLs.
            if !DD_URL_REGEX.is_match(host) {
                debug!("Configured endpoint '{}' appears to be a non-Datadog endpoint. Utilizing endpoint without modification.", host);
                return Ok(host.to_string());
            }

            // We expect to be getting a domain that has at least one subdomain portion (i.e., `app.datadoghq.com`) if
            // not more. We're aiming to simply replace the leftmost subdomain portion with the version prefix.
            let leftmost_segment = host.split('.').next().unwrap_or("");
            let versioned_segment = get_domain_prefix();
            let new_host = host.replacen(leftmost_segment, &versioned_segment, 1);

            Ok(new_host)
        }
        None => Err(EndpointError::Parse {
            source: url::ParseError::EmptyHost,
            domain: new_domain.clone(),
        }),
    }
}

/// Calculates the correct API endpoint to use based on the given override URL and site settings.
///
/// # Errors
///
/// If an override URL is provided and cannot be parsed, or if a valid endpoint cannot be constructed from the given
/// site, an error will be returned.
pub fn calculate_api_endpoint(dd_url: Option<&str>, site: &str) -> Result<Uri, GenericError> {
    match dd_url {
        // If an override URL is provided, use it directly.
        Some(url) => Uri::try_from(url).map_err(Into::into),
        None => {
            // When using the site site, we'll provide the default US-based site if the site value is empty. Overall,
            // we'll do a little prefixing and cleanup to normalize things.
            //
            // This includes ensuring it bears the "infrastructure" subdomain -- `app.` -- and the correct version
            // prefix relative to the current application version.
            let base_domain = if site.is_empty() { DEFAULT_SITE } else { site };
            let prefixed_domain = format!("app.{}", base_domain);
            let versioned_domain = add_adp_version_to_domain(prefixed_domain)?;

            Uri::builder()
                .scheme("https")
                .authority(versioned_domain)
                .path_and_query("/")
                .build()
                .map_err(Into::into)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_version() {
        let urls = [
            "https://app.datadoghq.com",     // US
            "https://app.datadoghq.eu",      // EU
            "app.ddog-gov.com",              // Gov
            "app.us2.datadoghq.com",         // Additional Site
            "https://app.xx9.datadoghq.com", // Arbitrary site
        ];
        let expected_urls = [
            ".datadoghq.com",
            ".datadoghq.eu",
            ".ddog-gov.com",
            ".us2.datadoghq.com",
            ".xx9.datadoghq.com",
        ];

        for (url, expected_url) in urls.iter().zip(expected_urls) {
            let prefix = get_domain_prefix();
            let actual = add_adp_version_to_domain(url.to_string()).expect("error adding version to domain");
            assert_eq!(format!("{}{}", prefix, expected_url), actual);
        }
    }

    #[test]
    fn skip_version() {
        let urls = [
            "https://custom.datadoghq.com",       // Custom
            "https://custom.agent.datadoghq.com", // Custom with 'agent' subdomain
            "https://app.custom.datadoghq.com",   // Custom
            "https://app.datadoghq.internal",     // Custom top-level domain
            "https://app.myproxy.com",            // Proxy
        ];
        let expected_urls = [
            "custom.datadoghq.com",
            "custom.agent.datadoghq.com",
            "app.custom.datadoghq.com",
            "app.datadoghq.internal",
            "app.myproxy.com",
        ];

        for (url, expected_url) in urls.iter().zip(expected_urls) {
            let actual = add_adp_version_to_domain(url.to_string()).expect("error adding version to domain");
            assert_eq!(format!("{}", expected_url), actual);
        }
    }

    #[test]
    fn calculate_api_endpoint_no_override_no_site() {
        let prefix = get_domain_prefix();
        let expected_uri = format!("https://{}.{}/", prefix, DEFAULT_SITE);

        let uri = calculate_api_endpoint(None, "").expect("error calculating default API endpoint");
        assert_eq!(expected_uri, uri.to_string());
    }

    #[test]
    fn calculate_api_endpoint_no_override() {
        let site = "us3.datadoghq.com";
        let prefix = get_domain_prefix();
        let expected_uri = format!("https://{}.{}/", prefix, site);

        let uri = calculate_api_endpoint(None, "us3.datadoghq.com").expect("error calculating custom API endpoint");
        assert_eq!(expected_uri, uri.to_string());
    }

    #[test]
    fn calculate_api_endpoint_no_site() {
        let override_url = "https://dogpound.io";
        let expected_uri = Uri::try_from(override_url).expect("should not fail to parse override URL");

        let actual_uri =
            calculate_api_endpoint(Some(override_url), "").expect("error calculating override API endpoint");
        assert_eq!(expected_uri, actual_uri);
    }

    #[test]
    fn calculate_api_endpoint_override_and_site() {
        let override_url = "https://dogpound.io";
        let expected_uri = Uri::try_from(override_url).expect("should not fail to parse override URL");

        let actual_uri = calculate_api_endpoint(Some(override_url), "us3.datadoghq.com")
            .expect("error calculating override API endpoint");
        assert_eq!(expected_uri, actual_uri);
    }
}

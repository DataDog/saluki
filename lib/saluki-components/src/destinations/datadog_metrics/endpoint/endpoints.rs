use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
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

/// Return the correct prefix for a domain.
pub fn get_domain_prefix(app: String) -> String {
    let app_details = saluki_metadata::get_app_details();
    let version = app_details.version();
    format!(
        "{}-{}-{}-{}.agent",
        version.major(),
        version.minor(),
        version.patch(),
        app
    )
}

/// Prefixes the domain with the ADP version.
pub fn add_adp_version_to_domain(domain: String) -> Result<String, EndpointError> {
    let dd_url_regexp = Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap();

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
            // Do not update unknown URLs
            if !dd_url_regexp.is_match(host) {
                debug!("Configured endpoint '{}' appears to be a non-Datadog endpoint. Utilizing endpoint without modification.", host);
                return Ok(host.to_string());
            }
            let subdomain = host.split('.').next().unwrap_or("");
            let new_subdomain = get_domain_prefix("adp".to_string());
            let new_host = host.replacen(subdomain, &new_subdomain, 1);

            Ok(new_host)
        }
        None => Err(EndpointError::Parse {
            source: url::ParseError::EmptyHost,
            domain: new_domain.clone(),
        }),
    }
}

/// Constructs the correct Uri based on DD_URL and site values.
pub fn determine_base(dd_url: &Option<String>, site: &str) -> Result<Uri, GenericError> {
    match &dd_url {
        Some(url) => Uri::try_from(url).map_err(Into::into),
        None => {
            let site = if site.is_empty() { DEFAULT_SITE } else { site };
            let authority = add_adp_version_to_domain(site.to_string())?;

            Uri::builder()
                .scheme("https")
                .authority(authority.as_str())
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
            let prefix = get_domain_prefix("adp".to_string());
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
}

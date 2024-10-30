use std::collections::{HashMap, HashSet};

use http::Uri;
use regex::Regex;
use saluki_error::GenericError;
use saluki_metadata;
use serde::{Deserialize, Deserializer};
use snafu::{ResultExt, Snafu};
use url::Url;

use crate::destinations::datadog_metrics::DEFAULT_SITE;

#[derive(Clone, Debug, Default)]
/// AdditionalEndpoints holds the endpoints and api keys parsed
/// from the user configuration.
pub struct AdditionalEndpoints {
    pub endpoints: HashMap<String, Vec<String>>,
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
/// Error type for invalid endpoints.
pub enum EndpointError {
    Parse { source: url::ParseError, domain: String },
}

#[derive(Default)]
/// Holds the mapping between domains and their api keys.
pub struct SingleDomainResolver {
    domain: String,
    api_keys: Vec<String>,
}

impl SingleDomainResolver {
    /// The domain of the resolver.
    pub fn domain(&self) -> &String {
        &self.domain
    }

    /// The api keys associated with the domain.
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

/// Converts AdditionalEndpoints into a list of SingleDomainResolvers with its
/// destination domain and api keys.
pub fn create_single_domain_resolvers(
    endpoints: &AdditionalEndpoints,
) -> Result<Vec<SingleDomainResolver>, EndpointError> {
    let mut resolvers = Vec::new();
    for (domain, api_keys) in &endpoints.endpoints {
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

impl AdditionalEndpoints {
    fn from_string(s: &str) -> Self {
        let endpoints: HashMap<String, Vec<String>> =
            serde_json::from_str(s).expect("Failed to parse string as JSON HashMap");
        AdditionalEndpoints { endpoints }
    }
}

impl<'de> Deserialize<'de> for AdditionalEndpoints {
    fn deserialize<D>(deserializer: D) -> Result<AdditionalEndpoints, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        Ok(AdditionalEndpoints::from_string(&s))
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

    let mut new_domain = domain.clone();

    // Url::Parse doesnt work without this
    if !domain.starts_with("https://") && !domain.starts_with("http://") {
        new_domain = format!("https://{}", domain);
    }

    let d = new_domain.clone();
    let url = Url::parse(&new_domain).context(Parse { domain })?;

    if let Some(host) = url.host_str() {
        // Do not update unknown URLs
        if !dd_url_regexp.is_match(host) {
            return Ok(host.to_string());
        }
        let subdomain = host.split('.').next().unwrap_or("");

        let new_subdomain = get_domain_prefix("adp".to_string());

        let new_host = &host.replacen(subdomain, &new_subdomain, 1);

        return Ok(new_host.to_string());
    }
    Err(EndpointError::Parse {
        source: url::ParseError::EmptyHost,
        domain: d,
    })
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

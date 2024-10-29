use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Deserializer};
use snafu::{ResultExt, Snafu};
use url::Url;

#[derive(Clone, Debug, Default)]
#[allow(unused)]
pub struct AdditionalEndpoints {
    pub endpoints: HashMap<String, Vec<String>>,
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum EndpointError {
    Parse { source: url::ParseError, domain: String },
}

#[allow(unused)]
#[derive(Default)]
pub struct SingleDomainResolver {
    domain: String,
    api_keys: Vec<String>,
}

impl SingleDomainResolver {
    pub fn domain(&self) -> &String {
        &self.domain
    }

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

pub fn create_single_domain_resolvers(
    endpoints: &AdditionalEndpoints,
) -> Result<Vec<SingleDomainResolver>, EndpointError> {
    let mut resolvers = Vec::new();
    for (domain, api_keys) in &endpoints.endpoints {
        let new_domain = add_adp_version_to_domain(domain.to_string(), "adp".to_string())?;
        let mut seen = HashMap::new();
        let mut resolver = SingleDomainResolver::default();
        for api_key in api_keys {
            let trimmed_api_key = api_key.trim();
            if trimmed_api_key.is_empty() || seen.contains_key(trimmed_api_key) {
                continue;
            }
            seen.insert(trimmed_api_key, true);
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
    format!("0-1-0-{}.agent", app)
}

/// Prefixes the domain with the ADP version.
pub fn add_adp_version_to_domain(domain: String, app: String) -> Result<String, EndpointError> {
    let dd_url_regexp = Regex::new(r"^app(\.mrf)?(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap();

    let d = domain.clone();
    let url = Url::parse(&domain).context(Parse { domain })?;

    if let Some(host) = url.host_str() {
        // Do not update unknown URLs
        if !dd_url_regexp.is_match(host) {
            return Ok(d);
        }
        let subdomain = host.split('.').next().unwrap_or("");

        let new_subdomain = get_domain_prefix(app);

        let new_host = &host.replacen(subdomain, &new_subdomain, 1);

        return Ok(new_host.to_string());
    }
    Err(EndpointError::Parse {
        source: url::ParseError::EmptyHost,
        domain: d,
    })
}

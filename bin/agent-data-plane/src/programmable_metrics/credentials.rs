//! The credentials recipients send with: what each credential reference resolves to, over the live configuration.
//!
//! Plans name credentials by reference, never by value (see [`resources`](super::resources)). A delivery host builds
//! each recipient's credential cell from the live view its reference resolves to through [`AdpCredentialSource`], so a
//! rotation reaches the next request without a plan change:
//!
//! | Reference | Resolves to |
//! |---|---|
//! | `native:api_key` | `api_key` |
//! | `native:additional/<endpoint digest>/<index>` | key `<index>` of the additional endpoint of that URL digest |
//! | `native:multi_region_failover.api_key` | `multi_region_failover.api_key` |
//! | `native:cluster_agent.auth_token` | `cluster_agent.auth_token` |
//! | `native:proxy.http`, `native:proxy.https` | the `<user>:<password>` of the `proxy.http` or `proxy.https` URL |
//!
//! A reference resolves to `None` when the configuration has no value for it: an endpoint or key no longer configured,
//! or a proxy URL without user information. A native key is passed as configured, blank included, so that a delivery
//! host applies the native rules of storing a key (a blank or invalid update keeps the last usable key).

use agent_data_plane_config::{Live, SalukiConfiguration};
use saluki_components::forwarders::delivery::{CredentialSource, ProxySettings};
use saluki_metric_plan::canonical::PlanHash;
use stringtheory::MetaString;
use url::Url;

use super::resources::{ADDITIONAL_API_KEY_PREFIX, CLUSTER_AGENT_TOKEN, MRF_API_KEY, PRIMARY_API_KEY};

/// A credential reference, parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Reference {
    /// The primary API key.
    PrimaryApiKey,

    /// One key of an additional endpoint, by the first 16 hexadecimal digits of the digest of its configured URL.
    Additional { endpoint_digest: String, index: usize },

    /// The multi-region failover API key.
    MrfApiKey,

    /// The Cluster Agent's bearer token.
    ClusterAgentToken,

    /// The user information of a proxy URL: the `http` one, or the `https` one.
    Proxy { https: bool },
}

impl Reference {
    /// Parses a reference, or returns `None` for one no configuration value stands behind.
    fn parse(reference: &str) -> Option<Self> {
        match reference {
            PRIMARY_API_KEY => return Some(Self::PrimaryApiKey),
            MRF_API_KEY => return Some(Self::MrfApiKey),
            CLUSTER_AGENT_TOKEN => return Some(Self::ClusterAgentToken),
            ProxySettings::NATIVE_HTTP_CREDENTIAL => return Some(Self::Proxy { https: false }),
            ProxySettings::NATIVE_HTTPS_CREDENTIAL => return Some(Self::Proxy { https: true }),
            _ => {}
        }
        let (endpoint_digest, index) = reference.strip_prefix(ADDITIONAL_API_KEY_PREFIX)?.split_once('/')?;
        let digest = endpoint_digest.len() == 16 && endpoint_digest.bytes().all(|b| b.is_ascii_hexdigit());
        let index = index.parse().ok().filter(|_| digest)?;
        Some(Self::Additional {
            endpoint_digest: endpoint_digest.to_ascii_lowercase(),
            index,
        })
    }

    /// Returns the credential's value in `config`, or `None` if it is not configured there.
    fn value(&self, config: &SalukiConfiguration) -> Option<MetaString> {
        let shared = &config.shared;
        match self {
            Self::PrimaryApiKey => Some(shared.endpoints.api_key.as_str().into()),
            Self::Additional { endpoint_digest, index } => shared
                .endpoints
                .additional_endpoints
                .iter()
                .find(|(endpoint, _)| endpoint_digest_of(endpoint) == *endpoint_digest)
                .and_then(|(_, keys)| keys.get(*index))
                .map(|key| key.as_str().into()),
            Self::MrfApiKey => config
                .domains
                .multi_region_failover
                .api_key
                .as_deref()
                .map(MetaString::from),
            Self::ClusterAgentToken => shared.cluster_agent.auth_token.as_deref().map(MetaString::from),
            Self::Proxy { https } => {
                let proxy = &shared.endpoints.proxy;
                userinfo(if *https { &proxy.https } else { &proxy.http })
            }
        }
    }
}

/// Returns the first 16 hexadecimal digits of the digest of an additional endpoint's configured URL, as its keys'
/// references name it.
fn endpoint_digest_of(endpoint: &str) -> String {
    PlanHash::of_bytes(endpoint.as_bytes()).digest()[..8]
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// Returns the user information of a proxy URL, `<user>:<password>` or `<user>`, as the URL writes it, or `None` for a
/// URL without any, or one that does not parse.
fn userinfo(url: &str) -> Option<MetaString> {
    let url = Url::parse(url).ok()?;
    let user = url.username();
    match url.password() {
        Some(password) => Some(format!("{}:{}", user, password).into()),
        None if !user.is_empty() => Some(user.into()),
        None => None,
    }
}

/// The credentials of a process: each reference resolved to a live view of the configuration.
#[derive(Clone, Debug)]
pub struct AdpCredentialSource {
    config: Live<SalukiConfiguration>,
}

impl AdpCredentialSource {
    /// Creates the source over `config`, the live view of the whole configuration.
    pub fn new(config: Live<SalukiConfiguration>) -> Self {
        Self { config }
    }

    /// Returns the current value of the credential `reference` names, or `None` if it is not configured, or if the
    /// reference is not one this source knows.
    #[cfg(test)]
    pub fn current(&self, reference: &str) -> Option<MetaString> {
        Reference::parse(reference)?.value(&self.config)
    }
}

impl CredentialSource for AdpCredentialSource {
    fn resolve(&self, reference: &MetaString) -> Option<Live<Option<MetaString>>> {
        let reference = Reference::parse(reference)?;
        Some(self.config.map(move |config| reference.value(config)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::programmable_metrics::{resources::additional_api_key, test_support::ConfigCell};

    fn config() -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = "primary-key".to_string();
        config.shared.endpoints.additional_endpoints = HashMap::from([(
            "https://dual.example.com".to_string(),
            vec!["dual-0".to_string(), "dual-1".to_string()],
        )]);
        config.domains.multi_region_failover.api_key = Some("mrf-key".to_string());
        config.shared.endpoints.proxy.https = "http://user:p%40ss@proxy.example.com:3129".to_string();
        config.shared.endpoints.proxy.http = "http://proxy.example.com:3128".to_string();
        config
    }

    fn source(config: SalukiConfiguration) -> AdpCredentialSource {
        AdpCredentialSource::new(Live::new_fixed(config))
    }

    #[test]
    fn every_reference_resolves_to_its_configuration_value() {
        let source = source(config());
        let value = |reference: &str| source.current(reference).map(|value| value.to_string());
        assert_eq!(value("native:api_key").as_deref(), Some("primary-key"));
        // The digest is the first 16 hexadecimal digits of the SHA-256 of the configured URL, computed with `shasum -a
        // 256` over `https://dual.example.com`.
        assert_eq!(
            additional_api_key("https://dual.example.com", 1),
            "native:additional/fa8d2eae247dcfe1/1"
        );
        assert_eq!(value("native:additional/fa8d2eae247dcfe1/1").as_deref(), Some("dual-1"));
        assert_eq!(value("native:additional/fa8d2eae247dcfe1/2"), None, "no third key");
        assert_eq!(value("native:additional/0000000000000000/0"), None, "no such endpoint");
        assert_eq!(
            value("native:multi_region_failover.api_key").as_deref(),
            Some("mrf-key")
        );
        assert_eq!(value("native:cluster_agent.auth_token"), None);
        // The user information as the URL writes it, percent-encoding included; none without any.
        assert_eq!(value("native:proxy.https").as_deref(), Some("user:p%40ss"));
        assert_eq!(value("native:proxy.http"), None);
        // References no configuration value stands behind are unknown.
        for unknown in [
            "api_key",
            "credentials/team_d",
            "native:additional/zz/0",
            "native:other",
        ] {
            assert!(source.resolve(&unknown.into()).is_none(), "{}", unknown);
        }
    }

    #[tokio::test]
    async fn a_resolved_view_follows_the_configuration_and_wakes_only_on_its_own_value() {
        let cell = ConfigCell::new(config());
        let source = AdpCredentialSource::new(cell.live());
        let mut dual = source.resolve(&"native:additional/fa8d2eae247dcfe1/0".into()).unwrap();
        assert_eq!(dual.as_deref(), Some("dual-0"));

        // A rotation of another credential leaves the view waiting; its own rotation and its removal wake it.
        let mut changed = config();
        changed.shared.endpoints.api_key = "rotated-primary".to_string();
        cell.set(changed.clone());
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), dual.changed()).await;
        assert!(waited.is_err());
        changed.shared.endpoints.additional_endpoints.insert(
            "https://dual.example.com".to_string(),
            vec!["rotated-0".to_string(), "dual-1".to_string()],
        );
        cell.set(changed.clone());
        assert_eq!(dual.changed().await.as_deref(), Some("rotated-0"));
        changed.shared.endpoints.additional_endpoints.clear();
        cell.set(changed);
        assert_eq!(dual.changed().await, None, "a removed endpoint's key is not configured");
    }
}

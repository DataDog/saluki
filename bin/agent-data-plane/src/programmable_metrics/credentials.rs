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
//! | `credentials/<name>` | entry `<name>` of `experimental.programmable_metrics.resources.credentials` |
//!
//! A reference resolves to `None` when the configuration has no value for it: an endpoint or key no longer configured,
//! a proxy URL without user information, or a declared credential whose entry is absent or still reads `ENC[...]`
//! because no secret backend resolved it. A native key is passed as configured, blank included, so that a delivery
//! host applies the native rules of storing a key (a blank or invalid update keeps the last usable key).
//!
//! [`missing_credentials`] is what the installer core is told before each lowering: the declared credentials the
//! resources artifacts it holds reference that are not configured, which hold those artifacts pending.

use std::collections::BTreeSet;

use agent_data_plane_config::{Live, SalukiConfiguration};
use saluki_components::forwarders::delivery::{CredentialSource, ProxySettings};
use saluki_metric_plan::{
    canonical::PlanHash,
    ir::CredentialRef,
    resources::{ResourcesArtifact, CREDENTIALS_NAMESPACE},
};
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

    /// An entry of the credentials resources artifacts may use.
    Declared(String),
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
        if let Some(name) = reference.strip_prefix(CREDENTIALS_NAMESPACE) {
            return (!name.is_empty()).then(|| Self::Declared(name.to_string()));
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
            Self::Declared(name) => config
                .control
                .programmable_metrics
                .resources
                .credentials
                .get(name)
                .filter(|value| !is_unresolved_secret(value))
                .map(|value| value.as_str().into()),
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

/// Returns `true` for a value that is still a secret handle, `ENC[...]`, which only a secret backend resolves: ADP
/// resolves no secrets itself, so such a credential is not configured.
fn is_unresolved_secret(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("ENC[") && value.ends_with(']')
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

/// Returns the declared credentials that `artifacts` reference and `config` does not configure: absent from
/// `experimental.programmable_metrics.resources.credentials`, or still a secret handle.
pub fn missing_credentials<'a>(
    artifacts: impl IntoIterator<Item = &'a ResourcesArtifact>, config: &SalukiConfiguration,
) -> BTreeSet<CredentialRef> {
    artifacts
        .into_iter()
        .flat_map(|artifact| artifact.recipients.iter().map(|recipient| &recipient.credential))
        .filter(|credential| {
            !matches!(Reference::parse(credential.as_str()), Some(reference @ Reference::Declared(_))
                if reference.value(config).is_some())
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_metric_plan::{
        ir::HostId,
        resources::{DeclaredProtocol, DeclaredRecipient, RESOURCES_VERSION},
    };

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
        let credentials = &mut config.control.programmable_metrics.resources.credentials;
        credentials.insert("team_d".to_string(), "team-d-key".to_string());
        credentials.insert("sealed".to_string(), "ENC[team_e]".to_string());
        credentials.insert("blank".to_string(), String::new());
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
        assert_eq!(value("credentials/team_d").as_deref(), Some("team-d-key"));
        // A blank entry is configured, which keeps a host's last usable key; a secret handle and an absent entry are
        // not configured.
        assert_eq!(value("credentials/blank").as_deref(), Some(""));
        assert_eq!(value("credentials/sealed"), None);
        assert_eq!(value("credentials/team_e"), None);
        // References no configuration value stands behind are unknown.
        for unknown in ["api_key", "credentials/", "native:additional/zz/0", "native:other"] {
            assert!(source.resolve(&unknown.into()).is_none(), "{}", unknown);
        }
    }

    #[tokio::test]
    async fn a_resolved_view_follows_the_configuration_and_wakes_only_on_its_own_value() {
        let cell = ConfigCell::new(config());
        let source = AdpCredentialSource::new(cell.live());
        let mut team_d = source.resolve(&"credentials/team_d".into()).unwrap();
        let mut dual = source.resolve(&"native:additional/fa8d2eae247dcfe1/0".into()).unwrap();
        assert_eq!(team_d.as_deref(), Some("team-d-key"));

        // A rotation of another credential leaves the view waiting; its own rotation and its removal wake it.
        let mut changed = config();
        changed.shared.endpoints.api_key = "rotated-primary".to_string();
        cell.set(changed.clone());
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), team_d.changed()).await;
        assert!(waited.is_err());
        changed
            .control
            .programmable_metrics
            .resources
            .credentials
            .insert("team_d".to_string(), "rotated-d".to_string());
        cell.set(changed.clone());
        assert_eq!(team_d.changed().await.as_deref(), Some("rotated-d"));
        changed
            .control
            .programmable_metrics
            .resources
            .credentials
            .remove("team_d");
        changed.shared.endpoints.additional_endpoints.clear();
        cell.set(changed);
        assert_eq!(team_d.changed().await, None);
        assert_eq!(dual.changed().await, None, "a removed endpoint's key is not configured");
    }

    #[test]
    fn missing_credentials_are_the_declared_ones_not_configured() {
        let recipient = |credential: &str| DeclaredRecipient {
            name: "r".to_string(),
            host: HostId::new("dd_out").unwrap(),
            endpoint: "http://127.0.0.1:17105".to_string(),
            credential: CredentialRef::new(credential).unwrap(),
            protocol: DeclaredProtocol::default(),
            delivery: None,
        };
        let artifact = |credentials: &[&str]| ResourcesArtifact {
            resources_version: RESOURCES_VERSION,
            recipients: credentials.iter().map(|credential| recipient(credential)).collect(),
            outputs: Vec::new(),
        };
        let first = artifact(&["credentials/team_d", "credentials/team_e"]);
        let second = artifact(&["credentials/sealed", "credentials/blank", "native:api_key"]);
        let missing = missing_credentials([&first, &second], &config())
            .into_iter()
            .map(|credential| credential.to_string())
            .collect::<Vec<_>>();
        // A native reference is never available to a resources artifact, so it counts as missing too.
        assert_eq!(missing, ["credentials/sealed", "credentials/team_e", "native:api_key"]);
        assert!(missing_credentials([&first], &{
            let mut config = config();
            let credentials = &mut config.control.programmable_metrics.resources.credentials;
            credentials.insert("team_e".to_string(), "e".to_string());
            config
        })
        .is_empty());
    }
}

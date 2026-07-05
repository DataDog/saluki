//! Cluster Agent configuration.

use std::net::IpAddr;

use agent_data_plane_config::shared::ClusterAgent;
use saluki_error::GenericError;

const DEFAULT_CLUSTER_AGENT_KUBERNETES_SERVICE_NAME: &str = "datadog-cluster-agent";

/// Cluster Agent forwarding configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClusterAgentConfiguration {
    enabled: bool,
    url: Option<String>,
    kubernetes_service_name: Option<String>,
    auth_token: Option<String>,
}

impl ClusterAgentConfiguration {
    /// Builds a `ClusterAgentConfiguration` from the translated Cluster Agent settings.
    pub fn from_configuration(config: &ClusterAgent) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.enabled,
            url: config.url.clone(),
            kubernetes_service_name: config.kubernetes_service_name.clone(),
            auth_token: config.auth_token.clone(),
        })
    }

    /// Returns the Cluster Agent HTTPS endpoint and bearer token when forwarding can be configured.
    pub fn endpoint_and_token(&self) -> Option<(String, String)> {
        self.endpoint_and_token_with_env(|key| std::env::var(key).ok())
    }

    fn endpoint_and_token_with_env<F>(&self, env_lookup: F) -> Option<(String, String)>
    where
        F: Fn(&str) -> Option<String>,
    {
        if !self.enabled {
            return None;
        }

        let endpoint = self.resolve_endpoint(env_lookup)?;

        Some((endpoint, self.auth_token.clone()?))
    }

    fn resolve_endpoint<F>(&self, env_lookup: F) -> Option<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(url) = self.url.as_deref() {
            return normalize_cluster_agent_url(url);
        }

        let service_name = self
            .kubernetes_service_name
            .as_deref()
            .unwrap_or(DEFAULT_CLUSTER_AGENT_KUBERNETES_SERVICE_NAME);
        if service_name.is_empty() {
            return None;
        }

        resolve_kubernetes_service_endpoint(service_name, env_lookup)
    }
}

fn normalize_cluster_agent_url(url: &str) -> Option<String> {
    let normalized = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };

    let parsed = url::Url::parse(&normalized).ok()?;
    if parsed.scheme() == "https" && parsed.host_str().is_some() {
        Some(normalized)
    } else {
        None
    }
}

fn resolve_kubernetes_service_endpoint<F>(service_name: &str, env_lookup: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let env_prefix = service_name.to_uppercase().replace('-', "_");
    let host_env = format!("{env_prefix}_SERVICE_HOST");
    let port_env = format!("{env_prefix}_SERVICE_PORT");

    let host = env_lookup(&host_env)?.trim().to_string();
    let port = env_lookup(&port_env)?.trim().to_string();
    if host.is_empty() || port.is_empty() {
        return None;
    }

    normalize_cluster_agent_url(&join_host_port(&host, &port))
}

fn join_host_port(host: &str, port: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]:{port}"),
        _ => format!("{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_agent_config_from(config: ClusterAgent) -> ClusterAgentConfiguration {
        ClusterAgentConfiguration::from_configuration(&config).expect("Cluster Agent configuration should build")
    }

    #[test]
    fn endpoint_and_token_requires_enabled_cluster_agent() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: false,
            url: Some("https://cluster-agent.example.com".to_string()),
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_resolvable_endpoint() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: None,
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_https_url() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: Some("http://cluster-agent.example.com".to_string()),
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[test]
    fn endpoint_and_token_adds_https_scheme_to_url() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: Some("cluster-agent.example.com:5005".to_string()),
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(
            config.endpoint_and_token(),
            Some((
                "https://cluster-agent.example.com:5005".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn endpoint_and_token_requires_non_empty_token() {
        // The config layer normalizes a whitespace-only `auth_token` to `None`; the component then
        // has no bearer token and forwarding cannot be configured.
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: Some("https://cluster-agent.example.com".to_string()),
            auth_token: None,
            kubernetes_service_name: None,
        });

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[test]
    fn endpoint_and_token_returns_https_url_and_token() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: Some("https://cluster-agent.example.com".to_string()),
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(
            config.endpoint_and_token(),
            Some((
                "https://cluster-agent.example.com".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn endpoint_and_token_resolves_default_kubernetes_service_env() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: None,
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("DATADOG_CLUSTER_AGENT_SERVICE_HOST", "127.0.0.1"),
                ("DATADOG_CLUSTER_AGENT_SERVICE_PORT", "443"),
            ])),
            Some(("https://127.0.0.1:443".to_string(), "secret-token".to_string()))
        );
    }

    #[test]
    fn endpoint_and_token_resolves_configured_kubernetes_service_env() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: None,
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: Some("custom-cluster-agent".to_string()),
        });

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("CUSTOM_CLUSTER_AGENT_SERVICE_HOST", "10.0.0.7"),
                ("CUSTOM_CLUSTER_AGENT_SERVICE_PORT", "5005"),
            ])),
            Some(("https://10.0.0.7:5005".to_string(), "secret-token".to_string()))
        );
    }

    #[test]
    fn endpoint_and_token_wraps_kubernetes_service_ipv6_host() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: None,
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: None,
        });

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("DATADOG_CLUSTER_AGENT_SERVICE_HOST", "fd38:552b:2959::4f4a"),
                ("DATADOG_CLUSTER_AGENT_SERVICE_PORT", "5005"),
            ])),
            Some((
                "https://[fd38:552b:2959::4f4a]:5005".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn endpoint_and_token_prefers_configured_url_over_kubernetes_service_env() {
        let config = cluster_agent_config_from(ClusterAgent {
            enabled: true,
            url: Some("https://configured-cluster-agent.example.com".to_string()),
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: Some("custom-cluster-agent".to_string()),
        });

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("CUSTOM_CLUSTER_AGENT_SERVICE_HOST", "10.0.0.7"),
                ("CUSTOM_CLUSTER_AGENT_SERVICE_PORT", "5005"),
            ])),
            Some((
                "https://configured-cluster-agent.example.com".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    fn env_lookup<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            entries
                .iter()
                .find_map(|(entry_key, entry_value)| (*entry_key == key).then(|| (*entry_value).to_string()))
        }
    }
}

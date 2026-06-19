//! Cluster Agent configuration.

use std::net::IpAddr;

use saluki_config_tools::GenericConfiguration;
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
    /// Creates a new `ClusterAgentConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.try_get_typed("cluster_agent.enabled")?.unwrap_or(false),
            url: get_non_empty_string(config, "cluster_agent.url")?,
            kubernetes_service_name: get_trimmed_string(config, "cluster_agent.kubernetes_service_name")?,
            auth_token: get_non_empty_string(config, "cluster_agent.auth_token")?,
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

fn get_non_empty_string(config: &GenericConfiguration, key: &str) -> Result<Option<String>, GenericError> {
    Ok(get_trimmed_string(config, key)?.filter(|value| !value.is_empty()))
}

fn get_trimmed_string(config: &GenericConfiguration, key: &str) -> Result<Option<String>, GenericError> {
    Ok(config
        .try_get_typed::<String>(key)?
        .map(|value| value.trim().to_string()))
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
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::*;

    async fn cluster_agent_config_from(value: serde_json::Value) -> ClusterAgentConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(Some(value), None, false).await;
        ClusterAgentConfiguration::from_configuration(&config).expect("Cluster Agent configuration should deserialize")
    }

    #[tokio::test]
    async fn endpoint_and_token_requires_enabled_cluster_agent() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": false,
                "url": "https://cluster-agent.example.com",
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[tokio::test]
    async fn endpoint_and_token_requires_resolvable_endpoint() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[tokio::test]
    async fn endpoint_and_token_requires_https_url() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "http://cluster-agent.example.com",
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[tokio::test]
    async fn endpoint_and_token_adds_https_scheme_to_url() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "cluster-agent.example.com:5005",
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(
            config.endpoint_and_token(),
            Some((
                "https://cluster-agent.example.com:5005".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn endpoint_and_token_requires_non_empty_token() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "https://cluster-agent.example.com",
                "auth_token": "  "
            }
        }))
        .await;

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[tokio::test]
    async fn endpoint_and_token_returns_https_url_and_token() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "url": " https://cluster-agent.example.com ",
                "auth_token": " secret-token "
            }
        }))
        .await;

        assert_eq!(
            config.endpoint_and_token(),
            Some((
                "https://cluster-agent.example.com".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn endpoint_and_token_resolves_default_kubernetes_service_env() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("DATADOG_CLUSTER_AGENT_SERVICE_HOST", "127.0.0.1"),
                ("DATADOG_CLUSTER_AGENT_SERVICE_PORT", "443"),
            ])),
            Some(("https://127.0.0.1:443".to_string(), "secret-token".to_string()))
        );
    }

    #[tokio::test]
    async fn endpoint_and_token_resolves_configured_kubernetes_service_env() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "kubernetes_service_name": "custom-cluster-agent",
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("CUSTOM_CLUSTER_AGENT_SERVICE_HOST", "10.0.0.7"),
                ("CUSTOM_CLUSTER_AGENT_SERVICE_PORT", "5005"),
            ])),
            Some(("https://10.0.0.7:5005".to_string(), "secret-token".to_string()))
        );
    }

    #[tokio::test]
    async fn endpoint_and_token_wraps_kubernetes_service_ipv6_host() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "auth_token": "secret-token"
            }
        }))
        .await;

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

    #[tokio::test]
    async fn endpoint_and_token_prefers_configured_url_over_kubernetes_service_env() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "https://configured-cluster-agent.example.com",
                "kubernetes_service_name": "custom-cluster-agent",
                "auth_token": "secret-token"
            }
        }))
        .await;

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

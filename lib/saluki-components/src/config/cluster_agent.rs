//! Cluster Agent configuration.

use std::net::IpAddr;

use saluki_component_config::ClusterAgentConfig;

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
    /// Creates a new `ClusterAgentConfiguration` from native typed config.
    pub fn from_native(config: ClusterAgentConfig) -> Self {
        Self {
            enabled: config.enabled,
            url: config
                .url
                .as_deref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            kubernetes_service_name: config.kubernetes_service_name.as_deref().map(|s| s.trim().to_string()),
            auth_token: config
                .auth_token
                .as_deref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
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
    use saluki_component_config::ClusterAgentConfig;

    use super::*;

    fn cluster_agent_config(
        enabled: bool, url: Option<&str>, kubernetes_service_name: Option<&str>, auth_token: Option<&str>,
    ) -> ClusterAgentConfiguration {
        ClusterAgentConfiguration::from_native(ClusterAgentConfig {
            enabled,
            url: url.map(str::to_string),
            kubernetes_service_name: kubernetes_service_name.map(str::to_string),
            auth_token: auth_token.map(str::to_string),
        })
    }

    #[test]
    fn endpoint_and_token_requires_enabled_cluster_agent() {
        let config = cluster_agent_config(
            false,
            Some("https://cluster-agent.example.com"),
            None,
            Some("secret-token"),
        );

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_resolvable_endpoint() {
        let config = cluster_agent_config(true, None, None, Some("secret-token"));

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_https_url() {
        let config = cluster_agent_config(
            true,
            Some("http://cluster-agent.example.com"),
            None,
            Some("secret-token"),
        );

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[test]
    fn endpoint_and_token_adds_https_scheme_to_url() {
        let config = cluster_agent_config(true, Some("cluster-agent.example.com:5005"), None, Some("secret-token"));

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
        let config = cluster_agent_config(true, Some("https://cluster-agent.example.com"), None, Some("  "));

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[test]
    fn endpoint_and_token_returns_https_url_and_token() {
        let config = cluster_agent_config(
            true,
            Some(" https://cluster-agent.example.com "),
            None,
            Some(" secret-token "),
        );

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
        let config = cluster_agent_config(true, None, None, Some("secret-token"));

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
        let config = cluster_agent_config(true, None, Some("custom-cluster-agent"), Some("secret-token"));

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
        let config = cluster_agent_config(true, None, None, Some("secret-token"));

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
        let config = cluster_agent_config(
            true,
            Some("https://configured-cluster-agent.example.com"),
            Some("custom-cluster-agent"),
            Some("secret-token"),
        );

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

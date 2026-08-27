//! Cluster Agent configuration.

use std::net::IpAddr;

/// Cluster Agent forwarding configuration.
///
/// Every field arrives already resolved: configuration owns the defaults, trims the string values, and turns a blank
/// value into either `None` or an empty name. What is left here is the endpoint decision, which needs the process
/// environment and so cannot be made at the configuration boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClusterAgentConfiguration {
    /// Whether forwarding to the Cluster Agent is turned on.
    ///
    /// Defaults to `false` in configuration. When this is `false`, no endpoint is resolved and nothing is forwarded,
    /// whatever the other fields hold.
    pub enabled: bool,

    /// Configured Cluster Agent URL.
    ///
    /// Defaults to unset in configuration. A value is preferred over Kubernetes service discovery, and it is used only
    /// if it resolves to an `https` URL with a host: a URL without a scheme is completed to `https://`, and anything
    /// else yields no endpoint rather than falling back to service discovery.
    pub url: Option<String>,

    /// Bearer token presented on Cluster Agent requests.
    ///
    /// Defaults to unset in configuration, which leaves nothing to forward with: the Cluster Agent has no anonymous
    /// access, so an unset token yields no endpoint.
    pub auth_token: Option<String>,

    /// Kubernetes service name whose injected environment variables locate the Cluster Agent.
    ///
    /// Configuration defaults this to `datadog-cluster-agent`. The name is upper-cased, its dashes become underscores,
    /// and the `<NAME>_SERVICE_HOST` and `<NAME>_SERVICE_PORT` variables Kubernetes injects into the pod supply the
    /// endpoint. An empty name turns the lookup off, which is how a deployment asks that `url` be the only way to
    /// reach the Cluster Agent.
    pub kubernetes_service_name: String,
}

impl ClusterAgentConfiguration {
    /// Returns the Cluster Agent HTTPS endpoint and bearer token when forwarding can be configured.
    ///
    /// Returns `None` when forwarding is turned off, when no `https` endpoint resolves from either `url` or the
    /// Kubernetes service environment, or when `auth_token` is unset.
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

        if self.kubernetes_service_name.is_empty() {
            return None;
        }

        resolve_kubernetes_service_endpoint(&self.kubernetes_service_name, env_lookup)
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

    /// The name the Datadog schema supplies when `cluster_agent.kubernetes_service_name` is absent.
    const DEFAULT_SERVICE_NAME: &str = "datadog-cluster-agent";

    fn enabled_config() -> ClusterAgentConfiguration {
        ClusterAgentConfiguration {
            enabled: true,
            url: None,
            auth_token: Some("secret-token".to_string()),
            kubernetes_service_name: DEFAULT_SERVICE_NAME.to_string(),
        }
    }

    #[test]
    fn endpoint_and_token_requires_enabled_cluster_agent() {
        let config = ClusterAgentConfiguration {
            enabled: false,
            url: Some("https://cluster-agent.example.com".to_string()),
            ..enabled_config()
        };

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_resolvable_endpoint() {
        let config = enabled_config();

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_requires_https_url() {
        let config = ClusterAgentConfiguration {
            url: Some("http://cluster-agent.example.com".to_string()),
            ..enabled_config()
        };

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_adds_https_scheme_to_url() {
        let config = ClusterAgentConfiguration {
            url: Some("cluster-agent.example.com:5005".to_string()),
            ..enabled_config()
        };

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[])),
            Some((
                "https://cluster-agent.example.com:5005".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn endpoint_and_token_requires_a_token() {
        let config = ClusterAgentConfiguration {
            url: Some("https://cluster-agent.example.com".to_string()),
            auth_token: None,
            ..enabled_config()
        };

        assert_eq!(config.endpoint_and_token_with_env(env_lookup(&[])), None);
    }

    #[test]
    fn endpoint_and_token_returns_https_url_and_token() {
        let config = ClusterAgentConfiguration {
            url: Some("https://cluster-agent.example.com".to_string()),
            ..enabled_config()
        };

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[])),
            Some((
                "https://cluster-agent.example.com".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn endpoint_and_token_resolves_default_kubernetes_service_env() {
        let config = enabled_config();

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
        let config = ClusterAgentConfiguration {
            kubernetes_service_name: "custom-cluster-agent".to_string(),
            ..enabled_config()
        };

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("CUSTOM_CLUSTER_AGENT_SERVICE_HOST", "10.0.0.7"),
                ("CUSTOM_CLUSTER_AGENT_SERVICE_PORT", "5005"),
            ])),
            Some(("https://10.0.0.7:5005".to_string(), "secret-token".to_string()))
        );
    }

    #[test]
    fn an_empty_kubernetes_service_name_turns_discovery_off() {
        // Configuration resolves an explicitly blank name to an empty string, which means the injected environment
        // variables are ignored even when they are present.
        let config = ClusterAgentConfiguration {
            kubernetes_service_name: String::new(),
            ..enabled_config()
        };

        assert_eq!(
            config.endpoint_and_token_with_env(env_lookup(&[
                ("DATADOG_CLUSTER_AGENT_SERVICE_HOST", "127.0.0.1"),
                ("DATADOG_CLUSTER_AGENT_SERVICE_PORT", "443"),
            ])),
            None
        );
    }

    #[test]
    fn endpoint_and_token_wraps_kubernetes_service_ipv6_host() {
        let config = enabled_config();

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
        let config = ClusterAgentConfiguration {
            url: Some("https://configured-cluster-agent.example.com".to_string()),
            kubernetes_service_name: "custom-cluster-agent".to_string(),
            ..enabled_config()
        };

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

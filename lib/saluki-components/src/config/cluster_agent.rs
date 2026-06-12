//! Cluster Agent configuration.

use saluki_config::GenericConfiguration;
use saluki_error::GenericError;

/// Cluster Agent forwarding configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClusterAgentConfiguration {
    enabled: bool,
    url: Option<String>,
    auth_token: Option<String>,
}

impl ClusterAgentConfiguration {
    /// Creates a new `ClusterAgentConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.try_get_typed("cluster_agent.enabled")?.unwrap_or(false),
            url: get_non_empty_string(config, "cluster_agent.url")?,
            auth_token: get_non_empty_string(config, "cluster_agent.auth_token")?,
        })
    }

    /// Returns the Cluster Agent HTTPS endpoint and bearer token when forwarding can be configured.
    pub fn endpoint_and_token(&self) -> Option<(String, String)> {
        if !self.enabled {
            return None;
        }

        let url = self.url.as_ref()?;
        if !url.starts_with("https://") {
            return None;
        }

        Some((url.clone(), self.auth_token.clone()?))
    }
}

fn get_non_empty_string(config: &GenericConfiguration, key: &str) -> Result<Option<String>, GenericError> {
    Ok(config
        .try_get_typed::<String>(key)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;
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

        assert_eq!(config.endpoint_and_token(), None);
    }

    #[tokio::test]
    async fn endpoint_and_token_requires_url() {
        let config = cluster_agent_config_from(json!({
            "cluster_agent": {
                "enabled": true,
                "auth_token": "secret-token"
            }
        }))
        .await;

        assert_eq!(config.endpoint_and_token(), None);
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
}

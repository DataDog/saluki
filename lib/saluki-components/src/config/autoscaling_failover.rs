//! Autoscaling failover configuration.

use saluki_config_tools::GenericConfiguration;
use saluki_error::GenericError;

fn default_metrics() -> Vec<String> {
    vec!["container.memory.usage".to_string(), "container.cpu.usage".to_string()]
}

/// Autoscaling failover configuration for the metrics pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoscalingFailoverConfiguration {
    enabled: bool,
    metrics: Vec<String>,
}

impl AutoscalingFailoverConfiguration {
    /// Creates a new `AutoscalingFailoverConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.try_get_typed("autoscaling.failover.enabled")?.unwrap_or(false),
            metrics: config
                .try_get_typed("autoscaling.failover.metrics")?
                .unwrap_or_else(default_metrics),
        })
    }

    /// Returns whether the autoscaling failover branch is requested by configuration.
    pub fn is_branch_requested(&self) -> bool {
        self.enabled && !self.metrics.is_empty()
    }

    /// Returns the metric name allowlist.
    pub fn metrics(&self) -> &[String] {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::*;

    async fn autoscaling_config_from(value: serde_json::Value) -> AutoscalingFailoverConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(Some(value), None, false).await;
        AutoscalingFailoverConfiguration::from_configuration(&config)
            .expect("autoscaling failover configuration should deserialize")
    }

    #[tokio::test]
    async fn defaults_to_disabled_with_default_metric_allowlist() {
        let config = autoscaling_config_from(json!({})).await;

        assert!(!config.is_branch_requested());
        assert_eq!(
            config.metrics(),
            ["container.memory.usage".to_string(), "container.cpu.usage".to_string()]
        );
    }

    #[tokio::test]
    async fn branch_is_requested_when_enabled_with_non_empty_metrics() {
        let config = autoscaling_config_from(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": ["custom.metric"]
                }
            }
        }))
        .await;

        assert!(config.is_branch_requested());
        assert_eq!(config.metrics(), ["custom.metric".to_string()]);
    }

    #[tokio::test]
    async fn empty_metric_allowlist_disables_branch() {
        let config = autoscaling_config_from(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": []
                }
            }
        }))
        .await;

        assert!(!config.is_branch_requested());
        assert!(config.metrics().is_empty());
    }
}

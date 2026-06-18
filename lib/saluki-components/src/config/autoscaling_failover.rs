//! Autoscaling failover configuration.

use saluki_component_config::AutoscalingFailoverConfig;

/// Autoscaling failover configuration for the metrics pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoscalingFailoverConfiguration {
    enabled: bool,
    metrics: Vec<String>,
}

impl AutoscalingFailoverConfiguration {
    /// Creates a new `AutoscalingFailoverConfiguration` from native typed config.
    pub fn from_native(config: AutoscalingFailoverConfig) -> Self {
        Self {
            enabled: config.enabled,
            metrics: config.metrics,
        }
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
    use saluki_component_config::AutoscalingFailoverConfig;

    use super::*;

    #[test]
    fn defaults_to_disabled_with_default_metric_allowlist() {
        let config = AutoscalingFailoverConfiguration::from_native(AutoscalingFailoverConfig::default());

        assert!(!config.is_branch_requested());
        assert_eq!(
            config.metrics(),
            ["container.memory.usage".to_string(), "container.cpu.usage".to_string()]
        );
    }

    #[test]
    fn branch_is_requested_when_enabled_with_non_empty_metrics() {
        let config = AutoscalingFailoverConfiguration::from_native(AutoscalingFailoverConfig {
            enabled: true,
            metrics: vec!["custom.metric".to_string()],
        });

        assert!(config.is_branch_requested());
        assert_eq!(config.metrics(), ["custom.metric".to_string()]);
    }

    #[test]
    fn empty_metric_allowlist_disables_branch() {
        let config = AutoscalingFailoverConfiguration::from_native(AutoscalingFailoverConfig {
            enabled: true,
            metrics: vec![],
        });

        assert!(!config.is_branch_requested());
        assert!(config.metrics().is_empty());
    }
}

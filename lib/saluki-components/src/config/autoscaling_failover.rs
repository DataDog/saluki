//! Autoscaling failover configuration.

/// Autoscaling failover configuration for the metrics pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoscalingFailoverConfiguration {
    enabled: bool,
    metrics: Vec<String>,
}

impl AutoscalingFailoverConfiguration {
    /// Creates a new `AutoscalingFailoverConfiguration`.
    ///
    /// `enabled` is whether autoscaling failover is requested (`is_branch_requested` also requires a non-empty
    /// `metrics`), and `metrics` is the allowlist of metric names eligible for the failover branch. Both values arrive
    /// already resolved: the configuration layer owns their defaults, so this constructor applies none of its own.
    pub fn new(enabled: bool, metrics: Vec<String>) -> Self {
        Self { enabled, metrics }
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
    use super::*;

    #[test]
    fn branch_is_not_requested_when_disabled() {
        let config = AutoscalingFailoverConfiguration::new(false, vec!["custom.metric".to_string()]);

        assert!(!config.is_branch_requested());
        assert_eq!(config.metrics(), ["custom.metric".to_string()]);
    }

    #[test]
    fn branch_is_requested_when_enabled_with_non_empty_metrics() {
        let config = AutoscalingFailoverConfiguration::new(true, vec!["custom.metric".to_string()]);

        assert!(config.is_branch_requested());
        assert_eq!(config.metrics(), ["custom.metric".to_string()]);
    }

    #[test]
    fn empty_metric_allowlist_disables_branch() {
        let config = AutoscalingFailoverConfiguration::new(true, Vec::new());

        assert!(!config.is_branch_requested());
        assert!(config.metrics().is_empty());
    }
}

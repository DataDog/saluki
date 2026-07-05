//! Autoscaling failover configuration.

use agent_data_plane_config::shared::AutoscalingFailover;
use saluki_error::GenericError;

/// Autoscaling failover configuration for the metrics pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoscalingFailoverConfiguration {
    enabled: bool,
    metrics: Vec<String>,
}

impl AutoscalingFailoverConfiguration {
    /// Builds an `AutoscalingFailoverConfiguration` from the translated failover settings.
    pub fn from_configuration(config: &AutoscalingFailover) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: config.enabled,
            metrics: config.metrics.clone(),
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

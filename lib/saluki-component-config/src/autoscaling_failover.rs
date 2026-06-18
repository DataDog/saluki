//! Component-native configuration for autoscaling failover.

/// Configuration for the autoscaling failover metrics gateway.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct AutoscalingFailoverConfig {
    /// Whether the autoscaling failover branch is enabled.
    pub enabled: bool,

    /// Allowlist of metric names to forward to the Cluster Agent for autoscaling.
    pub metrics: Vec<String>,
}

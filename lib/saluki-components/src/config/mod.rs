//! Datadog-specific configuration providers.

pub mod autoscaling_failover;
pub mod cluster_agent;
pub mod metric_mirroring;
pub mod mrf;

pub use self::autoscaling_failover::AutoscalingFailoverConfiguration;
pub use self::cluster_agent::ClusterAgentConfiguration;
pub use self::metric_mirroring::MetricMirroringConfiguration;
pub use self::mrf::MrfConfiguration;

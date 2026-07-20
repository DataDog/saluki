//! Datadog-specific configuration providers.

pub mod autoscaling_failover;
pub mod cluster_agent;
pub mod mrf;

// `KEY_ALIASES` and `DatadogRemapper` moved to `datadog-agent-config` so the configuration system
// can build its loader without depending on this crate. Keep the re-export while the binary
// migrates its imports to the new home.
// TODO: remove after migration to typed config.
pub use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};

pub use self::autoscaling_failover::AutoscalingFailoverConfiguration;
pub use self::cluster_agent::ClusterAgentConfiguration;
pub use self::mrf::MrfConfiguration;

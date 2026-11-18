//! Datadog-specific configuration providers and resolved settings.

pub mod agent_env;
pub mod autoscaling_failover;
pub mod cluster_agent;
pub mod metrics_endpoint_routing;
pub mod mrf;

pub use self::agent_env::AgentEnv;
pub use self::autoscaling_failover::AutoscalingFailoverConfiguration;
pub use self::cluster_agent::ClusterAgentConfiguration;
pub use self::metrics_endpoint_routing::MetricsEndpointRoutingConfiguration;
pub use self::mrf::MrfConfiguration;

//! Entry points for components to receive Remote Configuration updates.
//!
//! Subscribe to each product through a shared
//! [`RemoteConfigurationClient`](datadog_agent_remote_config::RemoteConfigurationClient), then pass the resulting
//! handle to components that use its configuration.

pub use crate::common::otlp::semantics::SemanticRegistryProvider;
pub use crate::transforms::trace_sampler::TraceSamplingSubscription;

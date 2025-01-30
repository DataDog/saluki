//! Workload provider implementations.

mod noop;
pub use self::noop::NoopWorkloadProvider;

mod remote_agent;
pub use self::remote_agent::{RemoteAgentWorkloadAPIHandler, RemoteAgentWorkloadProvider};

//! AutoDiscovery provider implementations.

mod local;
pub use self::local::LocalAutoDiscoveryProvider;

mod remote_agent;
pub use self::remote_agent::RemoteAgentAutoDiscoveryProvider;

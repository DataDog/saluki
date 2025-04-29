//! Autodiscovery provider implementations.

mod boxed;
pub use self::boxed::BoxedAutodiscoveryProvider;

mod local;
pub use self::local::LocalAutoDiscoveryProvider;

mod remote_agent;
pub use self::remote_agent::RemoteAgentAutoDiscoveryProvider;

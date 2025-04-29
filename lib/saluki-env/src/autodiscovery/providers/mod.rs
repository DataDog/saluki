//! Autodiscovery provider implementations.

/// The capacity of the autodiscovery stream.
pub const AD_STREAM_CAPACITY: usize = 100;

mod boxed;
pub use self::boxed::BoxedAutodiscoveryProvider;

mod local;
pub use self::local::LocalAutoDiscoveryProvider;

mod remote_agent;
pub use self::remote_agent::RemoteAgentAutoDiscoveryProvider;

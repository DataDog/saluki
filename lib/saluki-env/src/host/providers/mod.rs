//! Host provider implementations.

mod boxed;
pub use self::boxed::BoxedHostProvider;

mod fixed;
pub use self::fixed::FixedHostProvider;

mod remote_agent;
pub use self::remote_agent::RemoteAgentHostProvider;

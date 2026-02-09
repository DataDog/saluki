//! Helpers for interacting with the Datadog Agent.

mod client;
pub use self::client::RemoteAgentClient;

mod session;
pub use self::session::{SessionId, SessionIdHandle};

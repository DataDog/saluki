//! IPC primitives for interacting with the Datadog Agent.

#[cfg(feature = "full")]
pub mod client;
#[cfg(feature = "full")]
pub mod config;
#[cfg(feature = "full")]
pub mod session;
pub mod tls;

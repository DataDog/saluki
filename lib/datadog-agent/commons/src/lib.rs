//! Datadog Agent shared functionality.
//!
//! This crate is intended to be the single place for Datadog Agent-specific shared functionality, from configuration
//! handling logic to common logic and more. IF it deals with emulating a specific piece of functionality inherent to
//! the Agent, it likely exists (or belongs) here.

#![deny(missing_docs)]
#![deny(warnings)]

#[cfg(feature = "full")]
pub mod agent_version;

#[cfg(feature = "ipc-tls")]
pub mod ipc;

#[cfg(feature = "full")]
pub mod platform;

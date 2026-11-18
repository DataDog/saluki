//! Telemetry received by a `datadog-intake` server, shared by correctness and integration tests.
//!
//! This module fetches and normalizes intake data. Deciding what the data must look like is left to each test kind.

mod client;
pub use self::client::IntakeClient;

mod collected;
pub use self::collected::CollectedData;

pub mod metrics;

#[cfg(test)]
pub(crate) mod test_server;

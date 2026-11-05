//! Telemetry received by a `datadog-intake` server, shared by correctness and integration tests.
//!
//! This module fetches intake data. Deciding what the data must look like is left to each test kind.

mod collected;
pub(crate) use self::collected::CollectedData;

#[cfg(any(test, feature = "test-util"))]
pub mod test_server;

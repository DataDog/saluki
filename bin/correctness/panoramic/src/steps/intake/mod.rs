//! Telemetry received by a `datadog-intake` server, and its normalization.
//!
//! Deciding what the data must look like is left to the code that reads it.

mod collected;
pub use self::collected::CollectedData;

pub mod metrics;

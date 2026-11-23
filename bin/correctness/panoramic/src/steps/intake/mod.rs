//! Telemetry received by a `datadog-intake` server and the steps that assert on it.

mod collected;
pub use self::collected::CollectedData;

mod criteria;
pub use self::criteria::{MetricCriteria, TagMatch};

#[cfg(test)]
pub(crate) mod fixtures;

pub mod metrics;

mod sender;
pub(crate) use self::sender::IntakeFromFactory;

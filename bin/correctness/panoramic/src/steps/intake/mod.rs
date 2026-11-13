//! Telemetry received by a `datadog-intake` server, its normalization, and the criteria a metric is matched against.

mod collected;
pub use self::collected::CollectedData;

mod criteria;
pub use self::criteria::{MetricCriteria, TagMatch};

#[cfg(test)]
pub(crate) mod fixtures;

pub mod metrics;

//! Telemetry received by a `datadog-intake` server, its normalization, the criteria a metric is matched against, and
//! the attribution of requests to their sender.

mod collected;
pub use self::collected::CollectedData;

mod criteria;
pub use self::criteria::{MetricCriteria, TagMatch};

#[cfg(test)]
pub(crate) mod fixtures;

pub mod metrics;

mod sender;
pub(crate) use self::sender::IntakeFromFactory;

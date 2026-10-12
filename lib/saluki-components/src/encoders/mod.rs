//! Encoder implementations.

mod buffered_incremental;
pub use self::buffered_incremental::BufferedIncrementalConfiguration;

mod datadog;

/// The multiplexed metrics encode host: one encoder component per delivery host, running one lease per output.
pub mod encode_host {
    pub use super::datadog::metrics::host::*;
}

pub use self::datadog::{
    DatadogApmStatsEncoderConfiguration, DatadogEventsConfiguration, DatadogLogsConfiguration,
    DatadogMetricsConfiguration, DatadogServiceChecksConfiguration, DatadogTraceConfiguration, MetricsEndpointRouting,
};

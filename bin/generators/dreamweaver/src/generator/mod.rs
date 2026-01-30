//! Telemetry generators.

mod log;
mod metric;
mod telemetry;
mod trace;

pub use self::log::LogGenerator;
pub use self::metric::MetricGenerator;
pub use self::telemetry::{ServiceTelemetry, SimulationResult, TelemetryBatch};
pub use self::trace::TraceGenerator;

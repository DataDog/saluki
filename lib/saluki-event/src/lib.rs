//! Core event type for Saluki.
#![deny(warnings)]
#![deny(missing_docs)]

use std::fmt;

use bitmask_enum::bitmask;

pub mod metric;
use self::metric::Metric;

/// Telemetry data type.
///
/// This type is a bitmask, which means different data types can be combined together. This makes `DataType` mainly
/// useful for defining the type of telemetry data that a component emits, or can handles.
#[bitmask(u8)]
#[bitmask_config(vec_debug)]
pub enum DataType {
    /// Metrics.
    Metric,

    /// Logs.
    Log,

    /// Traces.
    Trace,
}

impl Default for DataType {
    fn default() -> Self {
        Self::all_bits()
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut types = Vec::new();

        if self.contains(Self::Metric) {
            types.push("Metric");
        }

        if self.contains(Self::Log) {
            types.push("Log");
        }

        if self.contains(Self::Trace) {
            types.push("Trace");
        }

        write!(f, "{}", types.join("|"))
    }
}

/// A telemetry event.
#[derive(Clone)]
pub enum Event {
    /// A metric.
    Metric(Metric),
}

impl Event {
    /// Converts this event into `Metric`.
    ///
    /// If the underlying event is not a `Metric`, this will return `None`.
    pub fn into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
        }
    }
}

//! Core event type for Saluki.
#![deny(warnings)]
#![deny(missing_docs)]

use std::fmt;

use bitmask_enum::bitmask;

pub mod metric;
use self::metric::Metric;

pub mod eventd;
use self::eventd::EventD;

pub mod service_check;
use self::service_check::ServiceCheck;

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

    /// An event.
    EventD(EventD),

    /// A service check.
    ServiceCheck(ServiceCheck),
}

impl Event {
    /// Converts this event into `Metric`.
    ///
    /// If the underlying event is not a `Metric`, this will return `None`.
    pub fn into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
            Event::EventD(_) => None,
            Event::ServiceCheck(_) => None,
        }
    }

    /// Converts this event into `Eventd`.
    ///
    /// If the underlying event is not a `Eventd`, this will return `None`.
    pub fn into_eventd(self) -> Option<EventD> {
        match self {
            Event::EventD(eventd) => Some(eventd),
            Event::Metric(_) => None,
            Event::ServiceCheck(_) => None,
        }
    }

    /// Converts this event into `ServiceCheck`.
    ///
    /// If the underlying event is not a `Eventd`, this will return `None`.
    pub fn into_service_check(self) -> Option<ServiceCheck> {
        match self {
            Event::ServiceCheck(service_check) => Some(service_check),
            Event::Metric(_) => None,
            Event::EventD(_) => None,
        }
    }
}

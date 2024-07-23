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

    /// Datadog Events.
    EventD,

    /// Service checks.
    ServiceCheck,
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

        if self.contains(Self::EventD) {
            types.push("DatadogEvent");
        }

        if self.contains(Self::ServiceCheck) {
            types.push("ServiceCheck");
        }

        write!(f, "{}", types.join("|"))
    }
}

/// A telemetry event.
#[derive(Clone)]
pub enum Event {
    /// A metric.
    Metric(Metric),

    /// A Datadog Event.
    EventD(EventD),

    /// A service check.
    ServiceCheck(ServiceCheck),
}

impl Event {
    /// Returns the inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns a mutable reference inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_metric_mut(&mut self) -> Option<&mut Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `EventD`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn into_eventd(self) -> Option<EventD> {
        match self {
            Event::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn into_service_check(self) -> Option<ServiceCheck> {
        match self {
            Event::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }
}

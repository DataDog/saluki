//! Core event type for Saluki.
#![deny(warnings)]
#![deny(missing_docs)]

use std::fmt;

use bitmask_enum::bitmask;

pub mod log;
use self::log::Log;

pub mod metric;
use self::metric::Metric;

pub mod trace;
use self::trace::Trace;

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
    /// Logs.
    Log,

    /// Metrics.
    Metric,

    /// Traces.
    Trace,

    /// Datadog Events.
    EventD,

    /// Service checks.
    ServiceCheck,
}

impl Default for DataType {
    fn default() -> Self {
        Self::none()
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut types = Vec::new();

        if self.contains(Self::Log) {
            types.push("Log");
        }

        if self.contains(Self::Metric) {
            types.push("Metric");
        }

        if self.contains(Self::Trace) {
            types.push("Trace");
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
    /// A log.
    Log(Log),

    /// A metric.
    Metric(Metric),

    /// A trace.
    Trace(Trace),

    /// A Datadog Event.
    EventD(EventD),

    /// A service check.
    ServiceCheck(ServiceCheck),
}

impl Event {
    /// Gets the data type of this event.
    pub fn data_type(&self) -> DataType {
        match self {
            Event::Log(_) => DataType::Log,
            Event::Metric(_) => DataType::Metric,
            Event::Trace(_) => DataType::Trace,
            Event::EventD(_) => DataType::EventD,
            Event::ServiceCheck(_) => DataType::ServiceCheck,
        }
    }

    /// Returns the inner event value, if this event is a `Log`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_log(self) -> Option<Log> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `Trace`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_trace(self) -> Option<Trace> {
        match self {
            Event::Trace(trace) => Some(trace),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `EventD`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_eventd(self) -> Option<EventD> {
        match self {
            Event::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_service_check(self) -> Option<ServiceCheck> {
        match self {
            Event::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }

    /// Returns a mutable reference inner event value, if this event is a `Log`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_log_mut(&mut self) -> Option<&mut Log> {
        match self {
            Event::Log(log) => Some(log),
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

    /// Returns a mutable reference inner event value, if this event is a `Trace`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_trace_mut(&mut self) -> Option<&mut Trace> {
        match self {
            Event::Trace(trace) => Some(trace),
            _ => None,
        }
    }

    /// Returns a mutable reference inner event value, if this event is a `EventD`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_eventd_mut(&mut self) -> Option<&mut EventD> {
        match self {
            Event::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns a mutable reference inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_service_check_mut(&mut self) -> Option<&mut ServiceCheck> {
        match self {
            Event::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }

    /// Returns `true` if the event is a log.
    pub fn is_log(&self) -> bool {
        matches!(self, Event::Log(_))
    }

    /// Returns `true` if the event is a metric.
    pub fn is_metric(&self) -> bool {
        matches!(self, Event::Metric(_))
    }

    /// Returns `true` if the event is a trace.
    pub fn is_trace(&self) -> bool {
        matches!(self, Event::Trace(_))
    }

    /// Returns `true` if the event is a eventd.
    pub fn is_eventd(&self) -> bool {
        matches!(self, Event::EventD(_))
    }

    /// Returns `true` if the event is a service check.
    pub fn is_service_check(&self) -> bool {
        matches!(self, Event::ServiceCheck(_))
    }
}

//! Telemetry events.

use std::fmt;

use bitmask_enum::bitmask;

pub mod metric;
use self::metric::Metric;

pub mod eventd;
use self::eventd::EventD;

pub mod service_check;
use self::service_check::ServiceCheck;

pub mod log;
use self::log::Log;

/// Telemetry event type.
///
/// This type is a bitmask, which means different event types can be combined together. This makes `EventType` mainly
/// useful for defining the type of telemetry events that a component emits, or can handle.
#[bitmask(u8)]
#[bitmask_config(vec_debug)]
pub enum EventType {
    /// Metrics.
    Metric,

    /// Datadog Events.
    EventD,

    /// Service checks.
    ServiceCheck,

    /// Logs.
    Log,
}

impl Default for EventType {
    fn default() -> Self {
        Self::none()
    }
}

impl fmt::Display for EventType {
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

        if self.contains(Self::Log) {
            types.push("Log");
        }

        write!(f, "{}", types.join("|"))
    }
}

/// A telemetry event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A metric.
    Metric(Metric),

    /// A Datadog Event.
    EventD(EventD),

    /// A service check.
    ServiceCheck(ServiceCheck),

    /// A log.
    Log(Log),
}

impl Event {
    /// Gets the type of this event.
    pub fn event_type(&self) -> EventType {
        match self {
            Event::Metric(_) => EventType::Metric,
            Event::EventD(_) => EventType::EventD,
            Event::ServiceCheck(_) => EventType::ServiceCheck,
            Event::Log(_) => EventType::Log,
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

    /// Returns a reference inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_metric(&self) -> Option<&Metric> {
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

    #[allow(unused)]
    /// Returns `true` if the event is a metric.
    pub fn is_metric(&self) -> bool {
        matches!(self, Event::Metric(_))
    }

    /// Returns `true` if the event is a eventd.
    pub fn is_eventd(&self) -> bool {
        matches!(self, Event::EventD(_))
    }

    /// Returns `true` if the event is a service check.
    pub fn is_service_check(&self) -> bool {
        matches!(self, Event::ServiceCheck(_))
    }

    /// Returns `true` if the event is a log.
    pub fn is_log(&self) -> bool {
        matches!(self, Event::Log(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "only used to get struct sizes for core event data types"]
    fn sizes() {
        println!("Event: {} bytes", std::mem::size_of::<Event>());
        println!("Metric: {} bytes", std::mem::size_of::<Metric>());
        println!("  Context: {} bytes", std::mem::size_of::<saluki_context::Context>());
        println!(
            "  MetricValues: {} bytes",
            std::mem::size_of::<super::metric::MetricValues>()
        );
        println!(
            "  MetricMetadata: {} bytes",
            std::mem::size_of::<super::metric::MetricMetadata>()
        );
        println!("EventD: {} bytes", std::mem::size_of::<EventD>());
        println!("ServiceCheck: {} bytes", std::mem::size_of::<ServiceCheck>());
        println!("Log: {} bytes", std::mem::size_of::<Log>());
    }
}

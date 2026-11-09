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

pub mod trace;
use self::trace::Trace;

pub mod trace_stats;
use self::trace_stats::TraceStats;

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

    /// Traces.
    Trace,

    /// Trace stats.
    TraceStats,
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

        if self.contains(Self::Trace) {
            types.push("Trace");
        }

        if self.contains(Self::TraceStats) {
            types.push("TraceStats");
        }

        write!(f, "{}", types.join("|"))
    }
}

impl EventType {
    /// Returns the combined in-memory size, in bytes, of one item of each event type in this set.
    ///
    /// Event buffers store each event type in its own collection, so this is the per-item cost of a buffer that may
    /// hold every event type in this set. For a single event type, this is the size of that event type alone.
    pub fn combined_item_size(self) -> usize {
        let mut size = 0;
        if self.contains(Self::Metric) {
            size += std::mem::size_of::<Metric>();
        }
        if self.contains(Self::EventD) {
            size += std::mem::size_of::<EventD>();
        }
        if self.contains(Self::ServiceCheck) {
            size += std::mem::size_of::<ServiceCheck>();
        }
        if self.contains(Self::Log) {
            size += std::mem::size_of::<Log>();
        }
        if self.contains(Self::Trace) {
            size += std::mem::size_of::<Trace>();
        }
        if self.contains(Self::TraceStats) {
            size += std::mem::size_of::<TraceStats>();
        }
        size
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

    /// A trace.
    Trace(Trace),

    /// Trace stats.
    TraceStats(TraceStats),
}

impl Event {
    /// Gets the type of this event.
    pub fn event_type(&self) -> EventType {
        match self {
            Event::Metric(_) => EventType::Metric,
            Event::EventD(_) => EventType::EventD,
            Event::ServiceCheck(_) => EventType::ServiceCheck,
            Event::Log(_) => EventType::Log,
            Event::Trace(_) => EventType::Trace,
            Event::TraceStats(_) => EventType::TraceStats,
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

    /// Returns a mutable reference to the inner event value, if this event is an `EventD`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_eventd_mut(&mut self) -> Option<&mut EventD> {
        match self {
            Event::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns the inner event value, if this event is an `EventD`.
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

    /// Returns a mutable reference to the inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_service_check_mut(&mut self) -> Option<&mut ServiceCheck> {
        match self {
            Event::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }

    /// Returns a reference inner event value, if this event is a `Trace`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_into_trace(self) -> Option<Trace> {
        match self {
            Event::Trace(trace) => Some(trace),
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

    /// Returns the inner event value, if this event is a `TraceStats`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_trace_stats(self) -> Option<TraceStats> {
        match self {
            Event::TraceStats(stats) => Some(stats),
            _ => None,
        }
    }

    #[allow(unused)]
    /// Returns `true` if the event is a `Metric`.
    pub fn is_metric(&self) -> bool {
        matches!(self, Event::Metric(_))
    }

    /// Returns `true` if the event is an `EventD`.
    pub fn is_eventd(&self) -> bool {
        matches!(self, Event::EventD(_))
    }

    /// Returns `true` if the event is a `ServiceCheck`.
    pub fn is_service_check(&self) -> bool {
        matches!(self, Event::ServiceCheck(_))
    }

    /// Returns `true` if the event is a `Log`.
    pub fn is_log(&self) -> bool {
        matches!(self, Event::Log(_))
    }

    /// Returns `true` if the event is a `Trace`.
    pub fn is_trace(&self) -> bool {
        matches!(self, Event::Trace(_))
    }
}

macro_rules! impl_event_view {
    ($name:ident) => {
        impl $name<'_> {
            /// Gets the type of this event.
            pub fn event_type(&self) -> EventType {
                match self {
                    Self::Metric(_) => EventType::Metric,
                    Self::EventD(_) => EventType::EventD,
                    Self::ServiceCheck(_) => EventType::ServiceCheck,
                    Self::Log(_) => EventType::Log,
                    Self::Trace(_) => EventType::Trace,
                    Self::TraceStats(_) => EventType::TraceStats,
                }
            }

            /// Returns `true` if the event is a `Metric`.
            pub fn is_metric(&self) -> bool {
                matches!(self, Self::Metric(_))
            }

            /// Returns `true` if the event is an `EventD`.
            pub fn is_eventd(&self) -> bool {
                matches!(self, Self::EventD(_))
            }

            /// Returns `true` if the event is a `ServiceCheck`.
            pub fn is_service_check(&self) -> bool {
                matches!(self, Self::ServiceCheck(_))
            }

            /// Returns `true` if the event is a `Log`.
            pub fn is_log(&self) -> bool {
                matches!(self, Self::Log(_))
            }

            /// Returns `true` if the event is a `Trace`.
            pub fn is_trace(&self) -> bool {
                matches!(self, Self::Trace(_))
            }

            /// Returns `true` if the event is a `TraceStats`.
            pub fn is_trace_stats(&self) -> bool {
                matches!(self, Self::TraceStats(_))
            }
        }
    };
}

/// A shared reference to an event held in an event buffer.
///
/// Event buffers store each event type in its own collection, so they can't hand out `&Event`. `EventRef` is the
/// equivalent view, borrowing the inner event value directly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventRef<'a> {
    /// A metric.
    Metric(&'a Metric),

    /// A Datadog Event.
    EventD(&'a EventD),

    /// A service check.
    ServiceCheck(&'a ServiceCheck),

    /// A log.
    Log(&'a Log),

    /// A trace.
    Trace(&'a Trace),

    /// Trace stats.
    TraceStats(&'a TraceStats),
}

impl_event_view!(EventRef);

impl<'a> EventRef<'a> {
    /// Returns a reference to the inner event value, if this event is a `Metric`.
    pub fn try_as_metric(&self) -> Option<&'a Metric> {
        match *self {
            Self::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns a reference to the inner event value, if this event is an `EventD`.
    pub fn try_as_eventd(&self) -> Option<&'a EventD> {
        match *self {
            Self::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns a reference to the inner event value, if this event is a `ServiceCheck`.
    pub fn try_as_service_check(&self) -> Option<&'a ServiceCheck> {
        match *self {
            Self::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }

    /// Returns a reference to the inner event value, if this event is a `Trace`.
    pub fn try_as_trace(&self) -> Option<&'a Trace> {
        match *self {
            Self::Trace(trace) => Some(trace),
            _ => None,
        }
    }

    /// Clones the referenced event into an owned [`Event`].
    pub fn to_owned_event(&self) -> Event {
        match *self {
            Self::Metric(metric) => Event::Metric(metric.clone()),
            Self::EventD(eventd) => Event::EventD(eventd.clone()),
            Self::ServiceCheck(service_check) => Event::ServiceCheck(service_check.clone()),
            Self::Log(log) => Event::Log(log.clone()),
            Self::Trace(trace) => Event::Trace(trace.clone()),
            Self::TraceStats(stats) => Event::TraceStats(stats.clone()),
        }
    }
}

/// A mutable reference to an event held in an event buffer.
///
/// Event buffers store each event type in its own collection, so they can't hand out `&mut Event`. `EventMut` is the
/// equivalent view, borrowing the inner event value directly.
#[derive(Debug, PartialEq)]
pub enum EventMut<'a> {
    /// A metric.
    Metric(&'a mut Metric),

    /// A Datadog Event.
    EventD(&'a mut EventD),

    /// A service check.
    ServiceCheck(&'a mut ServiceCheck),

    /// A log.
    Log(&'a mut Log),

    /// A trace.
    Trace(&'a mut Trace),

    /// Trace stats.
    TraceStats(&'a mut TraceStats),
}

impl_event_view!(EventMut);

impl EventMut<'_> {
    /// Returns a shared view of this event.
    pub fn as_event_ref(&self) -> EventRef<'_> {
        match self {
            Self::Metric(metric) => EventRef::Metric(metric),
            Self::EventD(eventd) => EventRef::EventD(eventd),
            Self::ServiceCheck(service_check) => EventRef::ServiceCheck(service_check),
            Self::Log(log) => EventRef::Log(log),
            Self::Trace(trace) => EventRef::Trace(trace),
            Self::TraceStats(stats) => EventRef::TraceStats(stats),
        }
    }

    /// Returns a reference to the inner event value, if this event is a `Metric`.
    pub fn try_as_metric(&self) -> Option<&Metric> {
        match self {
            Self::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns a mutable reference to the inner event value, if this event is a `Metric`.
    pub fn try_as_metric_mut(&mut self) -> Option<&mut Metric> {
        match self {
            Self::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Returns a mutable reference to the inner event value, if this event is an `EventD`.
    pub fn try_as_eventd_mut(&mut self) -> Option<&mut EventD> {
        match self {
            Self::EventD(eventd) => Some(eventd),
            _ => None,
        }
    }

    /// Returns a mutable reference to the inner event value, if this event is a `ServiceCheck`.
    pub fn try_as_service_check_mut(&mut self) -> Option<&mut ServiceCheck> {
        match self {
            Self::ServiceCheck(service_check) => Some(service_check),
            _ => None,
        }
    }

    /// Returns a mutable reference to the inner event value, if this event is a `Log`.
    pub fn try_as_log_mut(&mut self) -> Option<&mut Log> {
        match self {
            Self::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Returns a mutable reference to the inner event value, if this event is a `Trace`.
    pub fn try_as_trace_mut(&mut self) -> Option<&mut Trace> {
        match self {
            Self::Trace(trace) => Some(trace),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_event_types_stay_within_size_budget() {
        // `Event` and its variants are moved by value through the entire pipeline on every single data point, so
        // accidental growth (an extra field, a large inline buffer, a widened enum discriminant) is a real performance
        // regression rather than a cosmetic one.
        //
        // The budgets below are the current 64-bit layout sizes; if you intentionally change the layout of one of these
        // types, update the corresponding budget in the same change.
        macro_rules! assert_size_budget {
            ($ty:ty, $budget:expr) => {{
                let actual = std::mem::size_of::<$ty>();
                assert!(
                    actual <= $budget,
                    "{} is {} bytes, exceeding its {}-byte budget",
                    stringify!($ty),
                    actual,
                    $budget,
                );
            }};
        }

        assert_size_budget!(Event, 336);
        assert_size_budget!(Metric, 160);
        assert_size_budget!(super::metric::context::Context, 8);
        assert_size_budget!(super::metric::MetricValues, 104);
        assert_size_budget!(super::metric::MetricMetadata, 48);
        assert_size_budget!(EventD, 200);
        assert_size_budget!(ServiceCheck, 160);
        assert_size_budget!(Log, 192);
        assert_size_budget!(Trace, 336);
        assert_size_budget!(TraceStats, 24);
    }

    #[test]
    fn event_type_display_renders_each_single_flag() {
        // Note that a couple of the rendered names deliberately differ from the variant identifiers.
        assert_eq!(EventType::Metric.to_string(), "Metric");
        assert_eq!(EventType::EventD.to_string(), "DatadogEvent");
        assert_eq!(EventType::ServiceCheck.to_string(), "ServiceCheck");
        assert_eq!(EventType::Log.to_string(), "Log");
        assert_eq!(EventType::Trace.to_string(), "Trace");
        assert_eq!(EventType::TraceStats.to_string(), "TraceStats");
    }

    #[test]
    fn event_type_display_joins_combined_flags_in_declaration_order() {
        // Combined flags render in the fixed declaration order joined by `|`, regardless of how they were OR'd together.
        assert_eq!((EventType::Log | EventType::Metric).to_string(), "Metric|Log");
        assert_eq!(
            EventType::all_bits().to_string(),
            "Metric|DatadogEvent|ServiceCheck|Log|Trace|TraceStats"
        );

        // The empty bitmask (also the `Default`) renders as an empty string.
        assert_eq!(EventType::none().to_string(), "");
        assert_eq!(EventType::default().to_string(), "");
    }
}

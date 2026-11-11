//! Telemetry events.

use std::fmt;

use bitmask_enum::bitmask;

use crate::topology::interconnect::EventStorage;

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

/// A type of telemetry event, such as [`Metric`] or [`Trace`].
///
/// This lets code work with one event type generically: for example, event buffers provide typed views over just the
/// events of a given type. It's implemented for each type an [`Event`] can hold, and can't be implemented elsewhere.
pub trait EventKind: EventStorage + 'static {
    /// The event type.
    const EVENT_TYPE: EventType;

    /// Wraps this value in an [`Event`].
    fn into_event(self) -> Event;

    /// Unwraps the given event if it's of this type, or returns it unchanged if it isn't.
    // The error is the event handed back to the caller rather than an error type, so boxing it would only add an
    // allocation.
    #[allow(clippy::result_large_err)]
    fn try_from_event(event: Event) -> Result<Self, Event>;

    /// Returns a reference to the inner value of the given event, if it's of this type.
    fn try_from_event_ref(event: &Event) -> Option<&Self>;

    /// Returns a mutable reference to the inner value of the given event, if it's of this type.
    fn try_from_event_mut(event: &mut Event) -> Option<&mut Self>;
}

macro_rules! impl_event_kind {
    ($ty:ty, $variant:ident) => {
        impl EventKind for $ty {
            const EVENT_TYPE: EventType = EventType::$variant;

            fn into_event(self) -> Event {
                Event::$variant(self)
            }

            fn try_from_event(event: Event) -> Result<Self, Event> {
                match event {
                    Event::$variant(value) => Ok(value),
                    other => Err(other),
                }
            }

            fn try_from_event_ref(event: &Event) -> Option<&Self> {
                match event {
                    Event::$variant(value) => Some(value),
                    _ => None,
                }
            }

            fn try_from_event_mut(event: &mut Event) -> Option<&mut Self> {
                match event {
                    Event::$variant(value) => Some(value),
                    _ => None,
                }
            }
        }
    };
}

impl_event_kind!(Metric, Metric);
impl_event_kind!(EventD, EventD);
impl_event_kind!(ServiceCheck, ServiceCheck);
impl_event_kind!(Log, Log);
impl_event_kind!(Trace, Trace);
impl_event_kind!(TraceStats, TraceStats);

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
        Metric::try_from_event(self).ok()
    }

    /// Returns a reference inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_metric(&self) -> Option<&Metric> {
        Metric::try_from_event_ref(self)
    }

    /// Returns a mutable reference inner event value, if this event is a `Metric`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_metric_mut(&mut self) -> Option<&mut Metric> {
        Metric::try_from_event_mut(self)
    }

    /// Returns a mutable reference to the inner event value, if this event is an `EventD`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_eventd_mut(&mut self) -> Option<&mut EventD> {
        EventD::try_from_event_mut(self)
    }

    /// Returns the inner event value, if this event is an `EventD`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_eventd(self) -> Option<EventD> {
        EventD::try_from_event(self).ok()
    }

    /// Returns the inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_service_check(self) -> Option<ServiceCheck> {
        ServiceCheck::try_from_event(self).ok()
    }

    /// Returns a mutable reference to the inner event value, if this event is a `ServiceCheck`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_service_check_mut(&mut self) -> Option<&mut ServiceCheck> {
        ServiceCheck::try_from_event_mut(self)
    }

    /// Returns a reference inner event value, if this event is a `Trace`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_into_trace(self) -> Option<Trace> {
        Trace::try_from_event(self).ok()
    }

    /// Returns a mutable reference inner event value, if this event is a `Trace`.
    ///
    /// Otherwise, `None` is returned.
    pub fn try_as_trace_mut(&mut self) -> Option<&mut Trace> {
        Trace::try_from_event_mut(self)
    }

    /// Returns the inner event value, if this event is a `TraceStats`.
    ///
    /// Otherwise, `None` is returned and the original event is consumed.
    pub fn try_into_trace_stats(self) -> Option<TraceStats> {
        TraceStats::try_from_event(self).ok()
    }

    #[allow(unused)]
    /// Returns `true` if the event is a `Metric`.
    pub fn is_metric(&self) -> bool {
        Metric::try_from_event_ref(self).is_some()
    }

    /// Returns `true` if the event is an `EventD`.
    pub fn is_eventd(&self) -> bool {
        EventD::try_from_event_ref(self).is_some()
    }

    /// Returns `true` if the event is a `ServiceCheck`.
    pub fn is_service_check(&self) -> bool {
        ServiceCheck::try_from_event_ref(self).is_some()
    }

    /// Returns `true` if the event is a `Log`.
    pub fn is_log(&self) -> bool {
        Log::try_from_event_ref(self).is_some()
    }

    /// Returns `true` if the event is a `Trace`.
    pub fn is_trace(&self) -> bool {
        Trace::try_from_event_ref(self).is_some()
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
        assert_size_budget!(Log, 208);
        assert_size_budget!(Trace, 336);
        assert_size_budget!(TraceStats, 32);
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

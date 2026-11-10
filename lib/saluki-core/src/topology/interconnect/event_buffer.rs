use std::fmt;

use crate::{
    data_model::event::{
        eventd::EventD, log::Log, metric::Metric, service_check::ServiceCheck, trace::Trace, trace_stats::TraceStats,
        Event, EventKind, EventType,
    },
    topology::interconnect::{dispatcher::DispatchBuffer, Dispatchable},
};

/// A fixed-size event buffer.
///
/// Events are stored in a separate collection per event type, rather than as a single collection of [`Event`]. `Event`
/// is as large as its largest variant, so storing events by type means each event only takes up as much space as its
/// own type, and code that handles a single event type can work on that collection directly.
///
/// The buffer holds at most `N` events in total, across all event types. Each per-type collection is allocated the
/// first time an event of that type is pushed, with room for the remaining capacity of the buffer, so a buffer that
/// holds a single event type makes exactly one allocation.
///
/// Iterating over the buffer yields events grouped by type, in a fixed type order, and in insertion order within each
/// type. Relative order between events of different types isn't preserved.
#[derive(Clone)]
pub struct FixedSizeEventBuffer<const N: usize> {
    metrics: Vec<Metric>,
    eventds: Vec<EventD>,
    service_checks: Vec<ServiceCheck>,
    logs: Vec<Log>,
    traces: Vec<Trace>,
    trace_stats: Vec<TraceStats>,
}

impl<const N: usize> FixedSizeEventBuffer<N> {
    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of events in the event buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.metrics.len()
            + self.eventds.len()
            + self.service_checks.len()
            + self.logs.len()
            + self.traces.len()
            + self.trace_stats.len()
    }

    /// Returns `true` if the event buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if the event buffer has no remaining capacity.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    /// Returns the set of event types currently present in the event buffer.
    pub fn event_types(&self) -> EventType {
        let mut event_types = EventType::none();
        if !self.metrics.is_empty() {
            event_types |= EventType::Metric;
        }
        if !self.eventds.is_empty() {
            event_types |= EventType::EventD;
        }
        if !self.service_checks.is_empty() {
            event_types |= EventType::ServiceCheck;
        }
        if !self.logs.is_empty() {
            event_types |= EventType::Log;
        }
        if !self.traces.is_empty() {
            event_types |= EventType::Trace;
        }
        if !self.trace_stats.is_empty() {
            event_types |= EventType::TraceStats;
        }
        event_types
    }

    /// Returns `true` if this event buffer contains one or more events of the given event type.
    pub fn has_event_type(&self, event_type: EventType) -> bool {
        self.event_types().intersects(event_type)
    }

    /// Clears the event buffer, removing all events.
    ///
    /// Allocated capacity is retained.
    pub fn clear(&mut self) {
        self.metrics.clear();
        self.eventds.clear();
        self.service_checks.clear();
        self.logs.clear();
        self.traces.clear();
        self.trace_stats.clear();
    }

    /// Attempts to append an event to the event buffer.
    ///
    /// If the event buffer is full, `Some` is returned with the original event.
    #[inline]
    pub fn try_push(&mut self, event: Event) -> Option<Event> {
        if self.is_full() {
            return Some(event);
        }

        let remaining = N - self.len();
        match event {
            Event::Metric(metric) => push_reserving(&mut self.metrics, remaining, metric),
            Event::EventD(eventd) => push_reserving(&mut self.eventds, remaining, eventd),
            Event::ServiceCheck(service_check) => push_reserving(&mut self.service_checks, remaining, service_check),
            Event::Log(log) => push_reserving(&mut self.logs, remaining, log),
            Event::Trace(trace) => push_reserving(&mut self.traces, remaining, trace),
            Event::TraceStats(stats) => push_reserving(&mut self.trace_stats, remaining, stats),
        }
        None
    }

    /// Returns an iterator over the events of type `T` in the event buffer.
    pub fn events<T: EventKind>(&self) -> impl Iterator<Item = &T> {
        T::storage(self).iter()
    }

    /// Returns an iterator over mutable references to the events of type `T` in the event buffer.
    pub fn events_mut<T: EventKind>(&mut self) -> impl Iterator<Item = &mut T> {
        T::storage_mut(self).iter_mut()
    }

    /// Consumes the event buffer, returning an iterator over the events of type `T` it held.
    ///
    /// Events of other types are dropped.
    pub fn into_events<T: EventKind>(mut self) -> impl Iterator<Item = T> {
        std::mem::take(T::storage_mut(&mut self)).into_iter()
    }

    /// Retains only the events of type `T` for which `predicate` returns `true`.
    ///
    /// Events of other types are kept. The remaining events keep their relative order.
    pub fn retain_events<T, F>(&mut self, predicate: F)
    where
        T: EventKind,
        F: FnMut(&mut T) -> bool,
    {
        T::storage_mut(self).retain_mut(predicate);
    }

    /// Removes all events of type `T` from the event buffer, returning them in order.
    ///
    /// Events of other types are kept, in their relative order.
    pub fn take_events<T: EventKind>(&mut self) -> impl Iterator<Item = T> {
        std::mem::take(T::storage_mut(self)).into_iter()
    }

    /// Removes the events of type `T` for which `predicate` returns `true`, returning them in order.
    ///
    /// All other events are kept, in their relative order.
    pub fn extract_events<T, F>(&mut self, mut predicate: F) -> impl Iterator<Item = T>
    where
        T: EventKind,
        F: FnMut(&T) -> bool,
    {
        let extracted: Vec<T> = T::storage_mut(self).extract_if(.., |value| predicate(value)).collect();
        extracted.into_iter()
    }
}

/// Selects the collection that holds an event type in a [`FixedSizeEventBuffer`].
///
/// Every [`EventKind`] implements this trait, which lets the buffer's typed views go straight to the right
/// collection. As it can't be named outside of this crate, it also keeps `EventKind` from being implemented elsewhere.
pub trait EventStorage: Sized {
    /// Returns the collection of this event type in the given event buffer.
    fn storage<const N: usize>(buffer: &FixedSizeEventBuffer<N>) -> &Vec<Self>;

    /// Returns the collection of this event type in the given event buffer, mutably.
    fn storage_mut<const N: usize>(buffer: &mut FixedSizeEventBuffer<N>) -> &mut Vec<Self>;
}

macro_rules! impl_event_storage {
    ($ty:ty, $field:ident) => {
        impl EventStorage for $ty {
            fn storage<const N: usize>(buffer: &FixedSizeEventBuffer<N>) -> &Vec<Self> {
                &buffer.$field
            }

            fn storage_mut<const N: usize>(buffer: &mut FixedSizeEventBuffer<N>) -> &mut Vec<Self> {
                &mut buffer.$field
            }
        }
    };
}

impl_event_storage!(Metric, metrics);
impl_event_storage!(EventD, eventds);
impl_event_storage!(ServiceCheck, service_checks);
impl_event_storage!(Log, logs);
impl_event_storage!(Trace, traces);
impl_event_storage!(TraceStats, trace_stats);

/// Pushes `item` onto `items`, reserving room for `remaining` items if this is the first allocation.
#[inline]
fn push_reserving<T>(items: &mut Vec<T>, remaining: usize, item: T) {
    if items.capacity() == 0 {
        items.reserve_exact(remaining);
    }
    items.push(item);
}

impl<const N: usize> Default for FixedSizeEventBuffer<N> {
    fn default() -> Self {
        Self {
            metrics: Vec::new(),
            eventds: Vec::new(),
            service_checks: Vec::new(),
            logs: Vec::new(),
            traces: Vec::new(),
            trace_stats: Vec::new(),
        }
    }
}

impl<const N: usize> Dispatchable for FixedSizeEventBuffer<N> {
    fn item_count(&self) -> usize {
        self.len()
    }
}

impl<const N: usize> DispatchBuffer for FixedSizeEventBuffer<N> {
    type Item = Event;

    fn len(&self) -> usize {
        self.len()
    }

    fn is_full(&self) -> bool {
        self.is_full()
    }

    fn try_push(&mut self, item: Self::Item) -> Option<Self::Item> {
        self.try_push(item)
    }
}

impl<const N: usize> fmt::Debug for FixedSizeEventBuffer<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedSizeEventBuffer")
            .field("cap", &N)
            .field("len", &self.len())
            .field("event_types", &self.event_types())
            .finish()
    }
}

/// An owning iterator over the events in a [`FixedSizeEventBuffer`].
pub struct EventBufferIntoIter {
    metrics: std::vec::IntoIter<Metric>,
    eventds: std::vec::IntoIter<EventD>,
    service_checks: std::vec::IntoIter<ServiceCheck>,
    logs: std::vec::IntoIter<Log>,
    traces: std::vec::IntoIter<Trace>,
    trace_stats: std::vec::IntoIter<TraceStats>,
}

impl Iterator for EventBufferIntoIter {
    type Item = Event;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // Events are yielded type by type, so build each event directly from the first non-empty collection rather
        // than chaining iterators, which would move each event through every level of the chain.
        if let Some(metric) = self.metrics.next() {
            return Some(Event::Metric(metric));
        }
        if let Some(eventd) = self.eventds.next() {
            return Some(Event::EventD(eventd));
        }
        if let Some(service_check) = self.service_checks.next() {
            return Some(Event::ServiceCheck(service_check));
        }
        if let Some(log) = self.logs.next() {
            return Some(Event::Log(log));
        }
        if let Some(trace) = self.traces.next() {
            return Some(Event::Trace(trace));
        }
        self.trace_stats.next().map(Event::TraceStats)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.metrics.len()
            + self.eventds.len()
            + self.service_checks.len()
            + self.logs.len()
            + self.traces.len()
            + self.trace_stats.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for EventBufferIntoIter {}

impl<const N: usize> IntoIterator for FixedSizeEventBuffer<N> {
    type Item = Event;
    type IntoIter = EventBufferIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        EventBufferIntoIter {
            metrics: self.metrics.into_iter(),
            eventds: self.eventds.into_iter(),
            service_checks: self.service_checks.into_iter(),
            logs: self.logs.into_iter(),
            traces: self.traces.into_iter(),
            trace_stats: self.trace_stats.into_iter(),
        }
    }
}

/// An ergonomic wrapper over fallibly writing to fixed-size event buffers.
///
/// As `FixedSizeEventBuffer` has a fixed capacity, callers have to handle the scenario where they attempt to push an
/// event into the event buffer but the buffer has no more capacity. This generally involves having to swap it with a
/// new buffer, as well as holding the event around until they acquire the new buffer.
///
/// `EventBufferManager` provides a simple, ergonomic wrapper over a basic pattern of treating the current buffer as an
/// optional value, and handling the logic of ensuring we've a buffer to write into only when actually attempting a
/// write, rather than always holding on to one.
#[derive(Default)]
pub struct EventBufferManager<const N: usize> {
    current: Option<FixedSizeEventBuffer<N>>,
}

impl<const N: usize> EventBufferManager<N> {
    /// Attempts to push an event into the current event buffer.
    ///
    /// If the event buffer is full, it's replaced with a new event buffer before pushing the event, and `Some(buffer)`
    /// is returned containing the old event buffer. Otherwise, `None` is returned.
    pub fn try_push(&mut self, event: Event) -> Option<FixedSizeEventBuffer<N>> {
        let buffer = self.current.get_or_insert_default();

        match buffer.try_push(event) {
            Some(event) => {
                // Our current buffer is full, so replace it with a new buffer before trying to write the event
                // into it again, and return the old buffer to the caller.
                let old_buffer = std::mem::take(buffer);

                if buffer.try_push(event).is_some() {
                    panic!("New event buffer is unexpectedly full.")
                }

                Some(old_buffer)
            }

            // We were able to push the event into the current buffer, so we're done.
            None => None,
        }
    }

    /// Consumes the current event buffer, if one exists.
    pub fn consume(&mut self) -> Option<FixedSizeEventBuffer<N>> {
        self.current.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_model::event::{
        eventd::EventD,
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        Event, EventType,
    };

    /// Builds a buffer holding metrics `a` and `c`, and events `b` and `d`, interleaved in that order.
    fn mixed_buffer() -> FixedSizeEventBuffer<8> {
        let mut buffer = FixedSizeEventBuffer::<8>::default();
        for event in [
            Event::Metric(Metric::counter("a", 1.0)),
            Event::EventD(EventD::new("b", "b")),
            Event::Metric(Metric::counter("c", 1.0)),
            Event::EventD(EventD::new("d", "d")),
        ] {
            assert!(buffer.try_push(event).is_none());
        }
        buffer
    }

    fn metric_names<'a>(metrics: impl Iterator<Item = &'a Metric>) -> Vec<String> {
        metrics.map(|metric| metric.context().name().to_string()).collect()
    }

    #[test]
    fn typed_views_only_see_events_of_their_type() {
        let mut buffer = mixed_buffer();
        assert_eq!(metric_names(buffer.events::<Metric>()), ["a", "c"]);
        assert_eq!(
            buffer
                .events::<EventD>()
                .map(|eventd| eventd.title())
                .collect::<Vec<_>>(),
            ["b", "d"]
        );
        assert_eq!(buffer.events::<ServiceCheck>().count(), 0);

        for metric in buffer.events_mut::<Metric>() {
            metric.metadata_mut().set_unit("seconds");
        }
        assert!(buffer
            .events::<Metric>()
            .all(|metric| metric.metadata().unit() == Some("seconds")));
        assert_eq!(buffer.len(), 4);
    }

    #[test]
    fn into_events_drops_other_types() {
        let names: Vec<_> = mixed_buffer()
            .into_events::<Metric>()
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn retain_events_filters_only_its_type() {
        let mut buffer = mixed_buffer();
        buffer.retain_events::<Metric, _>(|metric| metric.context().name() == "c");

        assert_eq!(buffer.len(), 3);
        assert_eq!(metric_names(buffer.events::<Metric>()), ["c"]);
        assert_eq!(buffer.events::<EventD>().count(), 2);
        assert!(buffer.has_event_type(EventType::Metric));

        buffer.retain_events::<Metric, _>(|_| false);
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(buffer.has_event_type(EventType::EventD));
    }

    #[test]
    fn take_events_removes_its_type_in_order() {
        let mut buffer = mixed_buffer();
        let taken: Vec<_> = buffer
            .take_events::<EventD>()
            .map(|eventd| eventd.title().to_string())
            .collect();

        assert_eq!(taken, ["b", "d"]);
        assert_eq!(buffer.len(), 2);
        assert_eq!(metric_names(buffer.events::<Metric>()), ["a", "c"]);
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::Metric));
    }

    #[test]
    fn capacity() {
        let mut buffer = FixedSizeEventBuffer::<2>::default();
        assert_eq!(buffer.capacity(), 2);

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 43.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 44.0))).is_some());
    }

    #[test]
    fn clear() {
        // Create an empty event buffer and assert that it's empty and has no seen data types:
        let mut buffer = FixedSizeEventBuffer::<2>::default();
        assert!(buffer.is_empty());
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        // Now write a metric, and make sure that's reflected:
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(!buffer.is_empty());
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        // Finally, clear the buffer and ensure it's empty and has no seen data types:
        buffer.clear();
        assert!(buffer.is_empty());
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));
    }

    #[test]
    fn has_event_type() {
        let mut buffer = FixedSizeEventBuffer::<2>::default();
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        assert!(buffer.try_push(Event::EventD(EventD::new("title", "text"))).is_none());
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));
    }

    #[test]
    fn take_events_tracks_event_types() {
        let mut buffer = FixedSizeEventBuffer::<10>::default();
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 43.0))).is_none());
        assert!(buffer.try_push(Event::EventD(EventD::new("foo1", "bar1"))).is_none());
        assert!(buffer.try_push(Event::EventD(EventD::new("foo2", "bar2"))).is_none());
        assert!(buffer.try_push(Event::EventD(EventD::new("foo3", "bar3"))).is_none());
        assert!(buffer
            .try_push(Event::ServiceCheck(ServiceCheck::new("foo4", CheckStatus::Ok)))
            .is_none());
        assert!(buffer
            .try_push(Event::ServiceCheck(ServiceCheck::new("foo5", CheckStatus::Ok)))
            .is_none());

        assert_eq!(buffer.len(), 7);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        assert_eq!(buffer.take_events::<EventD>().count(), 3);
        assert_eq!(buffer.len(), 4);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        assert_eq!(buffer.take_events::<ServiceCheck>().count(), 2);
        assert_eq!(buffer.len(), 2);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        assert_eq!(buffer.take_events::<Metric>().count(), 2);
        assert_eq!(buffer.len(), 0);
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));
    }

    #[test]
    fn extract_events_removes_matching_events_in_order() {
        let mut buffer = mixed_buffer();
        assert!(buffer.try_push(Event::Metric(Metric::counter("e", 1.0))).is_none());

        let extracted: Vec<_> = buffer
            .extract_events::<Metric, _>(|metric| metric.context().name() != "c")
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(extracted, ["a", "e"]);
        assert_eq!(buffer.len(), 3);
        assert!(buffer.has_event_type(EventType::Metric));

        // The remaining events keep their relative order within each type.
        let remaining: Vec<_> = buffer.into_iter().collect();
        assert_eq!(
            remaining,
            [
                Event::Metric(Metric::counter("c", 1.0)),
                Event::EventD(EventD::new("b", "b")),
                Event::EventD(EventD::new("d", "d")),
            ]
        );
    }

    #[test]
    fn extract_events_tracks_event_types() {
        let mut buffer = mixed_buffer();
        assert_eq!(buffer.extract_events::<Metric, _>(|_| true).count(), 2);
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(buffer.has_event_type(EventType::EventD));

        assert_eq!(buffer.extract_events::<EventD, _>(|_| false).count(), 0);
        assert!(buffer.has_event_type(EventType::EventD));
    }

    #[test]
    fn retain_events_keeps_other_types_in_order() {
        let event1 = Event::Metric(Metric::counter("foo", 42.0));
        let event2 = Event::EventD(EventD::new("foo1", "bar1"));
        let event3 = Event::EventD(EventD::new("foo2", "bar2"));
        let event4 = Event::ServiceCheck(ServiceCheck::new("foo5", CheckStatus::Ok));

        let mut buffer = FixedSizeEventBuffer::<10>::default();
        for event in [&event1, &event2, &event3, &event4] {
            assert!(buffer.try_push(event.clone()).is_none());
        }
        assert_eq!(buffer.len(), 4);

        buffer.retain_events::<EventD, _>(|_| false);
        assert_eq!(buffer.len(), 2);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        let remaining: Vec<_> = buffer.into_iter().collect();
        assert_eq!(remaining, [event1, event4]);
    }

    #[test]
    fn allocates_each_event_type_once_with_remaining_capacity() {
        let mut buffer = FixedSizeEventBuffer::<8>::default();
        assert_eq!(buffer.metrics.capacity(), 0);
        assert_eq!(buffer.eventds.capacity(), 0);

        // The first event of a type reserves room for the rest of the buffer, so a buffer holding a single event type
        // makes exactly one allocation.
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 1.0))).is_none());
        assert_eq!(buffer.metrics.capacity(), 8);

        // A second event type only reserves what's left.
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 2.0))).is_none());
        assert!(buffer.try_push(Event::EventD(EventD::new("title", "text"))).is_none());
        assert_eq!(buffer.eventds.capacity(), 6);

        // Filling the rest of the buffer with metrics doesn't reallocate.
        for i in 0..5 {
            assert!(buffer
                .try_push(Event::Metric(Metric::counter("foo", i as f64)))
                .is_none());
        }
        assert!(buffer.is_full());
        assert_eq!(buffer.metrics.capacity(), 8);
        assert_eq!(buffer.eventds.capacity(), 6);
    }

    #[test]
    fn iterates_grouped_by_event_type_in_insertion_order() {
        let mut buffer = FixedSizeEventBuffer::<8>::default();
        let events = [
            Event::EventD(EventD::new("a", "a")),
            Event::Metric(Metric::counter("b", 1.0)),
            Event::ServiceCheck(ServiceCheck::new("c", CheckStatus::Ok)),
            Event::Metric(Metric::counter("d", 1.0)),
            Event::EventD(EventD::new("e", "e")),
        ];
        for event in events.iter().cloned() {
            assert!(buffer.try_push(event).is_none());
        }

        let expected = vec![
            events[1].clone(),
            events[3].clone(),
            events[0].clone(),
            events[4].clone(),
            events[2].clone(),
        ];
        let owned: Vec<Event> = buffer.into_iter().collect();
        assert_eq!(owned, expected);
    }

    #[test]
    fn event_buffer_manager_try_push_not_full() {
        let mut manager = EventBufferManager::<4>::default();

        let event1 = Event::Metric(Metric::counter("foo", 42.0));
        let event2 = Event::EventD(EventD::new("foo1", "bar1"));
        let event3 = Event::EventD(EventD::new("foo2", "bar2"));
        let event4 = Event::ServiceCheck(ServiceCheck::new("foo5", CheckStatus::Ok));

        // Add the four events.
        assert!(manager.try_push(event1.clone()).is_none());
        assert!(manager.try_push(event2.clone()).is_none());
        assert!(manager.try_push(event3.clone()).is_none());
        assert!(manager.try_push(event4.clone()).is_none());

        // Consume the current buffer and ensure all of the events match the expected events.
        let mut buffered_events = manager.consume().expect("manager should have buffer").into_iter();

        assert_eq!(Some(event1), buffered_events.next());
        assert_eq!(Some(event2), buffered_events.next());
        assert_eq!(Some(event3), buffered_events.next());
        assert_eq!(Some(event4), buffered_events.next());
        assert_eq!(None, buffered_events.next());
    }

    #[test]
    fn event_buffer_manager_try_push_full() {
        let mut manager = EventBufferManager::<3>::default();

        let event1 = Event::Metric(Metric::counter("foo", 42.0));
        let event2 = Event::EventD(EventD::new("foo1", "bar1"));
        let event3 = Event::EventD(EventD::new("foo2", "bar2"));
        let event4 = Event::ServiceCheck(ServiceCheck::new("foo5", CheckStatus::Ok));

        // Add the four events.
        //
        // We should only be able to add three events before getting a buffer back, since we have a capacity of three.
        assert!(manager.try_push(event1.clone()).is_none());
        assert!(manager.try_push(event2.clone()).is_none());
        assert!(manager.try_push(event3.clone()).is_none());

        let first_buffer = manager.try_push(event4.clone());
        assert!(first_buffer.is_some());

        let mut buffered_events1 = first_buffer.unwrap().into_iter();
        assert_eq!(Some(event1), buffered_events1.next());
        assert_eq!(Some(event2), buffered_events1.next());
        assert_eq!(Some(event3), buffered_events1.next());
        assert_eq!(None, buffered_events1.next());

        // Consume the buffer from the manager, which should have the fourth event.
        let mut buffered_events2 = manager
            .consume()
            .expect("manager should have second buffer")
            .into_iter();
        assert_eq!(Some(event4), buffered_events2.next());
        assert_eq!(None, buffered_events2.next());
    }

    #[test]
    #[should_panic(expected = "New event buffer is unexpectedly full")]
    fn event_buffer_manager_panics_when_fresh_buffer_cannot_hold_event() {
        // Defensive guard: on overflow, `EventBufferManager::try_push` swaps in a fresh buffer and assumes it can
        // always accept the event that overflowed the previous one. That invariant holds for every real capacity
        // (N >= 1); it can only be violated by a degenerate zero-capacity buffer, which trips the guard. This documents
        // that `EventBufferManager` requires a non-zero `N`.
        let mut manager = EventBufferManager::<0>::default();
        let _ = manager.try_push(Event::Metric(Metric::counter("foo", 1.0)));
    }
}

use std::fmt;

use crate::{
    data_model::event::{
        eventd::EventD, log::Log, metric::Metric, service_check::ServiceCheck, trace::Trace, trace_stats::TraceStats,
        Event, EventMut, EventRef, EventType,
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

    /// Returns an iterator over shared references to the events in the event buffer.
    pub fn iter(&self) -> EventBufferIter<'_> {
        EventBufferIter {
            metrics: self.metrics.iter(),
            eventds: self.eventds.iter(),
            service_checks: self.service_checks.iter(),
            logs: self.logs.iter(),
            traces: self.traces.iter(),
            trace_stats: self.trace_stats.iter(),
        }
    }

    /// Returns an iterator over mutable references to the events in the event buffer.
    pub fn iter_mut(&mut self) -> EventBufferIterMut<'_> {
        EventBufferIterMut {
            metrics: self.metrics.iter_mut(),
            eventds: self.eventds.iter_mut(),
            service_checks: self.service_checks.iter_mut(),
            logs: self.logs.iter_mut(),
            traces: self.traces.iter_mut(),
            trace_stats: self.trace_stats.iter_mut(),
        }
    }

    /// Extracts events from the event buffer given a predicate function.
    ///
    /// Events for which `predicate` returns `true` are removed from the buffer and returned. The remaining events keep
    /// their relative order.
    pub fn extract<F>(&mut self, mut predicate: F) -> impl Iterator<Item = Event>
    where
        F: FnMut(&EventRef<'_>) -> bool,
    {
        let mut removed = Self::default();
        extract_into(&mut self.metrics, &mut removed.metrics, |m| {
            predicate(&EventRef::Metric(m))
        });
        extract_into(&mut self.eventds, &mut removed.eventds, |e| {
            predicate(&EventRef::EventD(e))
        });
        extract_into(&mut self.service_checks, &mut removed.service_checks, |s| {
            predicate(&EventRef::ServiceCheck(s))
        });
        extract_into(&mut self.logs, &mut removed.logs, |l| predicate(&EventRef::Log(l)));
        extract_into(&mut self.traces, &mut removed.traces, |t| {
            predicate(&EventRef::Trace(t))
        });
        extract_into(&mut self.trace_stats, &mut removed.trace_stats, |s| {
            predicate(&EventRef::TraceStats(s))
        });
        removed.into_iter()
    }

    /// Removes events from the event buffer if they match the given predicate.
    ///
    /// The remaining events keep their relative order.
    pub fn remove_if<F>(&mut self, mut predicate: F)
    where
        F: FnMut(&mut EventMut<'_>) -> bool,
    {
        self.metrics.retain_mut(|m| !predicate(&mut EventMut::Metric(m)));
        self.eventds.retain_mut(|e| !predicate(&mut EventMut::EventD(e)));
        self.service_checks
            .retain_mut(|s| !predicate(&mut EventMut::ServiceCheck(s)));
        self.logs.retain_mut(|l| !predicate(&mut EventMut::Log(l)));
        self.traces.retain_mut(|t| !predicate(&mut EventMut::Trace(t)));
        self.trace_stats
            .retain_mut(|s| !predicate(&mut EventMut::TraceStats(s)));
    }
}

/// Pushes `item` onto `items`, reserving room for `remaining` items if this is the first allocation.
#[inline]
fn push_reserving<T>(items: &mut Vec<T>, remaining: usize, item: T) {
    if items.capacity() == 0 {
        items.reserve_exact(remaining);
    }
    items.push(item);
}

/// Moves items matching `predicate` from `items` into `removed`, preserving the order of both.
fn extract_into<T, F>(items: &mut Vec<T>, removed: &mut Vec<T>, mut predicate: F)
where
    F: FnMut(&T) -> bool,
{
    if items.is_empty() {
        return;
    }

    let mut kept = Vec::with_capacity(items.capacity());
    for item in items.drain(..) {
        if predicate(&item) {
            removed.push(item);
        } else {
            kept.push(item);
        }
    }
    *items = kept;
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

macro_rules! event_buffer_iter {
    ($(#[$doc:meta])* $name:ident<$($lt:lifetime)?>, $item:ty, $iter:ident, $wrap:ident) => {
        $(#[$doc])*
        pub struct $name<$($lt)?> {
            metrics: $iter<$($lt,)? Metric>,
            eventds: $iter<$($lt,)? EventD>,
            service_checks: $iter<$($lt,)? ServiceCheck>,
            logs: $iter<$($lt,)? Log>,
            traces: $iter<$($lt,)? Trace>,
            trace_stats: $iter<$($lt,)? TraceStats>,
        }

        impl<$($lt)?> Iterator for $name<$($lt)?> {
            type Item = $item;

            #[inline]
            fn next(&mut self) -> Option<Self::Item> {
                // Events are yielded type by type, so build each event directly from the first non-empty collection
                // rather than chaining iterators, which would move each event through every level of the chain.
                if let Some(metric) = self.metrics.next() {
                    return Some($wrap::Metric(metric));
                }
                if let Some(eventd) = self.eventds.next() {
                    return Some($wrap::EventD(eventd));
                }
                if let Some(service_check) = self.service_checks.next() {
                    return Some($wrap::ServiceCheck(service_check));
                }
                if let Some(log) = self.logs.next() {
                    return Some($wrap::Log(log));
                }
                if let Some(trace) = self.traces.next() {
                    return Some($wrap::Trace(trace));
                }
                self.trace_stats.next().map($wrap::TraceStats)
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

        impl<$($lt)?> ExactSizeIterator for $name<$($lt)?> {}
    };
}

type VecIntoIter<T> = std::vec::IntoIter<T>;
type SliceIter<'a, T> = std::slice::Iter<'a, T>;
type SliceIterMut<'a, T> = std::slice::IterMut<'a, T>;

event_buffer_iter!(
    /// An owning iterator over the events in a [`FixedSizeEventBuffer`].
    EventBufferIntoIter<>, Event, VecIntoIter, Event
);

event_buffer_iter!(
    /// An iterator over shared references to the events in a [`FixedSizeEventBuffer`].
    EventBufferIter<'a>, EventRef<'a>, SliceIter, EventRef
);

event_buffer_iter!(
    /// An iterator over mutable references to the events in a [`FixedSizeEventBuffer`].
    EventBufferIterMut<'a>, EventMut<'a>, SliceIterMut, EventMut
);

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

impl<'a, const N: usize> IntoIterator for &'a FixedSizeEventBuffer<N> {
    type Item = EventRef<'a>;
    type IntoIter = EventBufferIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, const N: usize> IntoIterator for &'a mut FixedSizeEventBuffer<N> {
    type Item = EventMut<'a>;
    type IntoIter = EventBufferIterMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
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
    use std::collections::VecDeque;

    use super::*;
    use crate::data_model::event::{
        eventd::EventD,
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        Event, EventType,
    };

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
    fn extract() {
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

        let eventd_event_buffer: VecDeque<Event> = buffer.extract(|e| e.is_eventd()).collect();
        assert_eq!(buffer.len(), 4);
        assert_eq!(eventd_event_buffer.len(), 3);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        let service_checks_event_buffer: VecDeque<Event> = buffer.extract(|e| e.is_service_check()).collect();
        assert_eq!(buffer.len(), 2);
        assert_eq!(service_checks_event_buffer.len(), 2);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        let new_buffer: VecDeque<Event> = buffer.extract(|e| e.is_metric()).collect();
        assert_eq!(buffer.len(), 0);
        assert_eq!(new_buffer.len(), 2);
        assert!(!buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));
    }

    #[test]
    fn remove_if() {
        let event1 = Event::Metric(Metric::counter("foo", 42.0));
        let event2 = Event::EventD(EventD::new("foo1", "bar1"));
        let event3 = Event::EventD(EventD::new("foo2", "bar2"));
        let event4 = Event::ServiceCheck(ServiceCheck::new("foo5", CheckStatus::Ok));

        let mut buffer = FixedSizeEventBuffer::<10>::default();

        // Add the four events.
        assert!(buffer.try_push(event1.clone()).is_none());
        assert!(buffer.try_push(event2.clone()).is_none());
        assert!(buffer.try_push(event3.clone()).is_none());
        assert!(buffer.try_push(event4.clone()).is_none());
        assert_eq!(buffer.len(), 4);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        // Remove events that are EventD.
        buffer.remove_if(|event| event.is_eventd());
        assert_eq!(buffer.len(), 2);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        // Make sure we have the expected events left.
        let mut remaining_events = buffer.into_iter().collect::<Vec<_>>();

        let remaining_event1 = remaining_events.remove(0);
        assert_eq!(remaining_event1, event1);

        let remaining_event2 = remaining_events.remove(0);
        assert_eq!(remaining_event2, event4);
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
        let by_ref: Vec<Event> = buffer.iter().map(|event| event.to_owned_event()).collect();
        assert_eq!(by_ref, expected);

        let by_mut: Vec<EventType> = buffer.iter_mut().map(|event| event.event_type()).collect();
        assert_eq!(by_mut, expected.iter().map(Event::event_type).collect::<Vec<_>>());

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

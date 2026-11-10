use std::{collections::VecDeque, fmt};

use crate::{
    data_model::event::{Event, EventKind, EventType},
    topology::interconnect::{dispatcher::DispatchBuffer, Dispatchable},
};

/// A fixed-size event buffer.
#[derive(Clone)]
pub struct FixedSizeEventBuffer<const N: usize> {
    events: VecDeque<Event>,
    seen_event_types: EventType,
}

impl<const N: usize> FixedSizeEventBuffer<N> {
    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of events in the event buffer.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` if the event buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns `true` if the event buffer has no remaining capacity.
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    /// Returns `true` if this event buffer contains one or more events of the given event type.
    pub fn has_event_type(&self, event_type: EventType) -> bool {
        self.seen_event_types.contains(event_type)
    }

    /// Clears the event buffer, removing all events and resetting the seen event types.
    pub fn clear(&mut self) {
        self.events.clear();
        self.seen_event_types = EventType::none();
    }

    /// Attempts to append an event to the back of the event buffer.
    ///
    /// If the event buffer is full, `Some` is returned with the original event.
    pub fn try_push(&mut self, event: Event) -> Option<Event> {
        if self.len() == self.capacity() {
            return Some(event);
        }

        self.seen_event_types |= event.event_type();
        self.events.push_back(event);
        None
    }

    /// Returns an iterator over the events of type `T` in the event buffer.
    pub fn events<T: EventKind>(&self) -> impl Iterator<Item = &T> {
        self.events.iter().filter_map(T::try_from_event_ref)
    }

    /// Returns an iterator over mutable references to the events of type `T` in the event buffer.
    pub fn events_mut<T: EventKind>(&mut self) -> impl Iterator<Item = &mut T> {
        self.events.iter_mut().filter_map(T::try_from_event_mut)
    }

    /// Consumes the event buffer, returning an iterator over the events of type `T` it held.
    ///
    /// Events of other types are dropped.
    pub fn into_events<T: EventKind>(self) -> impl Iterator<Item = T> {
        self.events
            .into_iter()
            .filter_map(|event| T::try_from_event(event).ok())
    }

    /// Retains only the events of type `T` for which `predicate` returns `true`.
    ///
    /// Events of other types are kept. The remaining events keep their relative order.
    pub fn retain_events<T, F>(&mut self, mut predicate: F)
    where
        T: EventKind,
        F: FnMut(&mut T) -> bool,
    {
        let mut seen_event_types = EventType::none();
        self.events.retain_mut(|event| {
            let keep = match T::try_from_event_mut(event) {
                Some(value) => predicate(value),
                None => true,
            };
            if keep {
                seen_event_types |= event.event_type();
            }
            keep
        });
        self.seen_event_types = seen_event_types;
    }

    /// Removes all events of type `T` from the event buffer, returning them in order.
    ///
    /// Events of other types are kept, in their relative order.
    pub fn take_events<T: EventKind>(&mut self) -> impl Iterator<Item = T> {
        self.extract_events(|_: &T| true)
    }

    /// Removes the events of type `T` for which `predicate` returns `true`, returning them in order.
    ///
    /// All other events are kept, in their relative order.
    pub fn extract_events<T, F>(&mut self, mut predicate: F) -> impl Iterator<Item = T>
    where
        T: EventKind,
        F: FnMut(&T) -> bool,
    {
        let mut extracted = Vec::new();
        let mut seen_event_types = EventType::none();
        // Rotate through the buffer once, popping every event from the front and pushing the ones we keep back on, so
        // the buffer's storage is reused.
        for _ in 0..self.events.len() {
            let event = self.events.pop_front().expect("buffer holds at least this many events");
            match T::try_from_event(event) {
                Ok(value) if predicate(&value) => extracted.push(value),
                Ok(value) => {
                    seen_event_types |= T::EVENT_TYPE;
                    self.events.push_back(value.into_event());
                }
                Err(event) => {
                    seen_event_types |= event.event_type();
                    self.events.push_back(event);
                }
            }
        }
        self.seen_event_types = seen_event_types;
        extracted.into_iter()
    }
}

impl<const N: usize> Default for FixedSizeEventBuffer<N> {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(N),
            seen_event_types: EventType::none(),
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
            .finish()
    }
}

impl<const N: usize> IntoIterator for FixedSizeEventBuffer<N> {
    type Item = Event;
    type IntoIter = std::collections::vec_deque::IntoIter<Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.into_iter()
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

        // The remaining events keep their relative order.
        let remaining: Vec<_> = buffer.into_iter().collect();
        assert_eq!(
            remaining,
            [
                Event::EventD(EventD::new("b", "b")),
                Event::Metric(Metric::counter("c", 1.0)),
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

use std::{collections::VecDeque, fmt};

use crate::{
    data_model::event::{Event, EventType},
    execution::BatchHeader,
    topology::interconnect::{dispatcher::DispatchBuffer, Dispatchable},
};

/// A fixed-size event buffer.
///
/// Besides its events, a buffer carries a [`BatchHeader`]. A new buffer's header is unstamped. Cloning a buffer copies
/// its header, so every copy of a fanned-out buffer carries the same stamp.
#[derive(Clone)]
pub struct FixedSizeEventBuffer<const N: usize> {
    events: VecDeque<Event>,
    seen_event_types: EventType,
    header: BatchHeader,
}

impl<const N: usize> FixedSizeEventBuffer<N> {
    /// Creates an empty event buffer that allocates room for `capacity` events up front.
    ///
    /// The buffer can still hold up to `N` events, growing as needed: `capacity` only sizes the initial allocation, and
    /// is clamped to `N`. This suits buffers sized to their contents, which would otherwise pay for `N` slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity.min(N)),
            seen_event_types: EventType::none(),
            header: BatchHeader::default(),
        }
    }

    /// Returns this buffer with the given header.
    pub fn with_header(mut self, header: BatchHeader) -> Self {
        self.header = header;
        self
    }

    /// Returns the batch header.
    pub fn header(&self) -> &BatchHeader {
        &self.header
    }

    /// Returns a mutable reference to the batch header.
    pub fn header_mut(&mut self) -> &mut BatchHeader {
        &mut self.header
    }

    /// Replaces the batch header.
    pub fn set_header(&mut self, header: BatchHeader) {
        self.header = header;
    }

    /// Takes the batch header out of the buffer, leaving it unstamped.
    pub fn take_header(&mut self) -> BatchHeader {
        std::mem::take(&mut self.header)
    }

    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of events the event buffer has storage allocated for: at least its length.
    ///
    /// This is the buffer's memory, where [`capacity`][Self::capacity] is the most events it can hold. A buffer created
    /// with [`with_capacity`][Self::with_capacity] allocates room for that many events, and grows as it fills beyond
    /// them; a default buffer allocates room for its capacity.
    pub fn allocated_capacity(&self) -> usize {
        self.events.capacity()
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

    /// Clears the event buffer, removing all events, resetting the seen event types, and resetting the header.
    ///
    /// Resetting the header means a reused buffer never keeps the generation of the batch it last carried alive.
    pub fn clear(&mut self) {
        self.events.clear();
        self.seen_event_types = EventType::none();
        self.header = BatchHeader::default();
    }

    /// Makes room for `additional` more events, up to the buffer's fixed capacity, so that pushing them allocates at
    /// most once.
    pub fn reserve(&mut self, additional: usize) {
        self.events.reserve(additional.min(N - self.len()));
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

    /// Extract events from the event buffer given a predicate function.
    ///
    /// Neither the extracted events nor the events left in the buffer keep their relative order. Use
    /// [`retain`][Self::retain] when order matters.
    pub fn extract<F>(&mut self, predicate: F) -> impl Iterator<Item = Event>
    where
        F: Fn(&Event) -> bool,
    {
        let mut indices_to_remove = Vec::new();
        let mut removed_events = VecDeque::new();
        let mut seen_event_types = EventType::none();

        for (pos, event) in self.events.iter_mut().enumerate() {
            if predicate(event) {
                indices_to_remove.push(pos);
            } else {
                seen_event_types |= event.event_type();
            }
        }

        // Remove elements from back to front to avoid index shifting issues
        for &pos in indices_to_remove.iter().rev() {
            if pos < self.events.len() {
                removed_events.push_back(self.events.swap_remove_back(pos).unwrap());
            }
        }

        // Update the seen event types to reflect what's left over.
        self.seen_event_types = seen_event_types;

        removed_events.into_iter()
    }

    /// Removes events from the event buffer if they match the given predicate.
    ///
    /// Each removed event's slot is filled with the buffer's last event, so the remaining events do not keep their
    /// relative order. Use [`retain`][Self::retain] when order matters.
    pub fn remove_if<F>(&mut self, mut predicate: F)
    where
        F: FnMut(&mut Event) -> bool,
    {
        let mut seen_event_types = EventType::none();

        let mut i = 0;
        let mut end = self.events.len();
        while i < end {
            if predicate(&mut self.events[i]) {
                self.events.swap_remove_back(i);
                end -= 1;
            } else {
                seen_event_types |= self.events[i].event_type();
                i += 1;
            }
        }

        // Update the seen event types to reflect what's left over.
        self.seen_event_types = seen_event_types;
    }

    /// Keeps only the events for which `keep` returns `true`, preserving their order.
    ///
    /// `keep` may modify each event before deciding.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&mut Event) -> bool,
    {
        let mut seen_event_types = EventType::none();
        // A `Vec` keeps an event by moving it once, where a `VecDeque` swaps it into place. Converting between the two
        // keeps the storage, and moves nothing for a buffer filled from its front, as buffers almost always are.
        let mut events = Vec::from(std::mem::take(&mut self.events));
        events.retain_mut(|event| {
            let kept = keep(event);
            if kept {
                seen_event_types |= event.event_type();
            }
            kept
        });
        self.events = VecDeque::from(events);

        // Update the seen event types to reflect what's left over.
        self.seen_event_types = seen_event_types;
    }

    /// Moves the events for which `take` returns `true` to the back of `other`, preserving the order of both the moved
    /// events and the events that stay.
    ///
    /// `take` is called once for each event, from front to back. No event is cloned, and this buffer reuses its own
    /// storage for the events that stay. An event `other` has no room for stays in this buffer. Neither buffer's
    /// header changes.
    pub fn move_into<F>(&mut self, other: &mut Self, mut take: F)
    where
        F: FnMut(&Event) -> bool,
    {
        let mut room = other.capacity() - other.len();
        // Events that stay move at most once, toward the front, into the storage this buffer already has; see
        // `retain` for the conversions.
        let mut events = Vec::from(std::mem::take(&mut self.events));
        let moved = events.extract_if(.., |event| {
            let taken = take(event) && room > 0;
            room -= usize::from(taken);
            taken
        });
        for event in moved {
            other.seen_event_types |= event.event_type();
            other.events.push_back(event);
        }

        // Update the seen event types to reflect what's left over.
        let mut seen_event_types = EventType::none();
        for event in &events {
            seen_event_types |= event.event_type();
        }
        self.seen_event_types = seen_event_types;
        self.events = VecDeque::from(events);
    }
}

impl<const N: usize> Default for FixedSizeEventBuffer<N> {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(N),
            seen_event_types: EventType::none(),
            header: BatchHeader::default(),
        }
    }
}

impl<const N: usize> Dispatchable for FixedSizeEventBuffer<N> {
    fn item_count(&self) -> usize {
        self.len()
    }

    fn allocated_items(&self) -> usize {
        self.allocated_capacity()
    }

    fn header_mut(&mut self) -> Option<&mut BatchHeader> {
        Some(&mut self.header)
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
            .field("header", &self.header)
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

impl<'a, const N: usize> IntoIterator for &'a FixedSizeEventBuffer<N> {
    type Item = &'a Event;
    type IntoIter = std::collections::vec_deque::Iter<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl<'a, const N: usize> IntoIterator for &'a mut FixedSizeEventBuffer<N> {
    type Item = &'a mut Event;
    type IntoIter = std::collections::vec_deque::IterMut<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.iter_mut()
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
    use std::{collections::VecDeque, sync::Arc};

    use super::*;
    use crate::data_model::event::{
        eventd::EventD,
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        Event, EventType,
    };
    use crate::execution::{AdmissionPointId, AdmissionTime, EntryId, GenerationId, GenerationRef, GenerationToken};

    #[test]
    fn capacity() {
        let mut buffer = FixedSizeEventBuffer::<2>::default();
        assert_eq!(buffer.capacity(), 2);

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 43.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 44.0))).is_some());
    }

    #[test]
    fn with_capacity_sizes_the_allocation_but_not_the_limit() {
        // A small initial allocation still grows up to the buffer's fixed capacity, and no further.
        let mut buffer = FixedSizeEventBuffer::<4>::with_capacity(1);
        assert_eq!(buffer.capacity(), 4);
        assert!(buffer.is_empty());
        for value in 0..4 {
            assert!(buffer
                .try_push(Event::Metric(Metric::counter("foo", value as f64)))
                .is_none());
        }
        assert!(buffer.is_full());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 4.0))).is_some());
        assert!(buffer.has_event_type(EventType::Metric));

        // The requested allocation is clamped to the fixed capacity.
        let buffer = FixedSizeEventBuffer::<4>::with_capacity(1_000_000);
        assert!(buffer.events.capacity() < 1_000_000);

        // The allocated capacity is what was allocated, where the capacity is the fixed limit.
        for (buffer, allocated) in [
            (FixedSizeEventBuffer::<4>::with_capacity(0), 0),
            (FixedSizeEventBuffer::<4>::with_capacity(2), 2),
            (FixedSizeEventBuffer::<4>::default(), 4),
        ] {
            assert_eq!(buffer.allocated_capacity(), allocated);
            assert_eq!(buffer.allocated_items(), allocated);
            assert_eq!(buffer.capacity(), 4);
        }
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

        let eventd_event_buffer: VecDeque<Event> = buffer.extract(Event::is_eventd).collect();
        assert_eq!(buffer.len(), 4);
        assert_eq!(eventd_event_buffer.len(), 3);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(buffer.has_event_type(EventType::ServiceCheck));

        let service_checks_event_buffer: VecDeque<Event> = buffer.extract(Event::is_service_check).collect();
        assert_eq!(buffer.len(), 2);
        assert_eq!(service_checks_event_buffer.len(), 2);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert!(!buffer.has_event_type(EventType::ServiceCheck));

        let new_buffer: VecDeque<Event> = buffer.extract(Event::is_metric).collect();
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

    fn stamped_header(generation: &GenerationRef) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(AdmissionTime::from_unix_secs(100)),
            admission_point: Some(AdmissionPointId::new(4)),
            epoch: 2,
            generation: Some(Arc::clone(generation)),
            entry: Some(EntryId::new(1)),
        }
    }

    fn metric(name: &'static str) -> Event {
        Event::Metric(Metric::counter(name, 1.0))
    }

    fn names<const N: usize>(buffer: &FixedSizeEventBuffer<N>) -> Vec<String> {
        buffer
            .into_iter()
            .map(|event| event.try_as_metric().unwrap().context().name().to_string())
            .collect()
    }

    #[test]
    fn new_buffers_are_unstamped() {
        assert!(!FixedSizeEventBuffer::<4>::default().header().is_stamped());
        assert!(!FixedSizeEventBuffer::<4>::with_capacity(1).header().is_stamped());

        let mut manager = EventBufferManager::<4>::default();
        assert!(manager.try_push(metric("a")).is_none());
        let buffer = manager.consume().expect("manager should have a buffer");
        assert!(!buffer.header().is_stamped());
    }

    #[test]
    fn the_header_can_be_set_changed_and_taken() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(3)));

        let buffer = FixedSizeEventBuffer::<4>::with_capacity(0).with_header(stamped_header(&generation));
        assert_eq!(buffer.header().admitted_at, Some(AdmissionTime::from_unix_secs(100)));
        assert_eq!(buffer.header().generation_id(), Some(GenerationId::new(3)));

        let mut buffer = FixedSizeEventBuffer::<4>::default();
        buffer.set_header(stamped_header(&generation));
        buffer.header_mut().entry = None;
        assert_eq!(buffer.header().entry, None);
        assert_eq!(buffer.header().epoch, 2);

        // Taking the header leaves the buffer unstamped.
        let taken = buffer.take_header();
        assert_eq!(taken.admission_point, Some(AdmissionPointId::new(4)));
        assert!(!buffer.header().is_stamped());
        assert_eq!(buffer.header().generation_id(), None);

        // The dispatch path reaches the same header.
        let mut buffer = FixedSizeEventBuffer::<4>::default();
        Dispatchable::header_mut(&mut buffer)
            .expect("event buffers carry a header")
            .admitted_at = Some(AdmissionTime::from_unix_secs(7));
        assert_eq!(buffer.header().admitted_at, Some(AdmissionTime::from_unix_secs(7)));
    }

    #[test]
    fn clones_carry_the_same_header() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(5)));
        let mut buffer = FixedSizeEventBuffer::<4>::default().with_header(stamped_header(&generation));
        assert!(buffer.try_push(metric("a")).is_none());

        let copy = buffer.clone();
        assert_eq!(copy.header().admitted_at, buffer.header().admitted_at);
        assert_eq!(copy.header().admission_point, buffer.header().admission_point);
        assert_eq!(copy.header().epoch, buffer.header().epoch);
        assert_eq!(copy.header().entry, buffer.header().entry);
        assert_eq!(copy.header().generation_id(), Some(GenerationId::new(5)));

        // The copies share the generation, so each keeps it alive.
        assert_eq!(Arc::strong_count(&generation), 3);
    }

    #[test]
    fn clearing_a_buffer_unstamps_it_and_releases_its_generation() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(8)));
        let mut buffer = FixedSizeEventBuffer::<4>::default().with_header(stamped_header(&generation));
        assert!(buffer.try_push(metric("a")).is_none());
        assert_eq!(Arc::strong_count(&generation), 2);

        buffer.clear();
        assert!(buffer.is_empty());
        assert!(!buffer.header().is_stamped());
        assert_eq!(buffer.header().entry, None);
        assert_eq!(Arc::strong_count(&generation), 1);
    }

    #[test]
    fn retain_keeps_survivors_in_order() {
        let mut buffer = FixedSizeEventBuffer::<10>::default();
        for name in ["a", "drop_b", "c", "drop_d", "e", "f"] {
            assert!(buffer.try_push(metric(name)).is_none());
        }
        assert!(buffer
            .try_push(Event::EventD(EventD::new("drop_event", "text")))
            .is_none());

        buffer.retain(|event| match event {
            Event::Metric(metric) => !metric.context().name().starts_with("drop_"),
            _ => false,
        });
        assert_eq!(names(&buffer), vec!["a", "c", "e", "f"]);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));

        // For comparison, `remove_if` moves the last survivor into each removed slot.
        let mut buffer = FixedSizeEventBuffer::<10>::default();
        for name in ["a", "drop_b", "c", "drop_d", "e", "f"] {
            assert!(buffer.try_push(metric(name)).is_none());
        }
        buffer.remove_if(|event| event.try_as_metric().unwrap().context().name().starts_with("drop_"));
        assert_eq!(names(&buffer), vec!["a", "f", "c", "e"]);
    }

    #[test]
    fn retain_can_modify_the_events_it_keeps_and_leaves_the_header() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(2)));
        let mut buffer = FixedSizeEventBuffer::<10>::default().with_header(stamped_header(&generation));
        for name in ["a", "b", "c"] {
            assert!(buffer.try_push(metric(name)).is_none());
        }

        buffer.retain(|event| {
            let metric = event.try_as_metric_mut().unwrap();
            if metric.context().name() == "b" {
                return false;
            }
            *metric.context_mut() = metric.context().with_name(format!("{}_kept", metric.context().name()));
            true
        });
        assert_eq!(names(&buffer), vec!["a_kept", "c_kept"]);
        assert_eq!(buffer.header().generation_id(), Some(GenerationId::new(2)));

        // Retaining nothing empties the buffer, including its seen event types.
        buffer.retain(|_| false);
        assert!(buffer.is_empty());
        assert!(!buffer.has_event_type(EventType::Metric));
    }

    #[test]
    fn move_into_moves_matching_events_keeping_the_order_of_both_buffers() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(3)));
        let mut buffer = FixedSizeEventBuffer::<10>::default().with_header(stamped_header(&generation));
        for name in ["a", "move_b", "c", "move_d", "e"] {
            assert!(buffer.try_push(metric(name)).is_none());
        }
        assert!(buffer
            .try_push(Event::EventD(EventD::new("move_event", "text")))
            .is_none());
        let mut other = FixedSizeEventBuffer::<10>::with_capacity(3);
        assert!(other.try_push(metric("already_there")).is_none());

        let mut calls = Vec::new();
        buffer.move_into(&mut other, |event| {
            let name = match event {
                Event::Metric(metric) => metric.context().name().to_string(),
                _ => "event".to_string(),
            };
            calls.push(name.clone());
            name.starts_with("move_") || name == "event"
        });

        // `take` sees every event once, front to back.
        assert_eq!(calls, vec!["a", "move_b", "c", "move_d", "e", "event"]);
        assert_eq!(names(&buffer), vec!["a", "c", "e"]);
        assert!(buffer.has_event_type(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
        assert_eq!(buffer.len(), 3);
        // Moved events follow what `other` already held, in their original order.
        assert_eq!(other.len(), 4);
        assert!(other.has_event_type(EventType::Metric));
        assert!(other.has_event_type(EventType::EventD));
        let moved = (&other)
            .into_iter()
            .map(|event| match event {
                Event::Metric(metric) => metric.context().name().to_string(),
                _ => "event".to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(moved, vec!["already_there", "move_b", "move_d", "event"]);
        // Headers stay where they were.
        assert_eq!(buffer.header().generation_id(), Some(GenerationId::new(3)));
        assert!(!other.header().is_stamped());
    }

    #[test]
    fn move_into_keeps_the_events_a_full_buffer_cannot_take() {
        let mut buffer = FixedSizeEventBuffer::<4>::default();
        for name in ["a", "b", "c"] {
            assert!(buffer.try_push(metric(name)).is_none());
        }
        let mut other = FixedSizeEventBuffer::<4>::default();
        for name in ["x", "y", "z"] {
            assert!(other.try_push(metric(name)).is_none());
        }

        buffer.move_into(&mut other, |_| true);
        assert_eq!(names(&other), vec!["x", "y", "z", "a"]);
        assert_eq!(names(&buffer), vec!["b", "c"]);
    }

    #[test]
    fn events_buffers_stay_within_their_size_budget() {
        // An events buffer is a deque, the seen event types, and the batch header stored inline: 40 bytes without the
        // header, 80 with it. Interconnect memory bounds count one struct per channel slot, so growth here grows every
        // topology's minimum bound.
        assert!(std::mem::size_of::<BatchHeader>() <= 40);
        assert!(std::mem::size_of::<crate::topology::EventsBuffer>() <= 80);
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

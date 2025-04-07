use std::{collections::VecDeque, fmt};

use saluki_event::{DataType, Event};

use crate::pooling::{helpers::pooled_newtype, Clearable, ObjectPool};

/// A double-ended queue implemented with a ring buffer that has a fixed capacity at creation.
struct FixedSizeVecDeque<T>(VecDeque<T>);

/// A fixed-size event buffer.
pub struct FixedSizeEventBufferInner {
    events: FixedSizeVecDeque<Event>,
    seen_data_types: DataType,
}

impl FixedSizeEventBufferInner {
    /// Creates a new fixed-size event buffer with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: FixedSizeVecDeque(VecDeque::with_capacity(capacity)),
            seen_data_types: DataType::none(),
        }
    }
}

impl Clearable for FixedSizeEventBufferInner {
    fn clear(&mut self) {
        self.events.0.clear();
        self.seen_data_types = DataType::none();
    }
}

pooled_newtype! {
    outer => FixedSizeEventBuffer,
    inner => FixedSizeEventBufferInner,
}

impl FixedSizeEventBuffer {
    /// Creates a new `FixedSizeEventBuffer` with the given capacity, for testing purposes.
    #[cfg(test)]
    pub fn for_test(capacity: usize) -> Self {
        use crate::pooling::helpers::get_pooled_object_via_builder;

        get_pooled_object_via_builder(|| FixedSizeEventBufferInner::with_capacity(capacity))
    }

    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        self.data().events.0.capacity()
    }

    /// Returns the number of events in the event buffer.
    pub fn len(&self) -> usize {
        self.data().events.0.len()
    }

    /// Returns `true` if the event buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.data().events.0.is_empty()
    }

    /// Returns `true` if the event buffer has no remaining capacity.
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    /// Returns `true` if this event buffer contains one or more events of the given data type.
    pub fn has_data_type(&self, data_type: DataType) -> bool {
        self.data().seen_data_types.contains(data_type)
    }

    /// Attempts to append an event to the back of the event buffer.
    ///
    /// If the event buffer is full, `Some` is returned with the original event.
    pub fn try_push(&mut self, event: Event) -> Option<Event> {
        if self.len() == self.capacity() {
            return Some(event);
        }

        self.data_mut().seen_data_types |= event.data_type();
        self.data_mut().events.0.push_back(event);
        None
    }

    /// Extract events from the event buffer given a predicate function.
    pub fn extract<F>(&mut self, predicate: F) -> impl Iterator<Item = Event>
    where
        F: Fn(&Event) -> bool,
    {
        let events = &mut self.data_mut().events;

        let mut indices_to_remove = Vec::new();
        let mut removed_events = VecDeque::new();

        for (pos, event) in events.0.iter_mut().enumerate() {
            if predicate(event) {
                indices_to_remove.push(pos);
            }
        }

        // Remove elements from back to front to avoid index shifting issues
        for &pos in indices_to_remove.iter().rev() {
            if pos < events.0.len() {
                removed_events.push_back(events.0.swap_remove_back(pos).unwrap());
            }
        }

        removed_events.into_iter()
    }
}

impl fmt::Debug for FixedSizeEventBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedSizeEventBuffer")
            .field("event_len", &self.len())
            .finish()
    }
}

impl IntoIterator for FixedSizeEventBuffer {
    type Item = Event;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { inner: self }
    }
}

impl<'a> IntoIterator for &'a FixedSizeEventBuffer {
    type Item = &'a Event;
    type IntoIter = std::collections::vec_deque::Iter<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data().events.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut FixedSizeEventBuffer {
    type Item = &'a mut Event;
    type IntoIter = std::collections::vec_deque::IterMut<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data_mut().events.0.iter_mut()
    }
}

pub struct IntoIter {
    inner: FixedSizeEventBuffer,
}

impl Iterator for IntoIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.data_mut().events.0.pop_front()
    }
}

/// An ergonomic wrapper over fallibly writing to event buffers backed by an object pool.
///
/// As `FixedSizeEventBuffer` has a fixed capacity, callers have to handle the scenario where they attempt to push an
/// event into the event buffer but the buffer has no more capacity. This generally involves having to swap it with a
/// new buffer, as well as holding the event around until they acquire the new buffer. As these event buffers often come
/// from an object pool, waiting for the object pool to have a buffer available _before_ sending the currently-full one
/// can lead to a deadlock condition.
///
/// `EventBufferManager` provides a simple, ergonomic wrapper over a basic pattern of treating the current buffer as an
/// optional value, and handling the logic of ensuring we have a buffer to write into only when actually attempting a
/// write, rather than always holding on to one.
pub struct EventBufferManager<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    pool: &'a O,
    current: Option<FixedSizeEventBuffer>,
}

impl<'a, O> EventBufferManager<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    /// Creates a new `EventBufferManager` with the given object pool.
    pub fn new(pool: &'a O) -> Self {
        Self { pool, current: None }
    }

    /// Attempts to push an event into the current event buffer.
    ///
    /// This method will acquire an event buffer from the object pool if necessary.
    ///
    /// If the event buffer is full, the original event and the current event buffer are returned. Otherwise, `None` is
    /// returned.
    pub async fn try_push(&mut self, event: Event) -> Option<(Event, FixedSizeEventBuffer)> {
        // Consume the event buffer if we have one, or acquire one from the pool on demand.
        let mut buffer = match self.current.take() {
            Some(current) => current,
            None => self.pool.acquire().await,
        };

        // Try writing into the event buffer.
        //
        // If we can't, we'll return it which instructs the caller to flush the buffer and then try again to push the event.
        if let Some(event) = buffer.try_push(event) {
            Some((event, buffer))
        } else {
            // Return the event buffer to the caller to allow it to continue to be used.
            self.current.replace(buffer);
            None
        }
    }

    /// Consumes the current event buffer, if one exists.
    pub fn consume(&mut self) -> Option<FixedSizeEventBuffer> {
        self.current.take()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use saluki_event::{
        eventd::EventD,
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        DataType, Event,
    };

    use super::{FixedSizeEventBuffer, FixedSizeEventBufferInner};
    use crate::pooling::{helpers::get_pooled_object_via_builder, Clearable as _};

    #[test]
    fn capacity() {
        let mut buffer =
            get_pooled_object_via_builder::<_, FixedSizeEventBuffer>(|| FixedSizeEventBufferInner::with_capacity(2));
        assert_eq!(buffer.capacity(), 2);

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 43.0))).is_none());
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 44.0))).is_some());
    }

    #[test]
    fn clear() {
        // Create an empty event buffer and assert that it's empty and has no seen data types:
        let mut buffer =
            get_pooled_object_via_builder::<_, FixedSizeEventBuffer>(|| FixedSizeEventBufferInner::with_capacity(10));
        assert!(buffer.is_empty());
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        // Now write a metric, and make sure that's reflected:
        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(!buffer.is_empty());
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        // Finally, clear the inner data -- this simulates what happens when an object is returned to the pool -- and
        // assert that the buffer is once again empty and has no seen data types:
        buffer.data_mut().clear();
        assert!(buffer.is_empty());
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));
    }

    #[test]
    fn has_data_type() {
        let mut buffer =
            get_pooled_object_via_builder::<_, FixedSizeEventBuffer>(|| FixedSizeEventBufferInner::with_capacity(10));
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        assert!(buffer.try_push(Event::EventD(EventD::new("title", "text"))).is_none());
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));
    }

    #[test]
    fn extract() {
        let mut buffer =
            get_pooled_object_via_builder::<_, FixedSizeEventBuffer>(|| FixedSizeEventBufferInner::with_capacity(10));
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

        let eventd_event_buffer: VecDeque<Event> = buffer.extract(Event::is_eventd).collect();
        assert_eq!(buffer.len(), 4);
        assert_eq!(eventd_event_buffer.len(), 3);

        let service_checks_event_buffer: VecDeque<Event> = buffer.extract(Event::is_service_check).collect();
        assert_eq!(buffer.len(), 2);
        assert_eq!(service_checks_event_buffer.len(), 2);

        let new_buffer: VecDeque<Event> = buffer.extract(Event::is_metric).collect();
        assert_eq!(buffer.len(), 0);
        assert_eq!(new_buffer.len(), 2);
    }
}

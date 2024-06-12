use std::collections::VecDeque;

use saluki_event::Event;

use crate::pooling::helpers::pooled;

pooled! {
    /// An event buffer.
    ///
    /// Event buffers are able to hold an arbitrary number of [`Event`]s within them, and are reclaimed by their
    /// originating buffer pool once dropped. This allows for efficient reuse of the event buffers as the underlying
    /// allocations to hold a large number of events can itself grow large.
    struct EventBuffer {
        events: VecDeque<Event>,
    }

    clear => |this| this.events.clear()
}

impl EventBuffer {
    /// Returns the total number of events the event buffer can hold without reallocating.
    pub fn capacity(&self) -> usize {
        self.data().events.capacity()
    }

    /// Returns `true` if the event buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.data().events.is_empty()
    }

    /// Returns the number of events in the event buffer.
    pub fn len(&self) -> usize {
        self.data().events.len()
    }

    /// Appends an event to the back of the event buffer.
    pub fn push(&mut self, event: Event) {
        self.data_mut().events.push_back(event);
    }
}

impl Extend<Event> for EventBuffer {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = Event>,
    {
        self.data_mut().events.extend(iter);
    }
}

impl IntoIterator for EventBuffer {
    type Item = Event;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { inner: self }
    }
}

impl<'a> IntoIterator for &'a EventBuffer {
    type Item = &'a Event;
    type IntoIter = std::collections::vec_deque::Iter<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data().events.iter()
    }
}

impl<'a> IntoIterator for &'a mut EventBuffer {
    type Item = &'a mut Event;
    type IntoIter = std::collections::vec_deque::IterMut<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data_mut().events.iter_mut()
    }
}

pub struct IntoIter {
    inner: EventBuffer,
}

impl Iterator for IntoIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.data_mut().events.pop_front()
    }
}

use std::{collections::VecDeque, fmt};

use saluki_event::{DataType, Event};

use crate::pooling::helpers::pooled;

pooled! {
    /// An event buffer.
    ///
    /// Event buffers are able to hold an arbitrary number of [`Event`]s within them, and are reclaimed by their
    /// originating buffer pool once dropped. This allows for efficient reuse of the event buffers as the underlying
    /// allocations to hold a large number of events can itself grow large.
    struct EventBuffer {
        events: VecDeque<Event>,
        seen_data_types: DataType,
    }

    clear => |this| { this.events.clear(); this.seen_data_types = DataType::none(); }
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

    /// Returns `true` if this event buffer contains one or more events of the given data type.
    pub fn has_data_type(&self, data_type: DataType) -> bool {
        self.data().seen_data_types.contains(data_type)
    }

    /// Appends an event to the back of the event buffer.
    pub fn push(&mut self, event: Event) {
        self.data_mut().seen_data_types |= event.data_type();
        self.data_mut().events.push_back(event);
    }

    /// Reserves capacity for at least `additional` more elements to be inserted in the event buffer.
    ///
    /// The event buffer may reserve more space to speculatively avoid frequent reallocations.
    ///
    /// ## Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    pub fn reserve(&mut self, additional: usize) {
        self.data_mut().events.reserve(additional);
    }
}

impl fmt::Debug for EventBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventBuffer")
            .field("event_len", &self.data().events.len())
            .finish()
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

#[cfg(test)]
mod tests {
    use super::EventBuffer;

    use saluki_context::Context;
    use saluki_event::{
        eventd::EventD,
        metric::{Metric, MetricMetadata, MetricValue},
        DataType, Event,
    };

    use crate::pooling::{helpers::get_pooled_object_via_default, Clearable as _};

    #[test]
    fn clear() {
        // Create an empty event buffer and assert that it's empty and has no seen data types:
        let mut buffer = get_pooled_object_via_default::<EventBuffer>();
        assert!(buffer.is_empty());
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        // Now write a metric, and make sure that's reflected:
        buffer.push(Event::Metric(Metric::from_parts(
            Context::from_static_parts("foo", &[]),
            MetricValue::Counter { value: 42.0 },
            MetricMetadata::default(),
        )));
        assert!(!buffer.is_empty());
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        // Finally, clear the inner data -- this simulates what happens when an object is returned to the pool -- and
        // assert that's once again empty and has no seen data types:
        buffer.data_mut().clear();
        assert!(buffer.is_empty());
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));
    }

    #[test]
    fn has_data_type() {
        let mut buffer = get_pooled_object_via_default::<EventBuffer>();
        assert!(!buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        buffer.push(Event::Metric(Metric::from_parts(
            Context::from_static_parts("foo", &[]),
            MetricValue::Counter { value: 42.0 },
            MetricMetadata::default(),
        )));
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(!buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));

        buffer.push(Event::EventD(EventD::new("title", "text")));
        assert!(buffer.has_data_type(DataType::Metric));
        assert!(buffer.has_data_type(DataType::EventD));
        assert!(!buffer.has_data_type(DataType::ServiceCheck));
    }
}

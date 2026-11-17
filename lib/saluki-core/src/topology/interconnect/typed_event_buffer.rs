use std::{fmt, ops::Deref};

use super::{dispatcher::DispatchBuffer, Dispatchable, FixedSizeEventBuffer, FromInterconnect};
use crate::data_model::event::EventKind;

/// A fixed-size buffer of events of a single, statically known type.
///
/// `TypedEventBuffer<T, N>` is the typed counterpart of [`FixedSizeEventBuffer<N>`]. A `FixedSizeEventBuffer` holds a
/// collection of whichever event type was pushed into it first, and a `TypedEventBuffer` holds a `Vec<T>`. Converting
/// between the two moves the collection without touching the events, so components can consume and dispatch typed
/// buffers while interconnects keep carrying `FixedSizeEventBuffer`.
///
/// Like `FixedSizeEventBuffer`, the buffer allocates room for `N` events on the first push, and never grows beyond `N`
/// events.
pub struct TypedEventBuffer<T, const N: usize> {
    items: Vec<T>,
}

impl<T, const N: usize> TypedEventBuffer<T, N> {
    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns `true` if the event buffer has no remaining capacity.
    pub fn is_full(&self) -> bool {
        self.items.len() >= N
    }

    /// Attempts to append an event to the event buffer.
    ///
    /// If the event buffer is full, `Some` is returned with the original event.
    pub fn try_push(&mut self, event: T) -> Option<T> {
        if self.is_full() {
            return Some(event);
        }

        if self.items.len() == self.items.capacity() {
            self.items.reserve_exact(N - self.items.len());
        }
        self.items.push(event);
        None
    }

    /// Retains only the events for which `predicate` returns `true`.
    ///
    /// The remaining events keep their relative order.
    pub fn retain<F>(&mut self, predicate: F)
    where
        F: FnMut(&mut T) -> bool,
    {
        self.items.retain_mut(predicate);
    }

    /// Removes all events from the event buffer, keeping its allocation.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Returns a mutable slice of the events in the event buffer.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.items
    }
}

impl<T, const N: usize> Default for TypedEventBuffer<T, N> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<T: Clone, const N: usize> Clone for TypedEventBuffer<T, N> {
    fn clone(&self) -> Self {
        // Clone with room for `N` events, so the clone can be filled up like the original.
        let mut items = Vec::with_capacity(N);
        items.extend_from_slice(&self.items);
        Self { items }
    }
}

impl<T, const N: usize> Deref for TypedEventBuffer<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T, const N: usize> IntoIterator for TypedEventBuffer<T, N> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a TypedEventBuffer<T, N> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut TypedEventBuffer<T, N> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter_mut()
    }
}

impl<T, const N: usize> fmt::Debug for TypedEventBuffer<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypedEventBuffer")
            .field("type", &std::any::type_name::<T>())
            .field("cap", &N)
            .field("len", &self.items.len())
            .finish()
    }
}

impl<T: Clone, const N: usize> Dispatchable for TypedEventBuffer<T, N> {
    fn item_count(&self) -> usize {
        self.items.len()
    }
}

impl<T: Clone, const N: usize> DispatchBuffer for TypedEventBuffer<T, N> {
    type Item = T;

    fn len(&self) -> usize {
        self.items.len()
    }

    fn is_full(&self) -> bool {
        self.is_full()
    }

    fn try_push(&mut self, item: T) -> Option<T> {
        self.try_push(item)
    }
}

/// Dispatching a typed buffer wraps its collection in a `FixedSizeEventBuffer`, without touching the events.
impl<T: EventKind, const N: usize> From<TypedEventBuffer<T, N>> for FixedSizeEventBuffer<N> {
    fn from(buffer: TypedEventBuffer<T, N>) -> Self {
        Self::from_items(buffer.items)
    }
}

/// Consuming a typed buffer unwraps the collection from a `FixedSizeEventBuffer`, without touching the events.
///
/// This fails, handing the buffer back, if the buffer holds events of another type.
impl<T: EventKind, const N: usize> FromInterconnect<FixedSizeEventBuffer<N>> for TypedEventBuffer<T, N> {
    fn from_interconnect(buffer: FixedSizeEventBuffer<N>) -> Result<Self, FixedSizeEventBuffer<N>> {
        buffer.try_into_items().map(|items| Self { items })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_model::event::{log::Log, metric::Metric, Event, EventType};

    type MetricsBuffer = TypedEventBuffer<Metric, 4>;

    fn metrics_buffer(names: &[&'static str]) -> FixedSizeEventBuffer<4> {
        let mut buffer = FixedSizeEventBuffer::default();
        for name in names {
            assert!(buffer.try_push(Event::Metric(Metric::counter(*name, 1.0))).is_none());
        }
        buffer
    }

    fn names(buffer: &MetricsBuffer) -> Vec<String> {
        buffer
            .iter()
            .map(|metric| metric.context().name().to_string())
            .collect()
    }

    #[test]
    fn holds_at_most_n_events() {
        let mut buffer = MetricsBuffer::default();
        for name in ["a", "b", "c", "d"] {
            assert!(buffer.try_push(Metric::counter(name, 1.0)).is_none());
        }

        assert!(buffer.is_full());
        assert!(buffer.try_push(Metric::counter("e", 1.0)).is_some());
        assert_eq!(names(&buffer), ["a", "b", "c", "d"]);
    }

    #[test]
    fn converts_to_and_from_an_untyped_buffer_without_moving_the_events() {
        let untyped = metrics_buffer(&["a", "b"]);
        let typed = MetricsBuffer::from_interconnect(untyped).expect("buffer holds metrics");
        let events_ptr = typed.as_ptr();
        assert_eq!(names(&typed), ["a", "b"]);

        let untyped = FixedSizeEventBuffer::from(typed);
        assert_eq!(untyped.event_type(), Some(EventType::Metric));

        let typed = MetricsBuffer::from_interconnect(untyped).expect("buffer holds metrics");
        assert_eq!(typed.as_ptr(), events_ptr);
    }

    #[test]
    fn rejects_a_buffer_of_another_type() {
        let mut untyped = FixedSizeEventBuffer::<4>::default();
        assert!(untyped.try_push(Event::Log(Log::new("hello"))).is_none());

        let untyped = MetricsBuffer::from_interconnect(untyped).expect_err("buffer holds logs");
        assert_eq!(untyped.event_type(), Some(EventType::Log));
        assert_eq!(untyped.len(), 1);
    }

    #[test]
    fn an_empty_buffer_converts_to_any_type() {
        let typed = MetricsBuffer::from_interconnect(FixedSizeEventBuffer::default()).expect("buffer is empty");
        assert!(typed.is_empty());
    }
}

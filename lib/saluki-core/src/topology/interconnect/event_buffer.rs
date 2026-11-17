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
/// An event buffer only ever holds events of a single type, stored as a collection of that type rather than as a
/// collection of [`Event`]. `Event` is as large as its largest variant, so storing events by type means each event only
/// takes up as much space as its own type, and code that handles a single event type can work on that collection
/// directly.
///
/// An empty buffer can accept an event of any type, and the first event pushed into it decides which type it holds,
/// allocating room for `N` events of that type. Once the buffer holds events, it rejects events of any other type in the
/// same way it rejects events when full. The buffer never grows beyond `N` events.
#[derive(Clone)]
pub struct FixedSizeEventBuffer<const N: usize> {
    storage: BufferStorage,
}

impl<const N: usize> FixedSizeEventBuffer<N> {
    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of events in the event buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.storage.len()
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

    /// Returns the type of the events in the event buffer, or `None` if it's empty.
    pub fn event_type(&self) -> Option<EventType> {
        if self.is_empty() {
            return None;
        }

        Some(match &self.storage {
            BufferStorage::Empty => return None,
            BufferStorage::Metrics(_) => EventType::Metric,
            BufferStorage::EventDs(_) => EventType::EventD,
            BufferStorage::ServiceChecks(_) => EventType::ServiceCheck,
            BufferStorage::Logs(_) => EventType::Log,
            BufferStorage::Traces(_) => EventType::Trace,
            BufferStorage::TraceStats(_) => EventType::TraceStats,
        })
    }

    /// Returns `true` if this event buffer contains one or more events of the given event type.
    pub fn has_event_type(&self, event_type: EventType) -> bool {
        self.event_type().is_some_and(|ty| ty.intersects(event_type))
    }

    /// Clears the event buffer, removing all events.
    ///
    /// Allocated capacity is retained, and the buffer can accept events of any type again.
    pub fn clear(&mut self) {
        self.storage.clear();
    }

    /// Attempts to append an event to the event buffer.
    ///
    /// If the event buffer is full, or already holds events of a different type, `Some` is returned with the original
    /// event.
    #[inline]
    pub fn try_push(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::Metric(metric) => self.try_push_event(metric),
            Event::EventD(eventd) => self.try_push_event(eventd),
            Event::ServiceCheck(service_check) => self.try_push_event(service_check),
            Event::Log(log) => self.try_push_event(log),
            Event::Trace(trace) => self.try_push_event(trace),
            Event::TraceStats(stats) => self.try_push_event(stats),
        }
    }

    /// The only way events are added to the buffer, which upholds its invariants: the buffer only ever holds events of
    /// a single type, and never holds, or allocates room for, more than `N` events.
    #[inline]
    fn try_push_event<T: EventKind>(&mut self, value: T) -> Option<Event> {
        // An empty buffer can accept any event type, even if it previously held a different one.
        if self.is_empty() && T::items(&self.storage).is_none() {
            self.storage = T::wrap(Vec::with_capacity(N));
        }

        match T::items_mut(&mut self.storage) {
            Some(items) if items.len() < N => {
                // Collections are allocated with room for exactly `N` events, but guard against a collection that has
                // less, so that pushing never grows it past `N`.
                if items.len() == items.capacity() {
                    items.reserve_exact(N - items.len());
                }
                items.push(value);
                None
            }
            _ => Some(value.into_event()),
        }
    }

    /// Returns an iterator over the events of type `T` in the event buffer.
    pub fn events<T: EventKind>(&self) -> impl Iterator<Item = &T> {
        T::items(&self.storage).into_iter().flatten()
    }

    /// Returns an iterator over mutable references to the events of type `T` in the event buffer.
    pub fn events_mut<T: EventKind>(&mut self) -> impl Iterator<Item = &mut T> {
        T::items_mut(&mut self.storage).into_iter().flatten()
    }

    /// Consumes the event buffer, returning an iterator over the events of type `T` it held.
    ///
    /// Events of other types are dropped.
    pub fn into_events<T: EventKind>(mut self) -> impl Iterator<Item = T> {
        self.take_events()
    }

    /// Retains only the events of type `T` for which `predicate` returns `true`.
    ///
    /// Events of other types are kept. The remaining events keep their relative order.
    pub fn retain_events<T, F>(&mut self, predicate: F)
    where
        T: EventKind,
        F: FnMut(&mut T) -> bool,
    {
        if let Some(items) = T::items_mut(&mut self.storage) {
            items.retain_mut(predicate);
        }
    }

    /// Removes all events of type `T` from the event buffer, returning them.
    ///
    /// Events of other types are kept. Event order isn't preserved.
    pub fn take_events<T: EventKind>(&mut self) -> impl Iterator<Item = T> {
        T::take(&mut self.storage).unwrap_or_default().into_iter()
    }

    /// Removes the events of type `T` for which `predicate` returns `true`, returning them.
    ///
    /// All other events are kept. Event order isn't preserved.
    pub fn extract_events<T, F>(&mut self, mut predicate: F) -> impl Iterator<Item = T>
    where
        T: EventKind,
        F: FnMut(&T) -> bool,
    {
        let extracted: Vec<T> = match T::items_mut(&mut self.storage) {
            Some(items) => items.extract_if(.., |value| predicate(value)).collect(),
            None => Vec::new(),
        };
        extracted.into_iter()
    }

    /// Creates an event buffer holding the given collection of events of type `T`.
    ///
    /// The collection is moved into the buffer as is. Callers must make sure it never holds, or has room for, more than
    /// `N` events.
    pub(super) fn from_items<T: EventKind>(items: Vec<T>) -> Self {
        Self {
            storage: T::wrap(items),
        }
    }

    /// Moves the collection of events out of the event buffer if it holds events of type `T`.
    ///
    /// An empty buffer gives an empty collection, whatever type it held before. A buffer holding events of another type
    /// is handed back unchanged.
    pub(super) fn try_into_items<T: EventKind>(mut self) -> Result<Vec<T>, Self> {
        match T::take(&mut self.storage) {
            Some(items) => Ok(items),
            None if self.is_empty() => Ok(Vec::new()),
            None => Err(self),
        }
    }
}

/// The events held by a [`FixedSizeEventBuffer`]: a collection of a single event type.
#[derive(Clone)]
pub enum BufferStorage {
    /// No events have been pushed yet, or the events were taken out.
    Empty,
    Metrics(Vec<Metric>),
    EventDs(Vec<EventD>),
    ServiceChecks(Vec<ServiceCheck>),
    Logs(Vec<Log>),
    Traces(Vec<Trace>),
    TraceStats(Vec<TraceStats>),
}

impl BufferStorage {
    fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Metrics(items) => items.len(),
            Self::EventDs(items) => items.len(),
            Self::ServiceChecks(items) => items.len(),
            Self::Logs(items) => items.len(),
            Self::Traces(items) => items.len(),
            Self::TraceStats(items) => items.len(),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Empty => {}
            Self::Metrics(items) => items.clear(),
            Self::EventDs(items) => items.clear(),
            Self::ServiceChecks(items) => items.clear(),
            Self::Logs(items) => items.clear(),
            Self::Traces(items) => items.clear(),
            Self::TraceStats(items) => items.clear(),
        }
    }
}

/// Selects the [`BufferStorage`] variant that holds an event type.
///
/// Every [`EventKind`] implements this trait, which lets the buffer's typed views go straight to the right collection.
/// As it can't be named outside of this crate, it also keeps `EventKind` from being implemented elsewhere.
pub trait EventStorage: Sized {
    /// Returns the events in the given storage, if it holds this event type.
    fn items(storage: &BufferStorage) -> Option<&Vec<Self>>;

    /// Returns the events in the given storage mutably, if it holds this event type.
    fn items_mut(storage: &mut BufferStorage) -> Option<&mut Vec<Self>>;

    /// Wraps a collection of this event type as storage.
    fn wrap(items: Vec<Self>) -> BufferStorage;

    /// Takes the events out of the given storage, leaving it empty, if it holds this event type.
    fn take(storage: &mut BufferStorage) -> Option<Vec<Self>>;
}

macro_rules! impl_event_storage {
    ($ty:ty, $variant:ident) => {
        impl EventStorage for $ty {
            fn items(storage: &BufferStorage) -> Option<&Vec<Self>> {
                match storage {
                    BufferStorage::$variant(items) => Some(items),
                    _ => None,
                }
            }

            fn items_mut(storage: &mut BufferStorage) -> Option<&mut Vec<Self>> {
                match storage {
                    BufferStorage::$variant(items) => Some(items),
                    _ => None,
                }
            }

            fn wrap(items: Vec<Self>) -> BufferStorage {
                BufferStorage::$variant(items)
            }

            fn take(storage: &mut BufferStorage) -> Option<Vec<Self>> {
                match std::mem::replace(storage, BufferStorage::Empty) {
                    BufferStorage::$variant(items) => Some(items),
                    other => {
                        *storage = other;
                        None
                    }
                }
            }
        }
    };
}

impl_event_storage!(Metric, Metrics);
impl_event_storage!(EventD, EventDs);
impl_event_storage!(ServiceCheck, ServiceChecks);
impl_event_storage!(Log, Logs);
impl_event_storage!(Trace, Traces);
impl_event_storage!(TraceStats, TraceStats);

impl<const N: usize> Default for FixedSizeEventBuffer<N> {
    fn default() -> Self {
        Self {
            storage: BufferStorage::Empty,
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
            .field("event_type", &self.event_type())
            .finish()
    }
}

/// An owning iterator over the events in a [`FixedSizeEventBuffer`].
pub struct EventBufferIntoIter {
    inner: IntoIterInner,
}

enum IntoIterInner {
    Empty,
    Metrics(std::vec::IntoIter<Metric>),
    EventDs(std::vec::IntoIter<EventD>),
    ServiceChecks(std::vec::IntoIter<ServiceCheck>),
    Logs(std::vec::IntoIter<Log>),
    Traces(std::vec::IntoIter<Trace>),
    TraceStats(std::vec::IntoIter<TraceStats>),
}

impl Iterator for EventBufferIntoIter {
    type Item = Event;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            IntoIterInner::Empty => None,
            IntoIterInner::Metrics(items) => items.next().map(Event::Metric),
            IntoIterInner::EventDs(items) => items.next().map(Event::EventD),
            IntoIterInner::ServiceChecks(items) => items.next().map(Event::ServiceCheck),
            IntoIterInner::Logs(items) => items.next().map(Event::Log),
            IntoIterInner::Traces(items) => items.next().map(Event::Trace),
            IntoIterInner::TraceStats(items) => items.next().map(Event::TraceStats),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = match &self.inner {
            IntoIterInner::Empty => 0,
            IntoIterInner::Metrics(items) => items.len(),
            IntoIterInner::EventDs(items) => items.len(),
            IntoIterInner::ServiceChecks(items) => items.len(),
            IntoIterInner::Logs(items) => items.len(),
            IntoIterInner::Traces(items) => items.len(),
            IntoIterInner::TraceStats(items) => items.len(),
        };
        (len, Some(len))
    }
}

impl ExactSizeIterator for EventBufferIntoIter {}

impl<const N: usize> IntoIterator for FixedSizeEventBuffer<N> {
    type Item = Event;
    type IntoIter = EventBufferIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        let inner = match self.storage {
            BufferStorage::Empty => IntoIterInner::Empty,
            BufferStorage::Metrics(items) => IntoIterInner::Metrics(items.into_iter()),
            BufferStorage::EventDs(items) => IntoIterInner::EventDs(items.into_iter()),
            BufferStorage::ServiceChecks(items) => IntoIterInner::ServiceChecks(items.into_iter()),
            BufferStorage::Logs(items) => IntoIterInner::Logs(items.into_iter()),
            BufferStorage::Traces(items) => IntoIterInner::Traces(items.into_iter()),
            BufferStorage::TraceStats(items) => IntoIterInner::TraceStats(items.into_iter()),
        };
        EventBufferIntoIter { inner }
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
    /// If the event buffer is full, or holds events of a different type, it's replaced with a new event buffer before
    /// pushing the event, and `Some(buffer)` is returned containing the old event buffer. Otherwise, `None` is returned.
    pub fn try_push(&mut self, event: Event) -> Option<FixedSizeEventBuffer<N>> {
        let buffer = self.current.get_or_insert_default();

        match buffer.try_push(event) {
            Some(event) => {
                // Our current buffer can't take the event, so replace it with a new buffer before trying to write the
                // event into it again, and return the old buffer to the caller.
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

    fn counter(name: &'static str) -> Event {
        Event::Metric(Metric::counter(name, 1.0))
    }

    /// Builds a buffer holding the metrics `a`, `b`, and `c`, in that order.
    fn metrics_buffer() -> FixedSizeEventBuffer<8> {
        let mut buffer = FixedSizeEventBuffer::<8>::default();
        for name in ["a", "b", "c"] {
            assert!(buffer.try_push(counter(name)).is_none());
        }
        buffer
    }

    fn metric_names<'a>(metrics: impl Iterator<Item = &'a Metric>) -> Vec<String> {
        metrics.map(|metric| metric.context().name().to_string()).collect()
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    /// Returns the capacity of the collection backing the buffer.
    fn allocated_capacity<const N: usize>(buffer: &FixedSizeEventBuffer<N>) -> usize {
        match &buffer.storage {
            BufferStorage::Empty => 0,
            BufferStorage::Metrics(items) => items.capacity(),
            BufferStorage::EventDs(items) => items.capacity(),
            BufferStorage::ServiceChecks(items) => items.capacity(),
            BufferStorage::Logs(items) => items.capacity(),
            BufferStorage::Traces(items) => items.capacity(),
            BufferStorage::TraceStats(items) => items.capacity(),
        }
    }

    #[test]
    fn typed_views_only_see_events_of_their_type() {
        let mut buffer = metrics_buffer();
        assert_eq!(metric_names(buffer.events::<Metric>()), ["a", "b", "c"]);
        assert_eq!(buffer.events::<EventD>().count(), 0);
        assert_eq!(buffer.events_mut::<ServiceCheck>().count(), 0);

        for metric in buffer.events_mut::<Metric>() {
            metric.metadata_mut().set_unit("seconds");
        }
        assert!(buffer
            .events::<Metric>()
            .all(|metric| metric.metadata().unit() == Some("seconds")));
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn into_events_yields_nothing_for_other_types() {
        assert_eq!(metrics_buffer().into_events::<EventD>().count(), 0);
        let names: Vec<_> = metrics_buffer()
            .into_events::<Metric>()
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn retain_events_filters_only_its_type() {
        let mut buffer = metrics_buffer();
        buffer.retain_events::<EventD, _>(|_| false);
        assert_eq!(buffer.len(), 3);

        buffer.retain_events::<Metric, _>(|metric| metric.context().name() != "b");
        assert_eq!(metric_names(buffer.events::<Metric>()), ["a", "c"]);
        assert!(buffer.has_event_type(EventType::Metric));

        buffer.retain_events::<Metric, _>(|_| false);
        assert!(buffer.is_empty());
        assert_eq!(buffer.event_type(), None);
    }

    #[test]
    fn take_events_empties_the_buffer() {
        let mut buffer = metrics_buffer();
        assert_eq!(buffer.take_events::<EventD>().count(), 0);
        assert_eq!(buffer.len(), 3);

        let taken: Vec<_> = buffer
            .take_events::<Metric>()
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(sorted(taken), ["a", "b", "c"]);
        assert!(buffer.is_empty());
        assert_eq!(buffer.event_type(), None);
    }

    #[test]
    fn extract_events_removes_matching_events() {
        let mut buffer = metrics_buffer();
        assert_eq!(buffer.extract_events::<EventD, _>(|_| true).count(), 0);

        let extracted: Vec<_> = buffer
            .extract_events::<Metric, _>(|metric| metric.context().name() != "b")
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(sorted(extracted), ["a", "c"]);
        assert_eq!(metric_names(buffer.events::<Metric>()), ["b"]);
        assert!(buffer.has_event_type(EventType::Metric));
    }

    #[test]
    fn rejects_events_of_a_different_type() {
        let mut buffer = metrics_buffer();
        let eventd = Event::EventD(EventD::new("title", "text"));
        assert_eq!(buffer.try_push(eventd.clone()), Some(eventd));
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.event_type(), Some(EventType::Metric));
        assert!(!buffer.has_event_type(EventType::EventD));
    }

    #[test]
    fn an_empty_buffer_accepts_any_type() {
        let mut buffer = metrics_buffer();
        buffer.clear();
        assert!(buffer
            .try_push(Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok)))
            .is_none());
        assert_eq!(buffer.event_type(), Some(EventType::ServiceCheck));

        assert_eq!(buffer.take_events::<ServiceCheck>().count(), 1);
        assert!(buffer.try_push(Event::EventD(EventD::new("title", "text"))).is_none());
        assert_eq!(buffer.event_type(), Some(EventType::EventD));
    }

    #[test]
    fn allocates_room_for_exactly_n_events_and_never_grows() {
        let mut buffer = FixedSizeEventBuffer::<8>::default();
        assert_eq!(allocated_capacity(&buffer), 0);

        assert!(buffer.try_push(counter("first")).is_none());
        assert_eq!(allocated_capacity(&buffer), 8);

        while !buffer.is_full() {
            assert!(buffer.try_push(counter("more")).is_none());
        }
        assert!(buffer.try_push(counter("overflow")).is_some());
        assert_eq!(allocated_capacity(&buffer), 8);

        // Clearing keeps the allocation, and refilling the buffer doesn't grow it.
        buffer.clear();
        while !buffer.is_full() {
            assert!(buffer.try_push(counter("again")).is_none());
        }
        assert_eq!(allocated_capacity(&buffer), 8);
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
        let mut buffer = FixedSizeEventBuffer::<2>::default();
        assert!(buffer.is_empty());
        assert_eq!(buffer.event_type(), None);

        assert!(buffer.try_push(Event::Metric(Metric::counter("foo", 42.0))).is_none());
        assert!(!buffer.is_empty());
        assert!(buffer.has_event_type(EventType::Metric));

        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.event_type(), None);
        assert!(!buffer.has_event_type(EventType::Metric));
    }

    #[test]
    fn iterates_in_insertion_order() {
        let events: Vec<Event> = metrics_buffer().into_iter().collect();
        assert_eq!(events, [counter("a"), counter("b"), counter("c")]);
        assert_eq!(FixedSizeEventBuffer::<8>::default().into_iter().count(), 0);
    }

    #[test]
    fn event_buffer_manager_try_push_not_full() {
        let mut manager = EventBufferManager::<4>::default();

        let events = [counter("a"), counter("b"), counter("c"), counter("d")];
        for event in events.iter().cloned() {
            assert!(manager.try_push(event).is_none());
        }

        let buffered: Vec<_> = manager
            .consume()
            .expect("manager should have buffer")
            .into_iter()
            .collect();
        assert_eq!(buffered, events);
    }

    #[test]
    fn event_buffer_manager_try_push_full() {
        let mut manager = EventBufferManager::<3>::default();

        // We should only be able to add three events before getting a buffer back, since we have a capacity of three.
        assert!(manager.try_push(counter("a")).is_none());
        assert!(manager.try_push(counter("b")).is_none());
        assert!(manager.try_push(counter("c")).is_none());

        let first_buffer = manager.try_push(counter("d")).expect("first buffer should be full");
        assert_eq!(
            first_buffer.into_iter().collect::<Vec<_>>(),
            [counter("a"), counter("b"), counter("c")]
        );

        let second_buffer = manager.consume().expect("manager should have second buffer");
        assert_eq!(second_buffer.into_iter().collect::<Vec<_>>(), [counter("d")]);
    }

    #[test]
    fn event_buffer_manager_swaps_buffers_when_the_event_type_changes() {
        let mut manager = EventBufferManager::<4>::default();
        assert!(manager.try_push(counter("a")).is_none());

        let eventd = Event::EventD(EventD::new("title", "text"));
        let metrics = manager
            .try_push(eventd.clone())
            .expect("a different event type needs a new buffer");
        assert_eq!(metrics.into_iter().collect::<Vec<_>>(), [counter("a")]);

        let eventds = manager.consume().expect("manager should have second buffer");
        assert_eq!(eventds.into_iter().collect::<Vec<_>>(), [eventd]);
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

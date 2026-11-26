use std::{
    any::Any,
    collections::{vec_deque, VecDeque},
    fmt,
    iter::{self, FusedIterator},
};

use crate::{
    data_model::event::{metric::Metric, Event, EventKind, EventType},
    topology::interconnect::{dispatcher::DispatchBuffer, Dispatchable},
};

#[derive(Clone)]
enum Backing {
    Metrics(VecDeque<Metric>),
    Mixed(VecDeque<Event>),
}

fn metric_ref<T: EventKind>(metric: &Metric) -> Option<&T> {
    (metric as &dyn Any).downcast_ref()
}

fn metric_mut<T: EventKind>(metric: &mut Metric) -> Option<&mut T> {
    (metric as &mut dyn Any).downcast_mut()
}

fn into_metric_kind<T: EventKind>(metric: Metric) -> Option<T> {
    // Move the metric out without constructing an `Event` or allocating a box.
    (&mut Some(metric) as &mut dyn Any).downcast_mut::<Option<T>>()?.take()
}

/// A fixed-size event buffer.
#[derive(Clone)]
pub struct FixedSizeEventBuffer<const N: usize> {
    events: Backing,
    seen_event_types: EventType,
}

impl<const N: usize> FixedSizeEventBuffer<N> {
    /// Returns the total number of events the event buffer can hold.
    pub fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of events in the event buffer.
    pub fn len(&self) -> usize {
        match &self.events {
            Backing::Metrics(metrics) => metrics.len(),
            Backing::Mixed(events) => events.len(),
        }
    }

    /// Returns `true` if the event buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        match &mut self.events {
            Backing::Metrics(metrics) => metrics.clear(),
            Backing::Mixed(events) => events.clear(),
        }
        self.seen_event_types = EventType::none();
    }

    /// Attempts to append an event to the back of the event buffer.
    ///
    /// If the event buffer is full, `Some` is returned with the original event.
    pub fn try_push(&mut self, event: Event) -> Option<Event> {
        if self.len() == self.capacity() {
            return Some(event);
        }

        match (&mut self.events, event) {
            (Backing::Metrics(metrics), Event::Metric(metric)) => {
                if metrics.capacity() == 0 {
                    *metrics = VecDeque::with_capacity(N);
                }
                metrics.push_back(metric);
                self.seen_event_types = EventType::Metric;
            }
            (Backing::Mixed(events), Event::Metric(metric)) if events.is_empty() => {
                let mut metrics = VecDeque::with_capacity(N);
                metrics.push_back(metric);
                self.events = Backing::Metrics(metrics);
                self.seen_event_types = EventType::Metric;
            }
            (Backing::Metrics(metrics), event) => {
                let mut events = VecDeque::with_capacity(N);
                events.extend(metrics.drain(..).map(Event::Metric));
                self.seen_event_types |= event.event_type();
                events.push_back(event);
                self.events = Backing::Mixed(events);
            }
            (Backing::Mixed(events), event) => {
                self.seen_event_types |= event.event_type();
                events.push_back(event);
            }
        }
        None
    }

    /// Returns an iterator over the events of type `T` in the event buffer.
    pub fn events<T: EventKind>(&self) -> impl Iterator<Item = &T> {
        let (metrics, events) = match &self.events {
            Backing::Metrics(metrics) if T::EVENT_TYPE == EventType::Metric => {
                (metrics.iter(), vec_deque::Iter::default())
            }
            Backing::Mixed(events) if self.has_event_type(T::EVENT_TYPE) => (vec_deque::Iter::default(), events.iter()),
            _ => (vec_deque::Iter::default(), vec_deque::Iter::default()),
        };
        metrics
            .filter_map(metric_ref)
            .chain(events.filter_map(T::try_from_event_ref))
    }

    /// Returns an iterator over mutable references to the events of type `T` in the event buffer.
    pub fn events_mut<T: EventKind>(&mut self) -> impl Iterator<Item = &mut T> {
        let has_event_type = self.has_event_type(T::EVENT_TYPE);
        let (metrics, events) = match &mut self.events {
            Backing::Metrics(metrics) if T::EVENT_TYPE == EventType::Metric => {
                (metrics.iter_mut(), vec_deque::IterMut::default())
            }
            Backing::Mixed(events) if has_event_type => (vec_deque::IterMut::default(), events.iter_mut()),
            _ => (vec_deque::IterMut::default(), vec_deque::IterMut::default()),
        };
        metrics
            .filter_map(metric_mut)
            .chain(events.filter_map(T::try_from_event_mut))
    }

    /// Consumes the event buffer, returning an iterator over the events of type `T` it held.
    ///
    /// Events of other types are dropped.
    pub fn into_events<T: EventKind>(mut self) -> impl Iterator<Item = T> {
        iter::from_fn(move || match &mut self.events {
            Backing::Metrics(metrics) if T::EVENT_TYPE == EventType::Metric => {
                metrics.pop_front().and_then(into_metric_kind)
            }
            Backing::Metrics(_) => None,
            Backing::Mixed(events) => {
                iter::from_fn(|| events.pop_front()).find_map(|event| T::try_from_event(event).ok())
            }
        })
    }

    /// Retains only the events of type `T` for which `predicate` returns `true`.
    ///
    /// Events of other types are kept. The remaining events keep their relative order.
    pub fn retain_events<T, F>(&mut self, mut predicate: F)
    where
        T: EventKind,
        F: FnMut(&mut T) -> bool,
    {
        if !self.has_event_type(T::EVENT_TYPE) {
            return;
        }

        let events = match &mut self.events {
            Backing::Metrics(metrics) => {
                metrics.retain_mut(|metric| metric_mut(metric).is_none_or(&mut predicate));
                if metrics.is_empty() {
                    self.seen_event_types = EventType::none();
                }
                return;
            }
            Backing::Mixed(events) => events,
        };
        let mut seen_event_types = EventType::none();
        events.retain_mut(|event| {
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

    /// Removes all events of type `T` from the event buffer, returning them.
    ///
    /// Events of other types are kept. Event order isn't preserved.
    pub fn take_events<T: EventKind>(&mut self) -> impl Iterator<Item = T> {
        self.extract_events(|_: &T| true)
    }

    /// Removes the events of type `T` for which `predicate` returns `true`, returning them.
    ///
    /// All other events are kept. Event order isn't preserved.
    pub fn extract_events<T, F>(&mut self, mut predicate: F) -> impl Iterator<Item = T>
    where
        T: EventKind,
        F: FnMut(&T) -> bool,
    {
        let mut extracted = Vec::new();
        if !self.has_event_type(T::EVENT_TYPE) {
            return extracted.into_iter();
        }
        let events = match &mut self.events {
            Backing::Metrics(metrics) => {
                let mut i = 0;
                while i < metrics.len() {
                    if metric_ref(&metrics[i]).is_some_and(&mut predicate) {
                        let metric = metrics.swap_remove_back(i).expect("index is in bounds");
                        extracted.push(into_metric_kind(metric).expect("metric type was already checked"));
                    } else {
                        i += 1;
                    }
                }
                if metrics.is_empty() {
                    self.seen_event_types = EventType::none();
                }
                return extracted.into_iter();
            }
            Backing::Mixed(events) => events,
        };

        // Check each event in place, and only move the ones we extract.
        let mut seen_event_types = EventType::none();
        let mut i = 0;
        while i < events.len() {
            if T::try_from_event_ref(&events[i]).is_some_and(&mut predicate) {
                // This moves the last event into slot `i`, so we check the same index again next.
                let event = events.swap_remove_back(i).expect("index is in bounds");
                let Ok(value) = T::try_from_event(event) else {
                    unreachable!("event was already checked to be of type `T`");
                };
                extracted.push(value);
            } else {
                seen_event_types |= events[i].event_type();
                i += 1;
            }
        }
        self.seen_event_types = seen_event_types;
        extracted.into_iter()
    }
}

impl<const N: usize> Default for FixedSizeEventBuffer<N> {
    fn default() -> Self {
        Self {
            events: Backing::Metrics(VecDeque::new()),
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
    type IntoIter = EventBufferIntoIter<N>;

    fn into_iter(self) -> Self::IntoIter {
        EventBufferIntoIter(self)
    }
}

/// An owning iterator over the events in a fixed-size event buffer.
#[derive(Clone, Debug, Default)]
pub struct EventBufferIntoIter<const N: usize>(FixedSizeEventBuffer<N>);

impl<const N: usize> Iterator for EventBufferIntoIter<N> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0.events {
            Backing::Metrics(metrics) => metrics.pop_front().map(Event::Metric),
            Backing::Mixed(events) => events.pop_front(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.0.len();
        (len, Some(len))
    }
}

impl<const N: usize> DoubleEndedIterator for EventBufferIntoIter<N> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0.events {
            Backing::Metrics(metrics) => metrics.pop_back().map(Event::Metric),
            Backing::Mixed(events) => events.pop_back(),
        }
    }
}

impl<const N: usize> ExactSizeIterator for EventBufferIntoIter<N> {}
impl<const N: usize> FusedIterator for EventBufferIntoIter<N> {}

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
        service_check::{CheckStatus, ServiceCheck},
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

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[test]
    fn metric_backing_promotes_in_order_and_restarts_after_clear() {
        let mut buffer = FixedSizeEventBuffer::<4>::default();
        let first = Event::Metric(Metric::counter("first", 1.0));
        let second = Event::Metric(Metric::counter("second", 1.0));
        let eventd = Event::EventD(EventD::new("third", "third"));

        assert!(buffer.try_push(first.clone()).is_none());
        assert!(buffer.try_push(second.clone()).is_none());
        let Backing::Metrics(metrics) = &buffer.events else {
            panic!("metric-only batch should use compact backing");
        };
        assert_eq!(metrics.capacity(), buffer.capacity());
        assert_eq!(buffer.events::<EventD>().count(), 0);

        assert!(buffer.try_push(eventd.clone()).is_none());
        assert!(matches!(&buffer.events, Backing::Mixed(_)));
        let mut iter = buffer.clone().into_iter();
        assert_eq!(iter.len(), 3);
        assert_eq!(iter.next(), Some(first.clone()));
        assert_eq!(iter.next_back(), Some(eventd));
        assert_eq!(iter.next(), Some(second));
        assert_eq!(iter.next(), None);

        buffer.clear();
        assert!(buffer.try_push(first.clone()).is_none());
        assert!(matches!(&buffer.events, Backing::Metrics(_)));
        assert_eq!(buffer.into_iter().next(), Some(first));
    }

    #[test]
    fn compact_typed_views_filter_and_drain() {
        let mut buffer = FixedSizeEventBuffer::<4>::default();
        for name in ["a", "b", "c", "d"] {
            assert!(buffer.try_push(Event::Metric(Metric::counter(name, 1.0))).is_none());
        }
        let eventd = Event::EventD(EventD::new("rejected", "rejected"));
        assert_eq!(buffer.try_push(eventd.clone()), Some(eventd));
        assert!(matches!(&buffer.events, Backing::Metrics(_)));
        for metric in buffer.events_mut::<Metric>() {
            metric.metadata_mut().set_unit("seconds");
        }
        assert!(buffer.events_mut::<EventD>().next().is_none());
        assert!(buffer
            .events::<Metric>()
            .all(|metric| metric.metadata().unit() == Some("seconds")));
        assert_eq!(buffer.take_events::<EventD>().count(), 0);

        let extracted: Vec<_> = buffer
            .extract_events::<Metric, _>(|metric| matches!(metric.context().name().as_ref(), "a" | "c"))
            .collect();
        assert_eq!(metric_names(extracted.iter()), ["a", "c"]);
        assert!(buffer.has_event_type(EventType::Metric));
        assert_eq!(metric_names(buffer.events::<Metric>()), ["d", "b"]);
        buffer.retain_events::<Metric, _>(|metric| metric.context().name() != "b");
        assert_eq!(metric_names(buffer.events::<Metric>()), ["d"]);
        assert_eq!(buffer.clone().into_events::<EventD>().count(), 0);
        assert_eq!(
            buffer.clone().into_events::<Metric>().next().unwrap().context().name(),
            "d"
        );
        assert_eq!(buffer.take_events::<Metric>().count(), 1);
        assert!(buffer.is_empty());
        assert!(!buffer.has_event_type(EventType::Metric));
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
    fn take_events_removes_its_type() {
        let mut buffer = mixed_buffer();
        let mut taken: Vec<_> = buffer
            .take_events::<EventD>()
            .map(|eventd| eventd.title().to_string())
            .collect();
        taken.sort();

        assert_eq!(taken, ["b", "d"]);
        assert_eq!(buffer.len(), 2);
        assert_eq!(sorted(metric_names(buffer.events::<Metric>())), ["a", "c"]);
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
    fn extract_events_removes_matching_events() {
        let mut buffer = mixed_buffer();
        assert!(buffer.try_push(Event::Metric(Metric::counter("e", 1.0))).is_none());

        let extracted: Vec<_> = buffer
            .extract_events::<Metric, _>(|metric| metric.context().name() != "c")
            .map(|metric| metric.context().name().to_string())
            .collect();
        assert_eq!(sorted(extracted), ["a", "e"]);
        assert_eq!(buffer.len(), 3);
        assert!(buffer.has_event_type(EventType::Metric));
        assert_eq!(metric_names(buffer.events::<Metric>()), ["c"]);

        let mut eventd_titles: Vec<_> = buffer.events::<EventD>().map(|eventd| eventd.title()).collect();
        eventd_titles.sort();
        assert_eq!(eventd_titles, ["b", "d"]);
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

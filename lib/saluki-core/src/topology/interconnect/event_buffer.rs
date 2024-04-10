use saluki_event::Event;

use crate::buffers::buffered;

buffered! {
    struct EventBuffer {
        events: Vec<Event>,
    }

    clear => |this| this.events.clear()
}

impl EventBuffer {
    pub fn capacity(&self) -> usize {
        self.data().events.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.data().events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.data().events.len()
    }

    pub fn push(&mut self, event: Event) {
        self.data_mut().events.push(event);
    }

    pub fn extend(&mut self, events: impl IntoIterator<Item = Event>) {
        self.data_mut().events.extend(events);
    }

    pub fn take_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.data_mut().events.drain(..)
    }
}

impl<'a> IntoIterator for &'a EventBuffer {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data().events.iter()
    }
}

impl<'a> IntoIterator for &'a mut EventBuffer {
    type Item = &'a mut Event;
    type IntoIter = std::slice::IterMut<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.data_mut().events.iter_mut()
    }
}

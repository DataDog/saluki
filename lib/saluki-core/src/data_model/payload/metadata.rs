use std::any::Any;

use anymap3::{CloneAny, Map};

/// Payload metadata.
///
/// Contains the event count and an extensible map of typed metadata values.
/// Components can store and retrieve arbitrary typed data using the `set` and `get` methods.
#[derive(Clone)]
pub struct PayloadMetadata {
    event_count: usize,
    extensions: Map<dyn CloneAny + Send + Sync>,
}

impl PayloadMetadata {
    /// Creates a new `PayloadMetadata` with the given event count.
    pub fn from_event_count(event_count: usize) -> Self {
        PayloadMetadata {
            event_count,
            extensions: Map::new(),
        }
    }

    /// Returns the number of events in the payload.
    pub fn event_count(&self) -> usize {
        self.event_count
    }

    /// Gets a reference to a typed extension value, if present.
    pub fn get<T: Any + Clone + Send + Sync>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// Sets a typed extension value, returning `self` for chaining.
    pub fn with<T: Any + Clone + Send + Sync>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Sets a typed extension value in place.
    pub fn set<T: Any + Clone + Send + Sync>(&mut self, value: T) {
        self.extensions.insert(value);
    }
}

/// Payload metadata.
#[derive(Clone)]
pub struct PayloadMetadata {
    event_count: usize,
}

impl PayloadMetadata {
    /// Creates a new `PayloadMetadata` with the given event count.
    pub fn from_event_count(event_count: usize) -> Self {
        PayloadMetadata { event_count }
    }

    /// Returns the number of events in the payload.
    pub fn event_count(&self) -> usize {
        self.event_count
    }
}

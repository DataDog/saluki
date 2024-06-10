use crate::{components::ComponentContext, topology::interconnect::EventStream};

/// Destination context.
pub struct DestinationContext {
    component_context: ComponentContext,
    events: EventStream,
}

impl DestinationContext {
    /// Creates a new `DestinationContext`.
    pub fn new(component_context: ComponentContext, events: EventStream) -> Self {
        Self {
            component_context,
            events,
        }
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    /// Gets a mutable reference to the event stream.
    pub fn events(&mut self) -> &mut EventStream {
        &mut self.events
    }
}

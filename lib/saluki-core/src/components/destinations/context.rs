use crate::{components::ComponentContext, topology::interconnect::EventStream};

pub struct DestinationContext {
    component_context: ComponentContext,
    events: EventStream,
}

impl DestinationContext {
    pub fn new(component_context: ComponentContext, events: EventStream) -> Self {
        Self {
            component_context,
            events,
        }
    }

    #[allow(unused)]
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    pub fn events(&mut self) -> &mut EventStream {
        &mut self.events
    }
}

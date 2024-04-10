pub mod destinations;
pub mod metrics;
pub mod sources;
pub mod transforms;

pub use self::destinations::{Destination, DestinationBuilder, DestinationContext};
pub use self::sources::{Source, SourceBuilder, SourceContext};
pub use self::transforms::{SynchronousTransform, Transform, TransformBuilder, TransformContext};

use crate::topology::ComponentId;

#[derive(Clone)]
pub struct ComponentContext {
    component_id: ComponentId,
    component_type: &'static str,
}

impl ComponentContext {
    pub fn source(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "source",
        }
    }

    pub fn transform(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "transform",
        }
    }

    pub fn destination(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "destination",
        }
    }

    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    pub fn component_type(&self) -> &'static str {
        self.component_type
    }
}

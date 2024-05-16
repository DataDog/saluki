use std::collections::HashMap;

use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;
use snafu::{ResultExt as _, Snafu};

use crate::components::{DestinationBuilder, SourceBuilder, TransformBuilder};

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    ComponentId, ComponentOutputId,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum BlueprintError {
    #[snafu(display("Failed to build/validate topology graph: {}", source))]
    InvalidGraph { source: GraphError },

    #[snafu(display("Failed to build component '{}': {}", id, source))]
    FailedToBuildComponent { id: ComponentId, source: GenericError },
}

/// A topology blueprint represents a directed graph of components.
#[derive(Default)]
pub struct TopologyBlueprint {
    graph: Graph,
    sources: HashMap<ComponentId, Box<dyn SourceBuilder>>,
    transforms: HashMap<ComponentId, Box<dyn TransformBuilder>>,
    destinations: HashMap<ComponentId, Box<dyn DestinationBuilder>>,
}

impl TopologyBlueprint {
    pub fn add_source<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: SourceBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_source(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.sources.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn add_transform<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: TransformBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_transform(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.transforms.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn add_destination<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: DestinationBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_destination(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.destinations.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn connect_component<I, I2, I2T>(
        &mut self, maybe_raw_component_id: I, inputs: I2,
    ) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        I2: IntoIterator<Item = I2T>,
        I2T: Into<String> + Clone,
        ComponentOutputId: TryFrom<I2T>,
        <ComponentOutputId as TryFrom<I2T>>::Error: Into<String>,
    {
        for maybe_raw_output_id in inputs.into_iter() {
            self.graph
                .add_edge(maybe_raw_output_id, maybe_raw_component_id.clone())
                .context(InvalidGraph)?;
        }

        Ok(self)
    }

    pub async fn build(self) -> Result<BuiltTopology, BlueprintError> {
        self.graph.validate().context(InvalidGraph)?;

        let mut sources = HashMap::new();
        for (id, builder) in self.sources {
            match builder.build().await {
                Ok(source) => {
                    sources.insert(id, source);
                }
                Err(e) => return Err(BlueprintError::FailedToBuildComponent { id, source: e }),
            }
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            match builder.build().await {
                Ok(transform) => {
                    transforms.insert(id, transform);
                }
                Err(e) => return Err(BlueprintError::FailedToBuildComponent { id, source: e }),
            }
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            match builder.build().await {
                Ok(destination) => {
                    destinations.insert(id, destination);
                }
                Err(e) => return Err(BlueprintError::FailedToBuildComponent { id, source: e }),
            }
        }

        Ok(BuiltTopology::from_parts(self.graph, sources, transforms, destinations))
    }
}

impl MemoryBounds for TopologyBlueprint {
    fn calculate_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for source in self.sources.values() {
            source.calculate_bounds(builder);
        }

        for transform in self.transforms.values() {
            transform.calculate_bounds(builder);
        }

        for destination in self.destinations.values() {
            destination.calculate_bounds(builder);
        }
    }
}

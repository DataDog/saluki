use std::collections::HashMap;

use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;
use snafu::{ResultExt as _, Snafu};

use crate::components::{destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder};

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    interconnect::EventBuffer,
    ComponentId, ComponentOutputId,
};

/// A topology blueprint error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum BlueprintError {
    /// Adding a component/connection lead to an invalid graph.
    #[snafu(display("Failed to build/validate topology graph: {}", source))]
    InvalidGraph {
        /// The underlying graph error.
        source: GraphError,
    },

    /// Failed to build a component.
    #[snafu(display("Failed to build component '{}': {}", id, source))]
    FailedToBuildComponent {
        /// Component ID for the component that failed to build.
        id: ComponentId,

        /// The underlying component build error.
        source: GenericError,
    },
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
    /// Adds a source component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
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

    /// Adds a transform component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
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

    /// Adds a destination component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
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

    /// Connects one or more source components to a destination commponent.
    ///
    /// ## Errors
    ///
    /// If the destination component ID, or any of the source component IDs, are invalid or do not exist, or if the data
    /// types between one of the source/destination component pairs is incompatible, an error is returned.
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

    /// Builds the topology.
    ///
    /// ## Errors
    ///
    /// If any of the components could not be built, an error is returned.
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
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let mut component_builder = builder.component("components");

        // Account for sources, transforms, and destinations.
        let mut source_builder = component_builder.component("sources");
        for (name, source) in &self.sources {
            let mut source_builder = source_builder.component(name.to_string());
            source.specify_bounds(&mut source_builder);
        }

        let mut transform_builder = component_builder.component("transforms");
        for (name, transform) in &self.transforms {
            let mut transform_builder = transform_builder.component(name.to_string());
            transform.specify_bounds(&mut transform_builder);
        }

        let mut destination_builder = component_builder.component("destinations");
        for (name, destination) in &self.destinations {
            let mut destination_builder = destination_builder.component(name.to_string());
            destination.specify_bounds(&mut destination_builder);
        }

        // Now account for all of the things that we provide components by default, like the global event buffer pool,
        // interconnect channels, and so on.

        // Every component that receives events gets an interconnect channel, which means all transforms and
        // destinations. These are fixed-sized channels, so we can account for them here.
        //
        // TODO: These values are semi-useless because the real memory usage is in the actual `Vec<Event>` that
        // underpins the buffer pool-capable `EventBuffer` wrapper type. Realistically, what we _want_ is have those
        // underlying vectors somehow be fixed-size, and then we would be able to actually say something here like our
        // fized-size buffer pool for `EventBuffer` is worth <buffer pool max size> * <size_of::<Event>() * max events
        // per buffer> bytes... and so on.
        builder
            .component("interconnects")
            .minimum()
            .with_array::<EventBuffer>(self.transforms.len() + self.destinations.len());

        // We also have our global event buffer pool that all components have shared access to.
        builder
            .component("buffer_pools")
            .component("event_buffer")
            .minimum()
            .with_array::<EventBuffer>(1024);
    }
}

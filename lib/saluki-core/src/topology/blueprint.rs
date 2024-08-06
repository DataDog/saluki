use std::collections::HashMap;

use memory_accounting::{
    allocator::{ComponentRegistry, Track as _, Tracked},
    MemoryBounds, MemoryBoundsBuilder,
};
use saluki_error::GenericError;
use snafu::{ResultExt as _, Snafu};

use crate::components::{destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder};

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    interconnect::EventBuffer,
    ComponentId,
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
    sources: HashMap<ComponentId, Tracked<Box<dyn SourceBuilder>>>,
    transforms: HashMap<ComponentId, Tracked<Box<dyn TransformBuilder>>>,
    destinations: HashMap<ComponentId, Tracked<Box<dyn DestinationBuilder>>>,
}

impl TopologyBlueprint {
    /// Adds a source component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_source<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: AsRef<str>,
        B: SourceBuilder + 'static,
    {
        let component_id = self.graph.add_source(component_id, &builder).context(InvalidGraph)?;
        let component_token = ComponentRegistry::global().register_component(component_id.to_string());

        let builder: Box<dyn SourceBuilder> = Box::new(builder);
        let _ = self
            .sources
            .insert(component_id, builder.track_allocations(component_token));

        Ok(self)
    }

    /// Adds a transform component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_transform<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: AsRef<str>,
        B: TransformBuilder + 'static,
    {
        let component_id = self.graph.add_transform(component_id, &builder).context(InvalidGraph)?;
        let component_token = ComponentRegistry::global().register_component(component_id.to_string());

        let builder: Box<dyn TransformBuilder> = Box::new(builder);
        let _ = self
            .transforms
            .insert(component_id, builder.track_allocations(component_token));

        Ok(self)
    }

    /// Adds a destination component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_destination<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: AsRef<str>,
        B: DestinationBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_destination(component_id, &builder)
            .context(InvalidGraph)?;
        let component_token = ComponentRegistry::global().register_component(component_id.to_string());

        let builder: Box<dyn DestinationBuilder> = Box::new(builder);
        let _ = self
            .destinations
            .insert(component_id, builder.track_allocations(component_token));

        Ok(self)
    }

    /// Connects one or more source component outputs to a destination component.
    ///
    /// ## Errors
    ///
    /// If the destination component ID, or any of the source component IDs, are invalid or do not exist, or if the data
    /// types between one of the source/destination component pairs is incompatible, an error is returned.
    pub fn connect_component<DI, SI, T>(
        &mut self, destination_component_id: DI, source_output_component_ids: SI,
    ) -> Result<&mut Self, BlueprintError>
    where
        DI: AsRef<str>,
        SI: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        for source_output_component_id in source_output_component_ids.into_iter() {
            self.graph
                .add_edge(source_output_component_id, destination_component_id.as_ref())
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
            let _guard = builder.enter();
            match builder.inner_ref().build().await {
                Ok(source) => {
                    sources.insert(id, source.in_current_component());
                }
                Err(e) => return Err(BlueprintError::FailedToBuildComponent { id, source: e }),
            }
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let _guard = builder.enter();
            match builder.inner_ref().build().await {
                Ok(transform) => {
                    transforms.insert(id, transform.in_current_component());
                }
                Err(e) => return Err(BlueprintError::FailedToBuildComponent { id, source: e }),
            }
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let _guard = builder.enter();
            match builder.inner_ref().build().await {
                Ok(destination) => {
                    destinations.insert(id, destination.in_current_component());
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
            source_builder.bounded_component(name.to_string(), source.inner_ref());
        }

        let mut transform_builder = component_builder.component("transforms");
        for (name, transform) in &self.transforms {
            transform_builder.bounded_component(name.to_string(), transform.inner_ref());
        }

        let mut destination_builder = component_builder.component("destinations");
        for (name, destination) in &self.destinations {
            destination_builder.bounded_component(name.to_string(), destination.inner_ref());
        }

        // Now account for all of the things that we provide components by default, like the global event buffer pool,
        // interconnect channels, and so on.

        // Every component that receives events gets an interconnect channel, which means all transforms and
        // destinations. These are fixed-sized channels, so we can account for them here.
        //
        // TODO: These values are semi-useless because the real memory usage is in the actual `Vec<Event>` that
        // underpins the buffer pool-capable `EventBuffer` wrapper type. Realistically, what we _want_ is have those
        // underlying vectors somehow be fixed-size, and then we would be able to actually say something here like our
        // fixed-size buffer pool for `EventBuffer` is worth <buffer pool max size> * <size_of::<Event>() * max events
        // per buffer> bytes... and so on.
        builder
            .component("interconnects")
            .minimum()
            .with_array::<EventBuffer>((self.transforms.len() + self.destinations.len()) * 128);

        // We also have our global event buffer pool that all components have shared access to.
        builder
            .component("buffer_pools")
            .component("event_buffer")
            .minimum()
            .with_array::<EventBuffer>(1024);
    }
}

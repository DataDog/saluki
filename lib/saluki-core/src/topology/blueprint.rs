use std::collections::HashMap;

use memory_accounting::{
    allocator::{Track as _, Tracked},
    ComponentRegistry,
};
use saluki_error::GenericError;
use snafu::{ResultExt as _, Snafu};

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    interconnect::EventBuffer,
    ComponentId,
};
use crate::components::{destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder};

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
pub struct TopologyBlueprint {
    graph: Graph,
    sources: HashMap<ComponentId, Tracked<Box<dyn SourceBuilder>>>,
    transforms: HashMap<ComponentId, Tracked<Box<dyn TransformBuilder>>>,
    destinations: HashMap<ComponentId, Tracked<Box<dyn DestinationBuilder>>>,
    component_registry: ComponentRegistry,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` attached to the given `ComponentRegistry`.
    pub fn from_component_registry(mut component_registry: ComponentRegistry) -> Self {
        // Update our bounds for some basic stuff that we know about upfront.
        let mut bounds_builder = component_registry.bounds_builder();
        bounds_builder
            .component("buffer_pools/event_buffer")
            .minimum()
            .with_array::<EventBuffer>(1024);

        Self {
            graph: Graph::default(),
            sources: HashMap::new(),
            transforms: HashMap::new(),
            destinations: HashMap::new(),
            component_registry,
        }
    }

    fn update_bounds_for_interconnect(&mut self) {
        // Increment the size of our interconnects-related bounds to account for a newly-added transform or destination.
        self.component_registry
            .get_or_create("interconnects")
            .bounds_builder()
            .minimum()
            .with_array::<EventBuffer>(128);
    }

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

        let mut source_registry = self
            .component_registry
            .get_or_create(format!("components/sources/{}", component_id));
        let mut bounds_builder = source_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn SourceBuilder> = Box::new(builder);
        let _ = self
            .sources
            .insert(component_id, builder.track_allocations(source_registry.token()));

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

        self.update_bounds_for_interconnect();

        let mut transform_registry = self
            .component_registry
            .get_or_create(format!("components/transforms/{}", component_id));
        let mut bounds_builder = transform_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn TransformBuilder> = Box::new(builder);
        let _ = self
            .transforms
            .insert(component_id, builder.track_allocations(transform_registry.token()));

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

        self.update_bounds_for_interconnect();

        let mut destination_registry = self
            .component_registry
            .get_or_create(format!("components/destinations/{}", component_id));
        let mut bounds_builder = destination_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn DestinationBuilder> = Box::new(builder);
        let _ = self
            .destinations
            .insert(component_id, builder.track_allocations(destination_registry.token()));

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

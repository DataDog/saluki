use std::collections::HashMap;

use memory_accounting::{allocator::Track as _, ComponentRegistry};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_event::Event;
use snafu::Snafu;

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    interconnect::{FixedSizeEventBuffer, FixedSizeEventBufferInner},
    ComponentId, RegisteredComponent,
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
    sources: HashMap<ComponentId, RegisteredComponent<Box<dyn SourceBuilder>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Box<dyn TransformBuilder>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Box<dyn DestinationBuilder>>>,
    component_registry: ComponentRegistry,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` attached to the given `ComponentRegistry`.
    pub fn from_component_registry(mut component_registry: ComponentRegistry) -> Self {
        // Account for our global event buffer pool, which has a lower and upper bound for the pooled object limit, but
        // using fixed-size event buffers.
        //
        // Based on how minimum/firm limits are calculated, we have to subtract the minimum size from our firm size.
        //
        // TODO: We should consider adding a helper to `MemoryBoundsBuilder` that does this for you, since it would also
        // ensure that if we ever changed the logic of how minimum/firm limits are used in calculations, we could avoid
        // having to change it in all places that need to do these manual minimum/firm calculations.
        const GLOBAL_EVENT_BUFFER_SIZE: usize =
            std::mem::size_of::<FixedSizeEventBufferInner>() + (512 * std::mem::size_of::<Event>());
        const GLOBAL_EVENT_BUFFER_POOL_SIZE_MIN: usize = 32 * GLOBAL_EVENT_BUFFER_SIZE;
        const GLOBAL_EVENT_BUFFER_POOL_SIZE_FIRM: usize =
            (512 * GLOBAL_EVENT_BUFFER_SIZE) - GLOBAL_EVENT_BUFFER_POOL_SIZE_MIN;

        let mut bounds_builder = component_registry.bounds_builder();
        let mut event_buffer_bounds_builder = bounds_builder.subcomponent("buffer_pools/event_buffer");
        event_buffer_bounds_builder
            .minimum()
            .with_fixed_amount(GLOBAL_EVENT_BUFFER_POOL_SIZE_MIN);
        event_buffer_bounds_builder
            .firm()
            .with_fixed_amount(GLOBAL_EVENT_BUFFER_POOL_SIZE_FIRM);

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
            .with_array::<FixedSizeEventBuffer>(128);
    }

    /// Adds a source component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_source<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: SourceBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_source(component_id, &builder)
            .error_context("Failed to build/validate topology graph.")?;

        let mut source_registry = self
            .component_registry
            .get_or_create(format!("components.sources.{}", component_id));
        let mut bounds_builder = source_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn SourceBuilder> = Box::new(builder);
        let _ = self
            .sources
            .insert(component_id, RegisteredComponent::new(builder, source_registry));

        Ok(self)
    }

    /// Adds a transform component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_transform<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: TransformBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_transform(component_id, &builder)
            .error_context("Failed to build/validate topology graph.")?;

        self.update_bounds_for_interconnect();

        let mut transform_registry = self
            .component_registry
            .get_or_create(format!("components.transforms.{}", component_id));
        let mut bounds_builder = transform_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn TransformBuilder> = Box::new(builder);
        let _ = self
            .transforms
            .insert(component_id, RegisteredComponent::new(builder, transform_registry));

        Ok(self)
    }

    /// Adds a destination component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_destination<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: DestinationBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_destination(component_id, &builder)
            .error_context("Failed to build/validate topology graph.")?;

        self.update_bounds_for_interconnect();

        let mut destination_registry = self
            .component_registry
            .get_or_create(format!("components.destinations.{}", component_id));
        let mut bounds_builder = destination_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        let builder: Box<dyn DestinationBuilder> = Box::new(builder);
        let _ = self
            .destinations
            .insert(component_id, RegisteredComponent::new(builder, destination_registry));

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
    ) -> Result<&mut Self, GenericError>
    where
        DI: AsRef<str>,
        SI: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        for source_output_component_id in source_output_component_ids.into_iter() {
            self.graph
                .add_edge(source_output_component_id, destination_component_id.as_ref())
                .error_context("Failed to build/validate topology graph.")?;
        }

        Ok(self)
    }

    /// Builds the topology.
    ///
    /// ## Errors
    ///
    /// If any of the components could not be built, an error is returned.
    pub async fn build(mut self) -> Result<BuiltTopology, GenericError> {
        self.graph
            .validate()
            .error_context("Failed to build/validate topology graph.")?;

        let mut sources = HashMap::new();
        for (id, builder) in self.sources {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let _guard = allocation_token.enter();
            let source = builder
                .build()
                .await
                .with_error_context(|| format!("Failed to build source '{}'.", id))?;

            sources.insert(
                id,
                RegisteredComponent::new(source.in_current_allocation_group(), component_registry),
            );
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let _guard = allocation_token.enter();
            let transform = builder
                .build()
                .await
                .with_error_context(|| format!("Failed to build transform '{}'.", id))?;

            transforms.insert(
                id,
                RegisteredComponent::new(transform.in_current_allocation_group(), component_registry),
            );
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let _guard = allocation_token.enter();
            let destination = builder
                .build()
                .await
                .with_error_context(|| format!("Failed to build destination '{}'.", id))?;

            destinations.insert(
                id,
                RegisteredComponent::new(destination.in_current_allocation_group(), component_registry),
            );
        }

        Ok(BuiltTopology::from_parts(
            self.graph,
            sources,
            transforms,
            destinations,
            self.component_registry.token(),
        ))
    }
}

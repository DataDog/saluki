use std::{collections::HashMap, num::NonZeroUsize};

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
use crate::components::{
    destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder, ComponentContext,
};

// SAFETY: These are obviously all non-zero.
const DEFAULT_EVENT_BUFFER_SIZE: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(1024) };
const DEFAULT_EVENT_BUFFER_POOL_SIZE_MIN: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(32) };
const DEFAULT_EVENT_BUFFER_POOL_SIZE_MAX: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(512) };

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
    name: String,
    graph: Graph,
    sources: HashMap<ComponentId, RegisteredComponent<Box<dyn SourceBuilder + Send>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Box<dyn TransformBuilder + Send>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Box<dyn DestinationBuilder + Send>>>,
    component_registry: ComponentRegistry,
    event_buffer_pool_buffer_size: usize,
    event_buffer_pool_size_min: usize,
    event_buffer_pool_size_max: usize,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` with the given name.
    pub fn new(name: &str, component_registry: &ComponentRegistry) -> Self {
        // Create a nested component registry for this topology.
        let component_registry = component_registry.get_or_create("topology").get_or_create(name);

        let mut blueprint = Self {
            name: name.to_string(),
            graph: Graph::default(),
            sources: HashMap::new(),
            transforms: HashMap::new(),
            destinations: HashMap::new(),
            component_registry,
            event_buffer_pool_buffer_size: 0,
            event_buffer_pool_size_min: 0,
            event_buffer_pool_size_max: 0,
        };

        // Set our default event buffer pool sizing.
        blueprint.with_global_event_buffer_pool_size(
            DEFAULT_EVENT_BUFFER_SIZE,
            DEFAULT_EVENT_BUFFER_POOL_SIZE_MIN,
            DEFAULT_EVENT_BUFFER_POOL_SIZE_MAX,
        );

        blueprint
    }

    fn update_bounds_for_interconnect(&mut self) {
        // Increment the size of our interconnects-related bounds to account for a newly-added transform or destination.
        self.component_registry
            .get_or_create("interconnects")
            .bounds_builder()
            .minimum()
            .with_array::<FixedSizeEventBuffer>("fixed size event buffers", 128);
    }

    /// Sets the global event buffer pool sizing.
    ///
    /// The global event buffer pool is used by components to acquire an event buffer that can be used to forward events
    /// to the next component in the topology.
    ///
    /// Each individual event buffer will be allocated to hold `buffer_size` events, and the pool will be sized to hold
    /// a minimum of `size_min` buffer, and up to a maximum of `size_max` event buffers.
    pub fn with_global_event_buffer_pool_size(
        &mut self, buffer_size: NonZeroUsize, size_min: NonZeroUsize, size_max: NonZeroUsize,
    ) -> &mut Self {
        self.event_buffer_pool_buffer_size = buffer_size.get();
        self.event_buffer_pool_size_min = size_min.get();
        self.event_buffer_pool_size_max = size_max.get();

        // Account for our global event buffer pool, which has a lower and upper bound for the pooled object limit, but
        // using fixed-size event buffers.
        //
        // Based on how minimum/firm limits are calculated, we have to subtract the minimum size from our firm size.
        let buffer_size_bytes = std::mem::size_of::<FixedSizeEventBufferInner>()
            + (self.event_buffer_pool_buffer_size * std::mem::size_of::<Event>());
        let event_buffer_pool_min_bytes = self.event_buffer_pool_size_min * buffer_size_bytes;
        let event_buffer_pool_max_bytes =
            (self.event_buffer_pool_size_max * buffer_size_bytes) - event_buffer_pool_min_bytes;

        let mut bounds_builder = self.component_registry.bounds_builder();
        let mut bounds_builder = bounds_builder.subcomponent("buffer_pools/event_buffer");

        bounds_builder.reset();
        bounds_builder
            .minimum()
            .with_fixed_amount("global event buffer pool", event_buffer_pool_min_bytes);
        bounds_builder
            .firm()
            .with_fixed_amount("global event buffer pool", event_buffer_pool_max_bytes);

        self
    }

    /// Adds a source component to the blueprint.
    ///
    /// ## Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_source<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: SourceBuilder + Send + 'static,
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

        let _ = self.sources.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), source_registry),
        );

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
        B: TransformBuilder + Send + 'static,
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

        let _ = self.transforms.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), transform_registry),
        );

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
        B: DestinationBuilder + Send + 'static,
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

        let _ = self.destinations.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), destination_registry),
        );

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

            let component_context = ComponentContext::source(id.clone());
            let source = builder
                .build(component_context)
                .track_allocations(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build source '{}'.", id))?;

            sources.insert(
                id,
                RegisteredComponent::new(source.track_allocations(allocation_token), component_registry),
            );
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::transform(id.clone());
            let transform = builder
                .build(component_context)
                .track_allocations(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build transform '{}'.", id))?;

            transforms.insert(
                id,
                RegisteredComponent::new(transform.track_allocations(allocation_token), component_registry),
            );
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::destination(id.clone());
            let destination = builder
                .build(component_context)
                .track_allocations(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build destination '{}'.", id))?;

            destinations.insert(
                id,
                RegisteredComponent::new(destination.track_allocations(allocation_token), component_registry),
            );
        }

        Ok(BuiltTopology::from_parts(
            self.name,
            self.graph,
            self.event_buffer_pool_buffer_size,
            self.event_buffer_pool_size_min,
            self.event_buffer_pool_size_max,
            sources,
            transforms,
            destinations,
            self.component_registry.token(),
        ))
    }
}

use std::{collections::HashMap, num::NonZeroUsize};

use memory_accounting::{allocator::Track as _, ComponentRegistry};
use saluki_error::{ErrorContext as _, GenericError};
use snafu::Snafu;

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    interconnect::FixedSizeEventBuffer,
    ComponentId, RegisteredComponent,
};
use crate::{
    components::{
        destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder, ComponentContext,
    },
    data_model::event::Event,
    topology::TopologyConfiguration,
};

// SAFETY: These are obviously non-zero.
const DEFAULT_EVENT_BUFFER_SIZE: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

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
pub struct TopologyBlueprint<T> {
    name: String,
    config: T,
    graph: Graph,
    sources: HashMap<ComponentId, RegisteredComponent<Box<dyn SourceBuilder + Send>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Box<dyn TransformBuilder + Send>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Box<dyn DestinationBuilder + Send>>>,
    component_registry: ComponentRegistry,
    event_buffer_pool_buffer_size: NonZeroUsize,
}

impl<T: TopologyConfiguration> TopologyBlueprint<T> {
    /// Creates an empty `TopologyBlueprint` with the given name and configuration.
    pub fn new(name: &str, config: T, component_registry: &ComponentRegistry) -> Self {
        // Create a nested component registry for this topology.
        let component_registry = component_registry.get_or_create("topology").get_or_create(name);

        Self {
            name: name.to_string(),
            config,
            graph: Graph::default(),
            sources: HashMap::new(),
            transforms: HashMap::new(),
            destinations: HashMap::new(),
            component_registry,
            event_buffer_pool_buffer_size: DEFAULT_EVENT_BUFFER_SIZE,
        }
    }

    /// Sets the size of event buffers in the global event buffer pool.
    ///
    /// The global event buffer pool is used by components to acquire an event buffer that can be used to forward events
    /// to the next component in the topology. Each individual event buffer will be allocated to hold `buffer_size` events.
    ///
    /// Defaults to 1024 events.
    pub fn with_global_event_buffer_pool_size(&mut self, buffer_size: NonZeroUsize) -> &mut Self {
        self.event_buffer_pool_buffer_size = buffer_size;
        self.recalculate_bounds();
        self
    }

    fn recalculate_bounds(&mut self) {
        let interconnect_capacity = self.config.interconnect_capacity().get();

        // Adjust the bounds related to interconnects.
        //
        // Every non-source component has an interconnect.
        let total_interconnect_capacity = interconnect_capacity * (self.transforms.len() + self.destinations.len());
        let mut bounds_builder = self.component_registry.bounds_builder();
        let mut bounds_builder = bounds_builder.subcomponent("interconnects");

        bounds_builder.reset();
        bounds_builder
            .minimum()
            .with_array::<FixedSizeEventBuffer<1024>>("fixed size event buffers", total_interconnect_capacity);

        // Adjust the bounds related to event buffers themselves.
        //
        // We calculate the maximum number of event buffers by adding up the total capacity of all non-source components, plus the count
        // of non-destination components. This is the effective upper bound because once all component channels are full, sending
        // components can only allocate one more event buffer before being blocked on sending, which is then the effective upper bound.
        let max_in_flight_event_buffers = ((self.transforms.len() + self.destinations.len()) * interconnect_capacity)
            + self.sources.len()
            + self.transforms.len();

        // TODO: This is fragile, and ideally we'd be deriving the of this directly from a method/function we could call with
        // `FixedSizeEventBuffer<1024>` (or, eventually, `TopologyConfig::Events`) as a parameter to figure it out for us.
        let buffer_size_bytes =
            std::mem::size_of::<FixedSizeEventBuffer<1024>>() + (std::mem::size_of::<Event>() * 1024);

        let event_buffer_pool_max_bytes = max_in_flight_event_buffers * buffer_size_bytes;

        let mut bounds_builder = self.component_registry.bounds_builder();
        let mut bounds_builder = bounds_builder.subcomponent("buffer_pools/event_buffer");

        bounds_builder.reset();
        bounds_builder
            .firm()
            .with_fixed_amount("global event buffer pool", event_buffer_pool_max_bytes);
    }

    /// Adds a source component to the blueprint.
    ///
    /// # Errors
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
            .error_context("Failed to add source to topology graph.")?;

        let mut source_registry = self
            .component_registry
            .get_or_create(format!("components.sources.{}", component_id));
        let mut bounds_builder = source_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.sources.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), source_registry),
        );

        Ok(self)
    }

    /// Adds a transform component to the blueprint.
    ///
    /// # Errors
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
            .error_context("Failed to add transform to topology graph.")?;

        let mut transform_registry = self
            .component_registry
            .get_or_create(format!("components.transforms.{}", component_id));
        let mut bounds_builder = transform_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.transforms.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), transform_registry),
        );

        Ok(self)
    }

    /// Adds a destination component to the blueprint.
    ///
    /// # Errors
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
            .error_context("Failed to add destination to topology graph.")?;

        let mut destination_registry = self
            .component_registry
            .get_or_create(format!("components.destinations.{}", component_id));
        let mut bounds_builder = destination_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.destinations.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), destination_registry),
        );

        Ok(self)
    }

    /// Connects one or more source component outputs to a destination component.
    ///
    /// # Errors
    ///
    /// If the destination component ID, or any of the source component IDs, are invalid or do not exist, or if the data
    /// types between one of the source/destination component pairs is incompatible, an error is returned.
    pub fn connect_component<DI, SI, I>(
        &mut self, destination_component_id: DI, source_output_component_ids: SI,
    ) -> Result<&mut Self, GenericError>
    where
        DI: AsRef<str>,
        SI: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        for source_output_component_id in source_output_component_ids.into_iter() {
            self.graph
                .add_edge(source_output_component_id, destination_component_id.as_ref())
                .error_context("Failed to add component connection to topology graph.")?;
        }

        Ok(self)
    }

    /// Builds the topology.
    ///
    /// # Errors
    ///
    /// If any of the components could not be built, an error is returned.
    pub async fn build(mut self) -> Result<BuiltTopology<T>, GenericError> {
        self.graph.validate().error_context("Failed to build topology graph.")?;

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
            self.config,
            self.graph,
            sources,
            transforms,
            destinations,
            self.component_registry.token(),
        ))
    }
}

use std::{collections::HashMap, num::NonZeroUsize};

use memory_accounting::{allocator::Track as _, ComponentRegistry, UsageExpr};
use saluki_error::{ErrorContext as _, GenericError};
use snafu::Snafu;

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    ComponentId, RegisteredComponent,
};
use crate::{
    components::{
        destinations::DestinationBuilder, encoders::EncoderBuilder, forwarders::ForwarderBuilder,
        sources::SourceBuilder, transforms::TransformBuilder, ComponentContext,
    },
    data_model::event::Event,
    topology::{EventsBuffer, DEFAULT_EVENTS_BUFFER_CAPACITY},
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
pub struct TopologyBlueprint {
    name: String,
    graph: Graph,
    sources: HashMap<ComponentId, RegisteredComponent<Box<dyn SourceBuilder + Send>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Box<dyn TransformBuilder + Send>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Box<dyn DestinationBuilder + Send>>>,
    encoders: HashMap<ComponentId, RegisteredComponent<Box<dyn EncoderBuilder + Send>>>,
    forwarders: HashMap<ComponentId, RegisteredComponent<Box<dyn ForwarderBuilder + Send>>>,
    component_registry: ComponentRegistry,
    interconnect_capacity: NonZeroUsize,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` with the given name.
    pub fn new(name: &str, component_registry: &ComponentRegistry) -> Self {
        // Create a nested component registry for this topology.
        let component_registry = component_registry.get_or_create("topology").get_or_create(name);

        Self {
            name: name.to_string(),
            graph: Graph::default(),
            sources: HashMap::new(),
            transforms: HashMap::new(),
            destinations: HashMap::new(),
            encoders: HashMap::new(),
            forwarders: HashMap::new(),
            component_registry,
            interconnect_capacity: super::DEFAULT_INTERCONNECT_CAPACITY,
        }
    }

    /// Sets the capacity of interconnects in the topology.
    ///
    /// Interconnects are used to connect components to one another. Once their capacity is reached, no more items can be sent
    /// through until in-flight items are processed. This will apply backpressure to the upstream components. Raising or lowering
    /// the capacity allows trading off throughput at the expense of memory usage.
    ///
    /// Defaults to 128.
    pub fn with_interconnect_capacity(&mut self, capacity: NonZeroUsize) -> &mut Self {
        self.interconnect_capacity = capacity;
        self.recalculate_bounds();
        self
    }

    fn recalculate_bounds(&mut self) {
        let interconnect_capacity = self.interconnect_capacity.get();

        let mut bounds_builder = self.component_registry.bounds_builder();
        let mut bounds_builder = bounds_builder.subcomponent("interconnects");
        bounds_builder.reset();

        // Adjust the bounds related to interconnects.
        //
        // This deals with the minimum size of the interconnects themselves, since they're bounded and thus allocated
        // up-front. Every non-source component has an interconnect.
        let total_interconnect_capacity = interconnect_capacity * (self.transforms.len() + self.destinations.len());
        bounds_builder
            .minimum()
            .with_array::<EventsBuffer>("events", total_interconnect_capacity);

        // TODO: Add a minimum subitem for payloads when we have payload interconnects.

        // Adjust the bounds related to event buffers themselves.
        //
        // We calculate the maximum number of event buffers by adding up the total capacity of all non-source components, plus the count
        // of non-destination components. This is the effective upper bound because once all component channels are full, sending
        // components can only allocate one more event buffer before being blocked on sending, which is then the effective upper bound.
        //
        // TODO: Somewhat fragile. Need to revisit this.
        // TODO: Add a firm subitem for payloads when we have payload interconnects.
        let max_in_flight_event_buffers = ((self.transforms.len() + self.destinations.len()) * interconnect_capacity)
            + self.sources.len()
            + self.transforms.len();

        bounds_builder
            .firm()
            // max_in_flight_event_buffers * (size_of<EventsContainer> + (size_of<Event> * default_event_buffer_capacity))
            .with_expr(UsageExpr::product(
                "events",
                UsageExpr::constant("max in-flight event buffers", max_in_flight_event_buffers),
                UsageExpr::sum(
                    "",
                    UsageExpr::struct_size::<EventsBuffer>("events buffer"),
                    UsageExpr::product(
                        "",
                        UsageExpr::struct_size::<Event>("event"),
                        UsageExpr::constant("default event buffer capacity", DEFAULT_EVENTS_BUFFER_CAPACITY),
                    ),
                ),
            ));
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

    /// Adds an encoder component to the blueprint.
    ///
    /// # Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_encoder<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: EncoderBuilder + Send + 'static,
    {
        let component_id = self
            .graph
            .add_encoder(component_id, &builder)
            .error_context("Failed to add encoder to topology graph.")?;

        let mut encoder_registry = self
            .component_registry
            .get_or_create(format!("components.encoders.{}", component_id));
        let mut bounds_builder = encoder_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.encoders.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), encoder_registry),
        );

        Ok(self)
    }

    /// Adds a forwarder component to the blueprint.
    ///
    /// # Errors
    ///
    /// If the component ID is invalid or the component cannot be added to the graph, an error is returned.
    pub fn add_forwarder<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: ForwarderBuilder + Send + 'static,
    {
        let component_id = self
            .graph
            .add_forwarder(component_id, &builder)
            .error_context("Failed to add forwarder to topology graph.")?;

        let mut forwarder_registry = self
            .component_registry
            .get_or_create(format!("components.forwarders.{}", component_id));
        let mut bounds_builder = forwarder_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.forwarders.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), forwarder_registry),
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
    pub async fn build(mut self) -> Result<BuiltTopology, GenericError> {
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

        let mut encoders = HashMap::new();
        for (id, builder) in self.encoders {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::encoder(id.clone());
            let encoder = builder
                .build(component_context)
                .track_allocations(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build encoder '{}'.", id))?;

            encoders.insert(
                id,
                RegisteredComponent::new(encoder.track_allocations(allocation_token), component_registry),
            );
        }

        let mut forwarders = HashMap::new();
        for (id, builder) in self.forwarders {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::forwarder(id.clone());
            let forwarder = builder
                .build(component_context)
                .track_allocations(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build forwarder '{}'.", id))?;

            forwarders.insert(
                id,
                RegisteredComponent::new(forwarder.track_allocations(allocation_token), component_registry),
            );
        }

        Ok(BuiltTopology::from_parts(
            self.name,
            self.graph,
            sources,
            transforms,
            destinations,
            encoders,
            forwarders,
            self.component_registry.token(),
            self.interconnect_capacity,
        ))
    }
}

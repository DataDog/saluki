use std::{collections::HashMap, num::NonZeroUsize};

use resource_accounting::{ComponentRegistry, Track as _, UsageExpr};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use snafu::Snafu;

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    ComponentId, RegisteredComponent,
};
use crate::{
    components::{
        decoders::DecoderBuilder, destinations::DestinationBuilder, encoders::EncoderBuilder,
        forwarders::ForwarderBuilder, relays::RelayBuilder, sources::SourceBuilder, transforms::TransformBuilder,
        ComponentContext,
    },
    data_model::event::Event,
    topology::{ids::AsComponentIds, EventsBuffer, DEFAULT_EVENTS_BUFFER_CAPACITY},
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
    relays: HashMap<ComponentId, RegisteredComponent<Box<dyn RelayBuilder + Send>>>,
    decoders: HashMap<ComponentId, RegisteredComponent<Box<dyn DecoderBuilder + Send>>>,
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
            relays: HashMap::new(),
            decoders: HashMap::new(),
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
            + self.decoders.len()
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
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
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

    /// Adds a relay component to the blueprint.
    ///
    /// # Errors
    ///
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
    pub fn add_relay<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: RelayBuilder + Send + 'static,
    {
        let component_id = self
            .graph
            .add_relay(component_id, &builder)
            .error_context("Failed to add relay to topology graph.")?;

        let mut relay_registry = self
            .component_registry
            .get_or_create(format!("components.relays.{}", component_id));
        let mut bounds_builder = relay_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.relays.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), relay_registry),
        );

        Ok(self)
    }

    /// Adds a decoder component to the blueprint.
    ///
    /// # Errors
    ///
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
    pub fn add_decoder<I, B>(&mut self, component_id: I, builder: B) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
        B: DecoderBuilder + Send + 'static,
    {
        let component_id = self
            .graph
            .add_decoder(component_id, &builder)
            .error_context("Failed to add decoder to topology graph.")?;

        let mut decoder_registry = self
            .component_registry
            .get_or_create(format!("components.decoders.{}", component_id));
        let mut bounds_builder = decoder_registry.bounds_builder();
        builder.specify_bounds(&mut bounds_builder);

        self.recalculate_bounds();

        let _ = self.decoders.insert(
            component_id,
            RegisteredComponent::new(Box::new(builder), decoder_registry),
        );

        Ok(self)
    }

    /// Adds a transform component to the blueprint.
    ///
    /// # Errors
    ///
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
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
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
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
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
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
    /// If the component ID is invalid or the component can't be added to the graph, an error is returned.
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

    /// Connects one or more upstream component outputs to one or more downstream components.
    ///
    /// This method allows for ergonomically defining many-to-one, one-to-many, and many-to-many connections to
    /// facilitate common patterns like fanning in many upstream components to a single downstream component, or fanning
    /// out a single upstream component to many downstream components.
    ///
    /// When both there are both multiple upstream _and_ downstream component IDs, connections resemble a mesh: every
    /// upstream component will be connected to every downstream component. This should be rare, but is technically
    /// supported.
    ///
    /// # Errors
    ///
    /// If any of the upstream or downstream component IDs are invalid or don't exist, or if the data types between one
    /// of the upstream/downstream component pairs is incompatible, an error is returned.
    pub fn connect_components<MS, SI, MD, DI>(
        &mut self, upstream_output_component_ids: SI, downstream_component_ids: DI,
    ) -> Result<&mut Self, GenericError>
    where
        SI: AsComponentIds<MS>,
        DI: AsComponentIds<MD>,
    {
        for upstream_output_component_id in upstream_output_component_ids.as_component_ids() {
            for downstream_component_id in downstream_component_ids.as_component_ids() {
                self.graph
                    .add_edge(upstream_output_component_id.as_ref(), downstream_component_id.as_ref())
                    .error_context("Failed to add component connection to topology graph.")?;
            }
        }

        Ok(self)
    }

    /// Connects a set of component IDs to one another in a pairwise fashion.
    ///
    /// This can be used to connect multiple components -- each sharing only a single edge between one another -- in a
    /// single call instead of multiple calls.
    ///
    /// For example, passing `["first", "second", "third"]` would connect `first`'s output to `second`'s input, and
    /// `second`'s output to `third`'s input.
    ///
    /// One caveat is that only the default output of a component can be used for connections past the first pair, as
    /// the identifier given must be able to describe both the component ID to _send_ to as well as the component output
    /// ID to connect to the subsequent component. This limitation does not exist on the first component ID, since it is
    /// only used in the context of being a component output ID.
    ///
    /// # Errors
    ///
    /// If any of the component IDs are invalid or don't exist, or if the data types between one of the
    /// upstream/downstream component pairs is incompatible, or if less than two component IDs are provided, an error is
    /// returned.
    ///
    /// Care should be taken on failure as this method will not rollback any previously successful connections, which
    /// could leave the blueprint in an indeterminate state if some connections are made prior to hitting an error.
    pub fn connect_components_in_order<IT, I>(&mut self, ordered_component_ids: IT) -> Result<&mut Self, GenericError>
    where
        IT: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let mut pending_output_component_id: Option<I> = None;
        let mut connected_any = false;

        for component_id in ordered_component_ids.into_iter() {
            if let Some(output_component_id) = pending_output_component_id.take() {
                self.graph
                    .add_edge(output_component_id.as_ref(), component_id.as_ref())
                    .error_context("Failed to add component connection to topology graph.")?;

                connected_any = true;
            }

            // Store the _current_ component ID so we can chain its connection to the next component, and so on.
            pending_output_component_id = Some(component_id);
        }

        // Make sure we connected at least one pair of components together, otherwise this is an invalid connection attempt.
        if !connected_any {
            return Err(generic_error!(
                "Two or more components must be provided for connection."
            ));
        }

        Ok(self)
    }

    /// Builds the topology.
    ///
    /// # Errors
    ///
    /// If any of the components couldn't be built, an error is returned.
    pub async fn build(mut self) -> Result<BuiltTopology, GenericError> {
        self.graph.validate().error_context("Failed to build topology graph.")?;

        let mut sources = HashMap::new();
        for (id, builder) in self.sources {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::source(id.clone());
            let source = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build source '{}'.", id))?;

            sources.insert(
                id,
                RegisteredComponent::new(source.track_resources(allocation_token), component_registry),
            );
        }

        let mut relays = HashMap::new();
        for (id, builder) in self.relays {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::relay(id.clone());
            let relay = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build relay '{}'.", id))?;

            relays.insert(
                id,
                RegisteredComponent::new(relay.track_resources(allocation_token), component_registry),
            );
        }

        let mut decoders = HashMap::new();
        for (id, builder) in self.decoders {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::decoder(id.clone());
            let decoder = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build decoder '{}'.", id))?;

            decoders.insert(
                id,
                RegisteredComponent::new(decoder.track_resources(allocation_token), component_registry),
            );
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::transform(id.clone());
            let transform = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build transform '{}'.", id))?;

            transforms.insert(
                id,
                RegisteredComponent::new(transform.track_resources(allocation_token), component_registry),
            );
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::destination(id.clone());
            let destination = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build destination '{}'.", id))?;

            destinations.insert(
                id,
                RegisteredComponent::new(destination.track_resources(allocation_token), component_registry),
            );
        }

        let mut encoders = HashMap::new();
        for (id, builder) in self.encoders {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::encoder(id.clone());
            let encoder = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build encoder '{}'.", id))?;

            encoders.insert(
                id,
                RegisteredComponent::new(encoder.track_resources(allocation_token), component_registry),
            );
        }

        let mut forwarders = HashMap::new();
        for (id, builder) in self.forwarders {
            let (builder, mut component_registry) = builder.into_parts();
            let allocation_token = component_registry.token();

            let component_context = ComponentContext::forwarder(id.clone());
            let forwarder = builder
                .build(component_context)
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build forwarder '{}'.", id))?;

            forwarders.insert(
                id,
                RegisteredComponent::new(forwarder.track_resources(allocation_token), component_registry),
            );
        }

        Ok(BuiltTopology::from_parts(
            self.name,
            self.graph,
            sources,
            relays,
            decoders,
            transforms,
            destinations,
            encoders,
            forwarders,
            self.component_registry.token(),
            self.interconnect_capacity,
        ))
    }
}

#[cfg(test)]
mod tests {
    use resource_accounting::ComponentRegistry;

    use super::TopologyBlueprint;
    use crate::{
        data_model::event::EventType,
        topology::test_util::{TestDestinationBuilder, TestSourceBuilder, TestTransformBuilder},
    };

    /// Builds a blueprint pre-populated with a source, transform, and destination, all dealing in event-D events.
    ///
    /// No connections are made between the components.
    fn blueprint_with_components() -> TopologyBlueprint {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);

        blueprint
            .add_source("source", TestSourceBuilder::default_output(EventType::EventD))
            .expect("should not fail to add source")
            .add_transform(
                "transform",
                TestTransformBuilder::default_output(EventType::EventD, EventType::EventD),
            )
            .expect("should not fail to add transform")
            .add_destination(
                "destination",
                TestDestinationBuilder::with_input_type(EventType::EventD),
            )
            .expect("should not fail to add destination");

        blueprint
    }

    /// Builds a blueprint pre-populated with the given source and destination component IDs, all dealing in event-D
    /// events.
    ///
    /// No connections are made between the components.
    fn blueprint_with_sources_and_destinations(source_ids: &[&str], destination_ids: &[&str]) -> TopologyBlueprint {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);

        for source_id in source_ids {
            blueprint
                .add_source(*source_id, TestSourceBuilder::default_output(EventType::EventD))
                .expect("should not fail to add source");
        }

        for destination_id in destination_ids {
            blueprint
                .add_destination(
                    *destination_id,
                    TestDestinationBuilder::with_input_type(EventType::EventD),
                )
                .expect("should not fail to add destination");
        }

        blueprint
    }

    /// Collects the blueprint's directed connections as a sorted list of `(from, to)` component ID pairs.
    fn connected_pairs(blueprint: &TopologyBlueprint) -> Vec<(String, String)> {
        let outbound_edges = blueprint.graph.get_outbound_directed_edges();

        let mut pairs = Vec::new();
        for (from, outputs) in &outbound_edges {
            for targets in outputs.values() {
                for to in targets {
                    pairs.push((from.component_id().to_string(), to.component_id().to_string()));
                }
            }
        }
        pairs.sort();
        pairs
    }

    #[test]
    fn connect_components_in_order_errors_with_fewer_than_two_ids() {
        let mut blueprint = blueprint_with_components();

        // No component IDs at all.
        let result = blueprint.connect_components_in_order(Vec::<&str>::new()).map(|_| ());
        assert!(result.is_err());

        // A single component ID is still not enough to form a connection.
        let result = blueprint.connect_components_in_order(["source"]).map(|_| ());
        assert!(result.is_err());

        // Neither attempt should have added any connections to the graph.
        assert!(connected_pairs(&blueprint).is_empty());
    }

    #[test]
    fn connect_components_in_order_connects_pairwise_left_to_right() {
        let mut blueprint = blueprint_with_components();

        blueprint
            .connect_components_in_order(["source", "transform", "destination"])
            .expect("should not fail to connect components in order");

        // Adjacent components should be connected from left to right (`source` -> `transform` -> `destination`), with
        // a single edge shared between each pair.
        assert_eq!(
            connected_pairs(&blueprint),
            vec![
                ("source".to_string(), "transform".to_string()),
                ("transform".to_string(), "destination".to_string()),
            ],
        );
    }

    #[test]
    fn connect_component_one_to_many_fans_out() {
        // A single upstream component is fanned out to multiple downstream components. The upstream ID is given as a
        // bare string (`Single`), while the downstream IDs are given as a slice (`Multiple`).
        let mut blueprint = blueprint_with_sources_and_destinations(&["source"], &["dest_a", "dest_b"]);

        blueprint
            .connect_components("source", ["dest_a", "dest_b"])
            .expect("should not fail to connect component");

        assert_eq!(
            connected_pairs(&blueprint),
            vec![
                ("source".to_string(), "dest_a".to_string()),
                ("source".to_string(), "dest_b".to_string()),
            ],
        );
    }

    #[test]
    fn connect_component_many_to_one_fans_in() {
        // Multiple upstream components are fanned in to a single downstream component. The upstream IDs are given as a
        // slice (`Multiple`), while the downstream ID is given as a bare string (`Single`).
        let mut blueprint = blueprint_with_sources_and_destinations(&["source_a", "source_b"], &["dest"]);

        blueprint
            .connect_components(["source_a", "source_b"], "dest")
            .expect("should not fail to connect component");

        assert_eq!(
            connected_pairs(&blueprint),
            vec![
                ("source_a".to_string(), "dest".to_string()),
                ("source_b".to_string(), "dest".to_string()),
            ],
        );
    }

    #[test]
    fn connect_component_many_to_many_creates_mesh() {
        // Multiple upstream components are meshed with multiple downstream components: every upstream component is
        // connected to every downstream component. Both sides are given as slices (`Multiple`).
        let mut blueprint = blueprint_with_sources_and_destinations(&["source_a", "source_b"], &["dest_a", "dest_b"]);

        blueprint
            .connect_components(["source_a", "source_b"], ["dest_a", "dest_b"])
            .expect("should not fail to connect component");

        assert_eq!(
            connected_pairs(&blueprint),
            vec![
                ("source_a".to_string(), "dest_a".to_string()),
                ("source_a".to_string(), "dest_b".to_string()),
                ("source_b".to_string(), "dest_a".to_string()),
                ("source_b".to_string(), "dest_b".to_string()),
            ],
        );
    }
}

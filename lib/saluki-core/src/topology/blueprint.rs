use std::{collections::HashMap, future::Future, num::NonZeroUsize, pin::Pin, sync::Mutex, time::Duration};

use async_trait::async_trait;
use resource_accounting::{ComponentRegistry, MemoryLimiter, Track as _, UsageExpr};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use snafu::Snafu;
use tokio::{pin, runtime::Handle, select, sync::oneshot};
use tracing::{error, info};

use super::{
    built::{BuiltTopology, WorkerPoolConfiguration},
    graph::{Graph, GraphError},
    ComponentId, RegisteredComponent, TopologySnapshot,
};
use crate::{
    components::{
        decoders::DecoderBuilder, destinations::DestinationBuilder, encoders::EncoderBuilder,
        forwarders::ForwarderBuilder, relays::RelayBuilder, sources::SourceBuilder, transforms::TransformBuilder,
        ComponentContext,
    },
    data_model::event::Event,
    health::HealthRegistry,
    runtime::{state::DataspaceRegistry, InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture},
    topology::{ids::AsComponentIds, EventsBuffer, DEFAULT_EVENTS_BUFFER_CAPACITY},
};

const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

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
///
/// A blueprint is assembled by adding components and connecting them together, and then run by adding it to a
/// [`Supervisor`][crate::runtime::Supervisor]: `TopologyBlueprint` implements [`Supervisable`], so there is no
/// standalone spawn/run method. A blueprint can only be initialized (and thus run) once.
pub struct TopologyBlueprint {
    name: String,
    build_state: Mutex<Option<TopologyBuildState>>,
    health_registry: Option<HealthRegistry>,
    memory_limiter: Option<MemoryLimiter>,
}

/// The consumable build state of a [`TopologyBlueprint`].
///
/// This is taken out of the blueprint when it's first initialized, at which point the topology is built and spawned.
struct TopologyBuildState {
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
    shutdown_timeout: Duration,
    worker_pool_config: WorkerPoolConfiguration,
    environment_ready: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    ready_signal: Option<oneshot::Sender<()>>,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` with the given name.
    pub fn new(name: &str, component_registry: &ComponentRegistry) -> Self {
        // Create a nested component registry for this topology.
        let component_registry = component_registry.get_or_create("topology").get_or_create(name);

        let build_state = TopologyBuildState {
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
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            worker_pool_config: WorkerPoolConfiguration::Dedicated,
            environment_ready: None,
            ready_signal: None,
        };

        Self {
            name: name.to_string(),
            build_state: Mutex::new(Some(build_state)),
            health_registry: None,
            memory_limiter: None,
        }
    }

    /// Gets a mutable reference to the build state.
    ///
    /// # Panics
    ///
    /// Panics if the blueprint has already been initialized (its build state having been consumed).
    fn state_mut(&mut self) -> &mut TopologyBuildState {
        self.build_state
            .get_mut()
            .expect("topology blueprint mutex poisoned")
            .as_mut()
            .expect("topology blueprint already initialized")
    }

    /// Returns a read-only snapshot of the topology graph.
    ///
    /// The snapshot is intended for diagnostics and documentation tooling. It validates the graph before exporting so
    /// callers see the same topology-shape errors that would prevent the blueprint from running.
    ///
    /// # Errors
    ///
    /// If the topology graph is invalid, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the blueprint mutex is poisoned.
    pub fn snapshot(&self) -> Result<TopologySnapshot, GenericError> {
        let guard = self.build_state.lock().expect("topology blueprint mutex poisoned");
        let state = guard
            .as_ref()
            .ok_or_else(|| generic_error!("Topology has already been initialized and cannot be snapshotted."))?;

        state
            .graph
            .validate()
            .error_context("Failed to validate topology graph before snapshot.")?;

        Ok(state.graph.snapshot())
    }

    /// Sets the capacity of interconnects in the topology.
    ///
    /// Interconnects are used to connect components to one another. Once their capacity is reached, no more items can be sent
    /// through until in-flight items are processed. This will apply backpressure to the upstream components. Raising or lowering
    /// the capacity allows trading off throughput at the expense of memory usage.
    ///
    /// Defaults to 128.
    pub fn with_interconnect_capacity(&mut self, capacity: NonZeroUsize) -> &mut Self {
        self.state_mut().set_interconnect_capacity(capacity);
        self
    }

    /// Sets how long the topology waits for components to stop during graceful shutdown.
    ///
    /// Defaults to 30 seconds.
    pub fn with_shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.state_mut().shutdown_timeout = timeout;
        self
    }

    /// Sets the health registry used when the topology is spawned.
    ///
    /// This must be set before the blueprint is added to a supervisor; initialization fails otherwise.
    pub fn with_health_registry(&mut self, health_registry: HealthRegistry) -> &mut Self {
        self.health_registry = Some(health_registry);
        self
    }

    /// Sets the memory limiter used when the topology is spawned.
    ///
    /// This must be set before the blueprint is added to a supervisor; initialization fails otherwise.
    pub fn with_memory_limiter(&mut self, memory_limiter: MemoryLimiter) -> &mut Self {
        self.memory_limiter = Some(memory_limiter);
        self
    }

    /// Sets a readiness signal that must resolve before the topology starts its components.
    ///
    /// When set, the topology is still built up front during initialization, but its components are not spawned until
    /// the given future resolves (or the topology is asked to shut down first). This is used to defer the topology from
    /// processing data until its dependencies -- such as the environment provider's metadata collectors -- are ready.
    pub fn with_environment_readiness<F>(&mut self, ready: F) -> &mut Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.state_mut().environment_ready = Some(Box::pin(ready));
        self
    }

    /// Returns a handle for awaiting the readiness of the topology once it's running.
    ///
    /// This handle depends on observing the readiness of the individual topology components, and so must be called after
    /// [`with_health_registry`][Self::with_health_registry].
    ///
    /// # Panics
    ///
    /// Panics if the health registry has not been set, or if the blueprint has already been initialized.
    pub fn topology_ready(&mut self) -> TopologyReady {
        let health_registry = self
            .health_registry
            .clone()
            .expect("health registry must be set before acquiring a topology readiness handle");
        let component_prefix = format!("{}.", super::health_component_root(&self.name));

        let (registered_tx, registered_rx) = oneshot::channel();
        self.state_mut().ready_signal = Some(registered_tx);

        TopologyReady {
            registered_rx,
            health_registry,
            component_prefix,
        }
    }

    /// Configures the topology to use the ambient Tokio runtime for component subtasks.
    ///
    /// Component subtasks will be spawned on whatever runtime is currently active when the topology is initialized.
    /// This avoids creating a dedicated thread pool, which is useful for resource-constrained environments.
    pub fn with_ambient_worker_pool(&mut self) -> &mut Self {
        self.state_mut().worker_pool_config = WorkerPoolConfiguration::Ambient;
        self
    }

    /// Configures the topology to use an externally provided Tokio runtime for component subtasks.
    ///
    /// Component subtasks will be spawned on the runtime associated with the given handle.
    pub fn with_explicit_worker_pool(&mut self, handle: Handle) -> &mut Self {
        self.state_mut().worker_pool_config = WorkerPoolConfiguration::Explicit(handle);
        self
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
        self.state_mut().add_source(component_id, builder)?;
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
        self.state_mut().add_relay(component_id, builder)?;
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
        self.state_mut().add_decoder(component_id, builder)?;
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
        self.state_mut().add_transform(component_id, builder)?;
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
        self.state_mut().add_destination(component_id, builder)?;
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
        self.state_mut().add_encoder(component_id, builder)?;
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
        self.state_mut().add_forwarder(component_id, builder)?;
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
        self.state_mut()
            .connect_components(upstream_output_component_ids, downstream_component_ids)?;
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
        self.state_mut().connect_components_in_order(ordered_component_ids)?;
        Ok(self)
    }
}

/// A handle for awaiting the readiness of a running topology.
pub struct TopologyReady {
    registered_rx: oneshot::Receiver<()>,
    health_registry: HealthRegistry,
    component_prefix: String,
}

impl TopologyReady {
    /// Waits until the topology has registered its components and all of them have reported ready.
    ///
    /// Returns `true` once the topology is fully ready, or `false` if the topology was torn down before it finished
    /// registering its components. The topology might be torn down before readiness is achieved if shutdown is
    /// requested while still waiting on an upstream dependency such as the environment provider.
    pub async fn wait(self) -> bool {
        // First, wait for the topology to actually register its components in the health registry.
        //
        // If we didn't do this, we could observe `all_ready_matching` return immediately (due to no matching components)
        // which would not correctly represent the topology being ready.
        if self.registered_rx.await.is_err() {
            return false;
        }

        // Now wait for all registered topology components to actually become ready.
        self.health_registry
            .all_ready_matching(|name| name.starts_with(&self.component_prefix))
            .await;

        true
    }
}

impl TopologyBuildState {
    fn set_interconnect_capacity(&mut self, capacity: NonZeroUsize) {
        self.interconnect_capacity = capacity;
        self.recalculate_bounds();
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

    fn add_source<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_relay<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_decoder<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_transform<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_destination<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_encoder<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn add_forwarder<I, B>(&mut self, component_id: I, builder: B) -> Result<(), GenericError>
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

        Ok(())
    }

    fn connect_components<MS, SI, MD, DI>(
        &mut self, upstream_output_component_ids: SI, downstream_component_ids: DI,
    ) -> Result<(), GenericError>
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

        Ok(())
    }

    fn connect_components_in_order<IT, I>(&mut self, ordered_component_ids: IT) -> Result<(), GenericError>
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

        Ok(())
    }

    /// Builds the topology.
    ///
    /// # Errors
    ///
    /// If any of the components couldn't be built, an error is returned.
    async fn build(mut self, name: String) -> Result<BuiltTopology, GenericError> {
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
            name,
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
            self.worker_pool_config,
        ))
    }
}

#[async_trait]
impl Supervisable for TopologyBlueprint {
    fn name(&self) -> &str {
        &self.name
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // Set an infinitely long (effectively) graceful shutdown timeout because we enforce our _own_ realistic graceful
        // shutdown as part of the supervisor future we generate.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    async fn initialize(&self, shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // Consume the build state.
        //
        // Topologies currently can't be initialized more than once.
        let mut build_state = self
            .build_state
            .lock()
            .expect("topology blueprint mutex poisoned")
            .take()
            .ok_or_else(|| generic_error!("Topology has already been initialized and cannot be run more than once."))?;

        let health_registry = self
            .health_registry
            .clone()
            .ok_or_else(|| generic_error!("Topology blueprint is missing its health registry."))?;
        let memory_limiter = self
            .memory_limiter
            .clone()
            .ok_or_else(|| generic_error!("Topology blueprint is missing its memory limiter."))?;

        let dataspace = DataspaceRegistry::try_current()
            .ok_or_else(|| generic_error!("Topology must be initialized within a supervised process context."))?;

        // Build our topology components.
        //
        // This creates the topology components but does not actually spawn them or run them in any way.
        //
        // We do this outside of the supervisor future to ensure that we fail during initialization, which bubbles up as
        // a non-restartable error that ultimately leads to the process exiting. This is the desired behavior at present
        // time, but maybe change in the future.
        let environment_ready = build_state.environment_ready.take();
        let ready_signal = build_state.ready_signal.take();
        let shutdown_timeout = build_state.shutdown_timeout;
        let built = build_state.build(self.name.clone()).await?;

        Ok(Box::pin(async move {
            pin!(shutdown);

            // If a readiness signal was provided, wait for it before spawning the components, but remain responsive to
            // shutdown so we exit promptly if asked to stop before we've started.
            if let Some(environment_ready) = environment_ready {
                select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = environment_ready => {},
                }
            }

            let mut running = built.spawn_inner(&health_registry, memory_limiter, dataspace).await?;

            // Signal that the topology has registered all of its components in the health registry, so any readiness
            // handle can begin waiting on those components. We send this only after `spawn_inner` so readiness can't be
            // observed before the topology's components exist.
            if let Some(ready_signal) = ready_signal {
                let _ = ready_signal.send(());
            }

            let mut topology_failed = false;
            select! {
                // The supervisor requested shutdown.
                _ = &mut shutdown => {
                    info!("Topology received shutdown signal. Shutting down...");
                },

                // A component finished before shutdown was requested, which we treat as a failure of the topology.
                _ = running.wait_for_unexpected_finish() => {
                    error!("Topology component unexpectedly finished. Shutting down...");
                    topology_failed = true;
                },
            }

            // Trigger graceful shutdown and wait for all components to stop.
            let shutdown_result = running.shutdown_with_timeout(shutdown_timeout).await;
            match (shutdown_result, topology_failed) {
                (Ok(()), false) => Ok(()),
                (Ok(()), true) => Err(generic_error!(
                    "Topology shut down after a component unexpectedly finished."
                )),
                (Err(e), _) => Err(e),
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use resource_accounting::{ComponentRegistry, MemoryLimiter};
    use tokio::sync::oneshot;

    use super::{TopologyBlueprint, TopologyReady};
    use crate::{
        data_model::event::EventType,
        health::HealthRegistry,
        runtime::{RestartMode, RestartStrategy, Supervisor, SupervisorError},
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
        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        let outbound_edges = guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .graph
            .get_outbound_directed_edges();

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
    fn snapshot_validates_the_blueprint_graph() {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("in", TestSourceBuilder::default_output(EventType::Metric))
            .expect("should not fail to add source");

        let error = blueprint
            .snapshot()
            .expect_err("disconnected graph should not snapshot");

        let error = format!("{error:?}");
        assert!(error.contains("disconnected components"), "unexpected error: {error}");
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

    /// Builds a connected `source` -> `transform` -> `destination` blueprint using the immediate-exit test components.
    fn connected_blueprint() -> TopologyBlueprint {
        let mut blueprint = blueprint_with_components();
        blueprint
            .connect_components_in_order(["source", "transform", "destination"])
            .expect("should not fail to connect components");
        blueprint
    }

    #[tokio::test]
    async fn topology_failure_shuts_down_supervisor() {
        // The test components all finish immediately, which the topology worker treats as an unexpected component
        // finish -- a topology failure. With a restart intensity of zero, that must fail the supervisor (and, in the
        // real binary, exit the process).
        let mut blueprint = connected_blueprint();
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop());

        let mut supervisor = Supervisor::new("test-topology")
            .expect("should not fail to create supervisor")
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)));
        supervisor.add_worker(blueprint);

        let (_tx, rx) = oneshot::channel::<()>();
        let result = tokio::time::timeout(Duration::from_secs(5), supervisor.run_with_shutdown(rx))
            .await
            .expect("supervisor should exit promptly");

        assert!(matches!(result, Err(SupervisorError::Shutdown)));
    }

    #[tokio::test]
    async fn topology_cannot_be_initialized_more_than_once() {
        // A topology can only be initialized once. Under the default restart strategy (which allows one restart), the
        // topology fails at runtime (components finish immediately), the supervisor attempts to restart it, and the
        // second initialization fails because the blueprint's build state was already consumed. That surfaces as a
        // non-restartable initialization failure.
        let mut blueprint = connected_blueprint();
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop());

        let mut supervisor = Supervisor::new("test-topology").expect("should not fail to create supervisor");
        supervisor.add_worker(blueprint);

        let (_tx, rx) = oneshot::channel::<()>();
        let result = tokio::time::timeout(Duration::from_secs(5), supervisor.run_with_shutdown(rx))
            .await
            .expect("supervisor should exit promptly");

        assert!(matches!(result, Err(SupervisorError::FailedToInitialize { .. })));
    }

    #[tokio::test]
    async fn topology_waits_for_environment_readiness_before_starting() {
        // The topology must not start its components until the environment readiness signal resolves. We provide a
        // signal that never resolves, then trigger shutdown: the topology should exit cleanly without ever spawning
        // its components (which would otherwise finish immediately and fail the supervisor), and the supervisor should
        // shut down successfully.
        let mut blueprint = connected_blueprint();
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_environment_readiness(std::future::pending::<()>());

        let mut supervisor = Supervisor::new("test-topology").expect("should not fail to create supervisor");
        supervisor.add_worker(blueprint);

        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move { supervisor.run_with_shutdown(rx).await });

        // Give the supervisor a moment to start and reach the readiness gate, then trigger shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).expect("should send shutdown signal");

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervisor should exit promptly")
            .expect("supervisor task should not panic");

        assert!(result.is_ok(), "supervisor should shut down cleanly, got: {:?}", result);
    }

    #[test]
    fn topology_ready_waits_for_registration_before_checking_readiness() {
        use tokio_test::{assert_pending, assert_ready, task::spawn};

        let health_registry = HealthRegistry::new();

        // Simulate an unrelated subsystem that has already registered and become ready. A naive readiness check against
        // the shared registry could resolve immediately here, even though the topology hasn't registered anything yet.
        let mut other = health_registry
            .register_component("env_provider.workload.foo")
            .expect("should register component");
        other.mark_ready();

        let (registered_tx, registered_rx) = oneshot::channel();
        let topology_ready = TopologyReady {
            registered_rx,
            health_registry: health_registry.clone(),
            component_prefix: "topology.primary.".to_string(),
        };

        let mut wait = spawn(topology_ready.wait());

        // Despite no topology components being registered yet, `wait` must not resolve: it's gated on the registration
        // signal, which is precisely what prevents a false-ready observation.
        assert_pending!(wait.poll());

        // Now register a topology component (as the topology does when it spawns), but leave it not-ready.
        let mut source = health_registry
            .register_component("topology.primary.sources.in")
            .expect("should register component");

        // Fire the registration signal. `wait` advances to the scoped readiness check, which is still pending because
        // the topology component hasn't reported ready.
        registered_tx.send(()).expect("receiver should be alive");
        assert_pending!(wait.poll());

        // Once the topology component reports ready, `wait` resolves to `true`.
        source.mark_ready();
        assert!(assert_ready!(wait.poll()));
    }

    #[tokio::test]
    async fn topology_ready_returns_false_when_torn_down_before_registration() {
        let health_registry = HealthRegistry::new();

        let (registered_tx, registered_rx) = oneshot::channel::<()>();
        let topology_ready = TopologyReady {
            registered_rx,
            health_registry,
            component_prefix: "topology.primary.".to_string(),
        };

        // Drop the sender without ever signaling, as happens when the topology is torn down before it registers its
        // components. `wait` should report that readiness will never be reached.
        drop(registered_tx);

        assert!(!topology_ready.wait().await);
    }
}

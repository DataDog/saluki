#[cfg(any(test, feature = "test-util"))]
use std::sync::Arc;
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Mutex,
    time::Duration,
};

use async_trait::async_trait;
use saluki_common::resource_tracking::Track as _;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use snafu::Snafu;
use tokio::{pin, runtime::Handle, select, sync::oneshot};
use tracing::info;

use super::{
    built::{AdmissionPointBinding, BuiltTopology, WorkerPoolConfiguration, DEFAULT_WORKER_POOL_THREADS},
    graph::{DataType, Graph, GraphError},
    snapshot::{AdmissionPointSnapshot, BlueprintSnapshot},
    ComponentId, ComponentOutputId,
};
use crate::accounting::{ComponentRegistry, MemoryLimiter, UsageExpr};
#[cfg(any(test, feature = "test-util"))]
use crate::accounting::{MemoryBounds, MemoryBoundsBuilder};
#[cfg(any(test, feature = "test-util"))]
use crate::{
    components::encoders::Encoder,
    data_model::{event::EventType, payload::PayloadType},
};
use crate::{
    components::{
        decoders::DecoderBuilder, destinations::DestinationBuilder, encoders::EncoderBuilder,
        forwarders::ForwarderBuilder, relays::RelayBuilder, sources::SourceBuilder, transforms::TransformBuilder,
        BuildContext, ComponentContext, ComponentType,
    },
    data_model::event::Event,
    execution::{AdmissionPointConfig, Stamper, MAX_HEARTBEAT_PERIOD, MIN_HEARTBEAT_PERIOD},
    health::HealthRegistry,
    runtime::{
        state::{DataspaceRegistry, ResourceRegistry},
        InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture,
    },
    support::SubsystemIdentifier,
    topology::{ids::AsComponentIds, topology_identifier, EventsBuffer, LaneEdge, DEFAULT_EVENTS_BUFFER_CAPACITY},
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
    resource_registry: Option<ResourceRegistry>,
}

/// The consumable build state of a [`TopologyBlueprint`].
///
/// This is taken out of the blueprint when it's first initialized, at which point the topology is built and spawned.
struct TopologyBuildState {
    topology_id: SubsystemIdentifier,
    graph: Graph,
    sources: HashMap<ComponentId, Box<dyn SourceBuilder + Send>>,
    relays: HashMap<ComponentId, Box<dyn RelayBuilder + Send>>,
    decoders: HashMap<ComponentId, Box<dyn DecoderBuilder + Send>>,
    transforms: HashMap<ComponentId, Box<dyn TransformBuilder + Send>>,
    destinations: HashMap<ComponentId, Box<dyn DestinationBuilder + Send>>,
    encoders: HashMap<ComponentId, Box<dyn EncoderBuilder + Send>>,
    forwarders: HashMap<ComponentId, Box<dyn ForwarderBuilder + Send>>,
    admission_points: Vec<AdmissionPointBinding>,
    component_registry: ComponentRegistry,
    interconnect_capacity: NonZeroUsize,
    input_capacities: HashMap<ComponentId, NonZeroUsize>,
    shutdown_timeout: Duration,
    worker_pool_config: WorkerPoolConfiguration,
    environment_ready: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    ready_signal: Option<oneshot::Sender<()>>,
}

impl TopologyBlueprint {
    /// Creates an empty `TopologyBlueprint` with the given name.
    pub fn new(name: &str, component_registry: &ComponentRegistry) -> Self {
        let topology_id = topology_identifier(name);
        let component_registry = component_registry.clone();

        let build_state = TopologyBuildState {
            topology_id,
            graph: Graph::default(),
            sources: HashMap::new(),
            relays: HashMap::new(),
            decoders: HashMap::new(),
            transforms: HashMap::new(),
            destinations: HashMap::new(),
            encoders: HashMap::new(),
            forwarders: HashMap::new(),
            admission_points: Vec::new(),
            component_registry,
            interconnect_capacity: super::DEFAULT_INTERCONNECT_CAPACITY,
            input_capacities: HashMap::new(),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            worker_pool_config: WorkerPoolConfiguration::Dedicated {
                threads: DEFAULT_WORKER_POOL_THREADS,
            },
            environment_ready: None,
            ready_signal: None,
        };

        Self {
            name: name.to_string(),
            build_state: Mutex::new(Some(build_state)),
            health_registry: None,
            memory_limiter: None,
            resource_registry: None,
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

    /// Sets the capacity of interconnects in the topology.
    ///
    /// Interconnects are used to connect components to one another. Once their capacity is reached, no more items can be sent
    /// through until in-flight items are processed. This will apply backpressure to the upstream components. Raising or lowering
    /// the capacity allows trading off throughput at the expense of memory usage.
    ///
    /// This is the capacity of every component's input, except for the components given one of their own with
    /// [`with_component_input_capacity`][Self::with_component_input_capacity].
    ///
    /// Defaults to 128.
    pub fn with_interconnect_capacity(&mut self, capacity: NonZeroUsize) -> &mut Self {
        self.state_mut().set_interconnect_capacity(capacity);
        self
    }

    /// Returns the capacity of interconnects in the topology, which
    /// [`with_interconnect_capacity`][Self::with_interconnect_capacity] sets.
    ///
    /// # Panics
    ///
    /// Panics if the blueprint has already been initialized.
    pub fn interconnect_capacity(&self) -> NonZeroUsize {
        self.build_state
            .lock()
            .expect("topology blueprint mutex poisoned")
            .as_ref()
            .expect("topology blueprint already initialized")
            .interconnect_capacity
    }

    /// Sets the capacity of one component's input interconnect, in place of the
    /// [interconnect capacity][Self::with_interconnect_capacity] of the topology.
    ///
    /// A component's input interconnect holds what upstream components send it until it reads it: once it is full,
    /// they wait. A larger capacity lets a component that does the work of several others, each of which had an input
    /// interconnect of its own, take as much from upstream before they wait as those components together did. The
    /// capacity stays the component's when the interconnect capacity of the topology changes, and when the component
    /// is replaced.
    ///
    /// A transform with several input lanes ([`TransformBuilder::input_lanes`]) has one input interconnect per lane,
    /// each of this capacity.
    ///
    /// The memory bounds of the topology count the component's interconnects at this capacity. Only transforms and
    /// destinations, whose input interconnects the bounds account for, can have a capacity of their own.
    ///
    /// # Errors
    ///
    /// If the blueprint has no component `component_id`, or if the component is not a transform or a destination, an
    /// error is returned and the blueprint is unchanged.
    pub fn with_component_input_capacity<I>(
        &mut self, component_id: I, capacity: NonZeroUsize,
    ) -> Result<&mut Self, GenericError>
    where
        I: AsRef<str>,
    {
        self.state_mut()
            .set_component_input_capacity(component_id.as_ref(), capacity)?;
        Ok(self)
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

    /// Sets the resource registry used when the topology is built and spawned.
    ///
    /// Components acquire scarce resources, such as bound network sockets, from this registry rather than creating
    /// them directly, so that the registry outlives them and can hand the same resource back when a component is
    /// rebuilt.
    ///
    /// This must be set before the blueprint is added to a supervisor; initialization fails otherwise.
    pub fn with_resource_registry(&mut self, resource_registry: ResourceRegistry) -> &mut Self {
        self.resource_registry = Some(resource_registry);
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
        let component_root = super::topology_identifier(&self.name);

        let (registered_tx, registered_rx) = oneshot::channel();
        self.state_mut().ready_signal = Some(registered_tx);

        TopologyReady {
            registered_rx,
            health_registry,
            component_root,
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

    /// Configures the topology to build a dedicated, multi-threaded Tokio runtime with `threads` worker threads for
    /// component subtasks, as it does by default with 8.
    ///
    /// A process whose components spawn more compute-bound subtasks than 8 threads serve, such as one encoder stream
    /// per output, sizes the pool from its cores.
    pub fn with_dedicated_worker_pool(&mut self, threads: NonZeroUsize) -> &mut Self {
        self.state_mut().worker_pool_config = WorkerPoolConfiguration::Dedicated { threads };
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
    /// If any of the upstream or downstream component IDs are invalid or don't exist, if the data types between one
    /// of the upstream/downstream component pairs is incompatible, or if a downstream component has more than one input
    /// lane, which [`connect_components_with`][Self::connect_components_with] connects, an error is returned.
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

    /// Connects one upstream component output to one downstream component, naming how the sender picks the lane of
    /// each buffer when the downstream component has lanes ([`TransformBuilder::input_lanes`]).
    ///
    /// An edge into a component with one lane leads to that lane, whatever `edge` names, as
    /// [`connect_components`][Self::connect_components] connects it. Every edge into a component with more than one
    /// lane is connected this way: [`connect_components`][Self::connect_components] refuses it. A send names one
    /// route, so an output can route by key, by affinity, or by spreading to the lanes of one component only, besides
    /// any number of components with one lane and of lanes it is fixed to (see
    /// [`Dispatcher::dispatch_named_to_lane`][crate::topology::interconnect::Dispatcher::dispatch_named_to_lane]).
    ///
    /// # Errors
    ///
    /// If either component ID is invalid or does not exist, if their data types are incompatible, if `edge` is fixed
    /// to a lane the downstream component does not have, or if `edge` routes to the lanes of the downstream component
    /// while the output already routes to another component's lanes, an error is returned.
    pub fn connect_components_with(&mut self, from: &str, to: &str, edge: LaneEdge) -> Result<&mut Self, GenericError> {
        self.state_mut().connect_components_with(from, to, edge)?;
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

    /// Marks a component output as an admission point.
    ///
    /// `output` is a component output ID: `"<component>"` for a component's default output, or
    /// `"<component>.<output>"` for a named output. The component must already be in the blueprint.
    ///
    /// When the topology runs, every buffer the component sends on that output is stamped with an admission time, the
    /// point's identifier and clock epoch, and a generation from the point's schedule, according to the point's
    /// [mode][crate::execution::AdmissionMode]. If the configuration enables heartbeats, a heartbeat driver runs
    /// beside the component for as long as the component runs, sending heartbeats to the configuration's
    /// [heartbeat recipients][AdmissionPointConfig::with_heartbeat_recipients], or to every component the output feeds
    /// if it lists none. If the configuration names a [registry][AdmissionPointConfig::with_registry], the point
    /// registers in it when the topology starts. Outputs that are not admission points send buffers unchanged.
    ///
    /// # Errors
    ///
    /// If `output` is not a valid component output ID, if the component or output does not exist, if the output emits
    /// payloads rather than events, if the output is already an admission point, if another output already uses the
    /// configuration's admission point ID, or if the heartbeat period is shorter than
    /// [`MIN_HEARTBEAT_PERIOD`][crate::execution::MIN_HEARTBEAT_PERIOD] (1 ms) or longer than
    /// [`MAX_HEARTBEAT_PERIOD`][crate::execution::MAX_HEARTBEAT_PERIOD] (1 hour), an error is returned. Connections
    /// may be made in any order, so heartbeat recipients are checked when the topology is built, which fails if the
    /// output does not feed every recipient directly.
    pub fn with_admission_point<O>(
        &mut self, output: O, config: AdmissionPointConfig,
    ) -> Result<&mut Self, GenericError>
    where
        O: AsRef<str>,
    {
        self.state_mut().add_admission_point(output.as_ref(), config)?;
        Ok(self)
    }

    /// Replaces the builder of the source `component_id` with `builder`, keeping the source's place in the blueprint.
    ///
    /// The replacement keeps every connection and admission point of the component it replaces, and its memory bounds
    /// replace the replaced builder's. The interconnect bounds of the topology are recalculated, since they depend on
    /// what a transform declares about its input. This lets a test run a blueprint that a builder function assembled,
    /// with some components swapped for test doubles, such as a scripted source in place of one that listens on a
    /// socket.
    ///
    /// # Errors
    ///
    /// If the blueprint has no source `component_id`, or if `builder` declares other outputs than the source it
    /// replaces, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn replace_source<B>(&mut self, component_id: &str, builder: B) -> Result<&mut Self, GenericError>
    where
        B: SourceBuilder + Send + 'static,
    {
        let state = self.state_mut();
        let id = state.check_replacement(ComponentType::Source, |graph| graph.add_source(component_id, &builder))?;
        state.replace_bounds(ComponentType::Source, &id, &builder);
        state.sources.insert(id, Box::new(builder));
        state.recalculate_bounds();
        Ok(self)
    }

    /// Replaces the builder of the transform `component_id` with `builder`, keeping the transform's place in the
    /// blueprint, as [`replace_source`][Self::replace_source] does for a source.
    ///
    /// # Errors
    ///
    /// If the blueprint has no transform `component_id`, or if `builder` declares another input or other outputs than
    /// the transform it replaces, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn replace_transform<B>(&mut self, component_id: &str, builder: B) -> Result<&mut Self, GenericError>
    where
        B: TransformBuilder + Send + 'static,
    {
        let state = self.state_mut();
        let id = state.check_replacement(ComponentType::Transform, |graph| {
            graph.add_transform(component_id, &builder)
        })?;
        state.replace_bounds(ComponentType::Transform, &id, &builder);
        state.transforms.insert(id, Box::new(builder));
        state.recalculate_bounds();
        Ok(self)
    }

    /// Replaces the builder of the destination `component_id` with `builder`, keeping the destination's place in the
    /// blueprint, as [`replace_source`][Self::replace_source] does for a source.
    ///
    /// # Errors
    ///
    /// If the blueprint has no destination `component_id`, or if `builder` declares another input than the
    /// destination it replaces, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn replace_destination<B>(&mut self, component_id: &str, builder: B) -> Result<&mut Self, GenericError>
    where
        B: DestinationBuilder + Send + 'static,
    {
        let state = self.state_mut();
        let id = state.check_replacement(ComponentType::Destination, |graph| {
            graph.add_destination(component_id, &builder)
        })?;
        state.replace_bounds(ComponentType::Destination, &id, &builder);
        state.destinations.insert(id, Box::new(builder));
        state.recalculate_bounds();
        Ok(self)
    }

    /// Replaces the builder of the encoder `component_id` with `builder`, keeping the encoder's place in the
    /// blueprint, as [`replace_source`][Self::replace_source] does for a source.
    ///
    /// # Errors
    ///
    /// If the blueprint has no encoder `component_id`, or if `builder` declares another input or output than the
    /// encoder it replaces, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn replace_encoder<B>(&mut self, component_id: &str, builder: B) -> Result<&mut Self, GenericError>
    where
        B: EncoderBuilder + Send + 'static,
    {
        let state = self.state_mut();
        let id = state.check_replacement(ComponentType::Encoder, |graph| {
            graph.add_encoder(component_id, &builder)
        })?;
        state.replace_bounds(ComponentType::Encoder, &id, &builder);
        state.encoders.insert(id, Box::new(builder));
        state.recalculate_bounds();
        Ok(self)
    }

    /// Wraps the encoder `component_id` in the encoder that `wrap` returns when the topology builds it, keeping the
    /// encoder's builder, place, and memory bounds.
    ///
    /// `wrap` receives the encoder the blueprint's builder built, so a test can keep a real encoder and change only how
    /// it runs, such as observing what it receives before running it.
    ///
    /// # Errors
    ///
    /// If the blueprint has no encoder `component_id`, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn wrap_encoder<F>(&mut self, component_id: &str, wrap: F) -> Result<&mut Self, GenericError>
    where
        F: Fn(Box<dyn Encoder + Send>) -> Box<dyn Encoder + Send> + Send + Sync + 'static,
    {
        let state = self.state_mut();
        let (id, inner) = ComponentId::try_from(component_id)
            .ok()
            .and_then(|id| state.encoders.remove_entry(&id))
            .ok_or_else(|| {
                generic_error!(
                    "Cannot wrap encoder '{}': the blueprint has no such encoder. Add it before wrapping it.",
                    component_id
                )
            })?;
        let wrapped = WrappedEncoderBuilder {
            inner,
            wrap: Arc::new(wrap),
        };
        state.encoders.insert(id, Box::new(wrapped));
        Ok(self)
    }

    /// Replaces the builder of the forwarder `component_id` with `builder`, keeping the forwarder's place in the
    /// blueprint, as [`replace_source`][Self::replace_source] does for a source.
    ///
    /// # Errors
    ///
    /// If the blueprint has no forwarder `component_id`, or if `builder` declares another input than the forwarder it
    /// replaces, an error is returned and the blueprint is unchanged.
    #[cfg(any(test, feature = "test-util"))]
    pub fn replace_forwarder<B>(&mut self, component_id: &str, builder: B) -> Result<&mut Self, GenericError>
    where
        B: ForwarderBuilder + Send + 'static,
    {
        let state = self.state_mut();
        let id = state.check_replacement(ComponentType::Forwarder, |graph| {
            graph.add_forwarder(component_id, &builder)
        })?;
        state.replace_bounds(ComponentType::Forwarder, &id, &builder);
        state.forwarders.insert(id, Box::new(builder));
        state.recalculate_bounds();
        Ok(self)
    }

    /// Returns a read-only description of the blueprint's structure.
    ///
    /// The snapshot lists every component with its type and declared outputs, every connection, and every output
    /// marked as an admission point. It allows comparing the structure of a blueprint with another description of a
    /// topology, such as a processing plan, without building or running it.
    ///
    /// Returns `None` once the blueprint has been initialized, since initialization consumes its structure.
    pub fn snapshot(&self) -> Option<BlueprintSnapshot> {
        let guard = self.build_state.lock().expect("topology blueprint mutex poisoned");
        guard.as_ref().map(TopologyBuildState::snapshot)
    }
}

/// Wraps a built encoder in another.
#[cfg(any(test, feature = "test-util"))]
type EncoderWrapper = Arc<dyn Fn(Box<dyn Encoder + Send>) -> Box<dyn Encoder + Send> + Send + Sync>;

/// An encoder builder that builds what the builder it wraps does, and wraps the encoder it builds.
#[cfg(any(test, feature = "test-util"))]
struct WrappedEncoderBuilder {
    inner: Box<dyn EncoderBuilder + Send>,
    wrap: EncoderWrapper,
}

#[cfg(any(test, feature = "test-util"))]
impl EncoderBuilder for WrappedEncoderBuilder {
    fn input_event_type(&self) -> EventType {
        self.inner.input_event_type()
    }

    fn output_payload_type(&self) -> PayloadType {
        self.inner.output_payload_type()
    }

    // Written out rather than with `async_trait`: the wrapped builder is `Send` but not `Sync`, so an `async fn` here
    // could not hold `&self` across the build. The wrapped builder's own future is `Send`, and nothing else borrows
    // `self`.
    fn build<'life0, 'async_trait>(
        &'life0 self, context: BuildContext,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn Encoder + Send>, GenericError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let built = self.inner.build(context);
        let wrap = Arc::clone(&self.wrap);
        Box::pin(async move { Ok(wrap(built.await?)) })
    }
}

#[cfg(any(test, feature = "test-util"))]
impl MemoryBounds for WrappedEncoderBuilder {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.inner.specify_bounds(builder);
    }
}

/// A handle for awaiting the readiness of a running topology.
pub struct TopologyReady {
    registered_rx: oneshot::Receiver<()>,
    health_registry: HealthRegistry,
    component_root: SubsystemIdentifier,
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
        self.health_registry.all_ready_under(self.component_root).await;

        true
    }
}

impl TopologyBuildState {
    fn set_interconnect_capacity(&mut self, capacity: NonZeroUsize) {
        self.interconnect_capacity = capacity;
        self.recalculate_bounds();
    }

    fn set_component_input_capacity(&mut self, component_id: &str, capacity: NonZeroUsize) -> Result<(), GenericError> {
        let id = ComponentId::try_from(component_id).map_err(|reason| {
            generic_error!(
                "Cannot set the input capacity of '{}': it is not a valid component ID: {}.",
                component_id,
                reason
            )
        })?;
        match self.graph.component_type(&id) {
            Some(ComponentType::Transform | ComponentType::Destination) => {}
            Some(other) => {
                return Err(generic_error!(
                    "Cannot set the input capacity of {} '{}': only transforms and destinations can have an input \
                     capacity of their own, since the topology's memory bounds account for their input interconnects \
                     only.",
                    other.as_str(),
                    id
                ))
            }
            None => {
                return Err(generic_error!(
                    "Cannot set the input capacity of '{}': the blueprint has no such component. Add the component \
                     before setting its input capacity.",
                    id
                ))
            }
        }
        self.input_capacities.insert(id, capacity);
        self.recalculate_bounds();
        Ok(())
    }

    /// Returns the capacity of a component's input interconnect: its own, or else the interconnect capacity.
    ///
    /// A component with several input lanes has one interconnect of this capacity per lane.
    fn input_capacity(&self, component_id: &ComponentId) -> NonZeroUsize {
        self.input_capacities
            .get(component_id)
            .copied()
            .unwrap_or(self.interconnect_capacity)
    }

    fn recalculate_bounds(&mut self) {
        let mut bounds_builder = self.component_registry.bounds_builder(&self.topology_id);
        let mut bounds_builder = bounds_builder.subcomponent("interconnects");
        bounds_builder.reset();

        // The interconnects of every non-source component: one per input lane, each of the component's capacity, and
        // each holding buffers of at most as many events as the component declares. Only transforms have lanes, or
        // buffers of fewer events than an events buffer holds.
        let interconnects = self
            .transforms
            .iter()
            .map(|(component_id, builder)| {
                let lanes = usize::from(builder.input_lanes().get());
                let events = input_buffer_events(builder.as_ref());
                (self.input_capacity(component_id).get().saturating_mul(lanes), events)
            })
            .chain(
                self.destinations
                    .keys()
                    .map(|component_id| (self.input_capacity(component_id).get(), DEFAULT_EVENTS_BUFFER_CAPACITY)),
            )
            .collect::<Vec<_>>();

        // Adjust the bounds related to interconnects.
        //
        // This deals with the minimum size of the interconnects themselves, since they're bounded and thus allocated
        // up-front. Every non-source component has an interconnect per input lane, of the topology's capacity or of its
        // own.
        let total_interconnect_capacity = interconnects
            .iter()
            .fold(0usize, |total, (buffers, _)| total.saturating_add(*buffers));
        bounds_builder
            .minimum()
            .with_array::<EventsBuffer>("events", total_interconnect_capacity);

        // TODO: Add a minimum subitem for payloads when we have payload interconnects.

        // Adjust the bounds related to event buffers themselves.
        //
        // The struct size of an events buffer includes its batch header, which is stored inline. Heartbeats are empty
        // buffers that allocate no event storage, so they occupy only interconnect slots, which the minimum covers.
        //
        // We calculate the maximum number of event buffers by adding up the total capacity of all non-source
        // components, plus the count of non-destination tasks: sources, decoders, and each lane of each transform. This
        // is the effective upper bound because once all component channels are full, sending tasks can only allocate
        // one more event buffer before being blocked on sending, which is then the effective upper bound.
        //
        // An interconnect whose component declares buffers of fewer events holds that many events per buffer, and is
        // counted apart, so that a component fed with smaller buffers, such as a lane of a partitioned state owner,
        // can hold more of them in the same memory.
        //
        // TODO: Somewhat fragile. Need to revisit this.
        // TODO: Add a firm subitem for payloads when we have payload interconnects.
        let transform_lanes = self
            .transforms
            .values()
            .map(|builder| usize::from(builder.input_lanes().get()))
            .sum::<usize>();
        let max_in_flight_event_buffers = interconnects
            .iter()
            .filter(|(_, events)| *events == DEFAULT_EVENTS_BUFFER_CAPACITY)
            .fold(0usize, |total, (buffers, _)| total.saturating_add(*buffers))
            .saturating_add(self.sources.len() + self.decoders.len() + transform_lanes);

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

        // The interconnects of smaller buffers, by buffer size, smallest first.
        let mut smaller = BTreeMap::<usize, usize>::new();
        for (buffers, events) in interconnects
            .iter()
            .filter(|(_, events)| *events < DEFAULT_EVENTS_BUFFER_CAPACITY)
        {
            let total = smaller.entry(*events).or_default();
            *total = total.saturating_add(*buffers);
        }
        for (events, buffers) in smaller {
            bounds_builder.firm().with_expr(UsageExpr::product(
                "events",
                UsageExpr::constant("in-flight event buffers of fewer events", buffers),
                UsageExpr::sum(
                    "",
                    UsageExpr::struct_size::<EventsBuffer>("events buffer"),
                    UsageExpr::product(
                        "",
                        UsageExpr::struct_size::<Event>("event"),
                        UsageExpr::constant("events per buffer", events),
                    ),
                ),
            ));
        }
    }

    fn add_admission_point(&mut self, output: &str, config: AdmissionPointConfig) -> Result<(), GenericError> {
        let output_id = ComponentOutputId::try_from(output)
            .map_err(|reason| generic_error!("Invalid admission point output '{}': {}.", output, reason))?;

        let (component_type, data_type) = self.graph.get_output(&output_id).map_err(|e| {
            generic_error!(
                "Cannot mark '{}' as an admission point: {}. Add the component, declaring this output, before marking \
                 the output.",
                output_id,
                e
            )
        })?;
        if !matches!(data_type, DataType::Event(_)) {
            return Err(generic_error!(
                "Cannot mark '{}' as an admission point: it is a {} output. Admission points stamp event buffers, so \
                 only event outputs of sources, decoders, and transforms can be admission points.",
                output_id,
                data_type
            ));
        }

        if self.admission_points.iter().any(|binding| binding.output == output_id) {
            return Err(generic_error!(
                "Output '{}' is already an admission point. Each output can have at most one.",
                output_id
            ));
        }
        if config.stamper() == Stamper::Owned {
            return Err(generic_error!(
                "Cannot mark '{}' as an admission point: admission point {} is owned, so the one task that admits \
                 through it stamps it, and no dispatcher does. Give its configuration to that task, or configure a \
                 point a dispatcher stamps.",
                output_id,
                config.id().get()
            ));
        }
        if config.source_clock().is_some() {
            return Err(generic_error!(
                "Cannot mark '{}' as an admission point: admission point {} shares a source clock, which only owned \
                 points do. Configure it as owned, or give it a clock of its own.",
                output_id,
                config.id().get()
            ));
        }
        if config.heartbeat_each_second().is_some() {
            return Err(generic_error!(
                "Cannot mark '{}' as an admission point: admission point {} heartbeats in each second, which only an \
                 owned point's task does. Configure it as owned, or remove the heartbeat in each second.",
                output_id,
                config.id().get()
            ));
        }
        if let Some(existing) = self
            .admission_points
            .iter()
            .find(|binding| binding.config.id() == config.id())
        {
            return Err(generic_error!(
                "Cannot mark '{}' as an admission point: admission point ID {} is already used by output '{}'. Give \
                 every admission point a unique ID.",
                output_id,
                config.id().get(),
                existing.output
            ));
        }
        if let Some(period) = config
            .heartbeat()
            .filter(|period| !(MIN_HEARTBEAT_PERIOD..=MAX_HEARTBEAT_PERIOD).contains(period))
        {
            return Err(generic_error!(
                "The heartbeat period of admission point '{}' is {:?}, outside the allowed range of {:?} through {:?}. \
                 Use a period within that range, or leave heartbeats disabled.",
                output_id,
                period,
                MIN_HEARTBEAT_PERIOD,
                MAX_HEARTBEAT_PERIOD
            ));
        }

        self.admission_points.push(AdmissionPointBinding {
            output: output_id,
            component_type,
            config,
        });

        Ok(())
    }

    /// Checks that every heartbeat recipient of every admission point is a component its output feeds directly.
    fn check_heartbeat_recipients(&self) -> Result<(), GenericError> {
        let (_, edges) = self.graph.snapshot(&|component_id| self.input_capacity(component_id));
        for binding in &self.admission_points {
            let unfed = binding.config.heartbeat_recipients().iter().find(|recipient| {
                !edges
                    .iter()
                    .any(|edge| edge.from() == &binding.output && edge.to() == *recipient)
            });
            if let Some(recipient) = unfed {
                return Err(generic_error!(
                    "Admission point '{}' sends heartbeats to '{}', which the output does not feed. List only \
                     components connected to the output as heartbeat recipients.",
                    binding.output,
                    recipient
                ));
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> BlueprintSnapshot {
        let (components, edges) = self.graph.snapshot(&|component_id| self.input_capacity(component_id));
        let admission_points = self
            .admission_points
            .iter()
            .map(|binding| {
                AdmissionPointSnapshot::new(
                    binding.output.clone(),
                    binding.config.id(),
                    binding.config.mode(),
                    binding.config.heartbeat(),
                )
            })
            .collect();
        BlueprintSnapshot::new(components, edges, admission_points)
    }

    fn component_identity(&self, component_type: ComponentType, component_id: &ComponentId) -> SubsystemIdentifier {
        ComponentContext::new(&self.topology_id, component_id.clone(), component_type).identity()
    }

    /// Checks that a replacement, which `add` adds to an empty graph, has the shape of the component it replaces:
    /// the same type, input, and outputs. Returns the component's ID.
    #[cfg(any(test, feature = "test-util"))]
    fn check_replacement<F>(&self, component_type: ComponentType, add: F) -> Result<ComponentId, GenericError>
    where
        F: FnOnce(&mut Graph) -> Result<ComponentId, GraphError>,
    {
        let mut scratch = Graph::default();
        let id = add(&mut scratch).error_context("Failed to describe the replacement component.")?;
        // A replacement keeps the input capacity of the component it replaces.
        let input_capacity = |component_id: &ComponentId| self.input_capacity(component_id);
        let (replacement, _) = scratch.snapshot(&input_capacity);
        let (components, _) = self.graph.snapshot(&input_capacity);
        let existing = components
            .iter()
            .find(|component| component.id() == &id && component.component_type() == component_type)
            .ok_or_else(|| {
                generic_error!(
                    "Cannot replace {} '{}': the blueprint has no such {}. Add it before replacing it.",
                    component_type.as_str(),
                    id,
                    component_type.as_str()
                )
            })?;
        match replacement.first() {
            Some(replacement) if replacement == existing => Ok(id),
            replacement => Err(generic_error!(
                "Cannot replace {} '{}': the replacement declares {}, but the component it replaces declares {}. A \
                 replacement must declare what it replaces, so that every connection stays valid.",
                component_type.as_str(),
                id,
                replacement.map_or_else(|| "nothing".to_string(), describe_shape),
                describe_shape(existing)
            )),
        }
    }

    /// Replaces the memory bounds of a component with those of its replacement builder.
    #[cfg(any(test, feature = "test-util"))]
    fn replace_bounds(
        &mut self, component_type: ComponentType, component_id: &ComponentId, builder: &dyn MemoryBounds,
    ) {
        let identity = self.component_identity(component_type, component_id);
        let mut bounds_builder = self.component_registry.bounds_builder(&identity);
        bounds_builder.reset();
        builder.specify_bounds(&mut bounds_builder);
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

        let identity = self.component_identity(ComponentType::Source, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.sources.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Relay, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.relays.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Decoder, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.decoders.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Transform, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.transforms.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Destination, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.destinations.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Encoder, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.encoders.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

        let identity = self.component_identity(ComponentType::Forwarder, &component_id);
        builder.specify_bounds(&mut self.component_registry.bounds_builder(&identity));

        let _ = self.forwarders.insert(component_id, Box::new(builder));

        // Only once the component is counted do the topology's bounds include its interconnect and buffers.
        self.recalculate_bounds();

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

    fn connect_components_with(&mut self, from: &str, to: &str, edge: LaneEdge) -> Result<(), GenericError> {
        self.graph
            .add_lane_edge(from, to, edge)
            .error_context("Failed to add component connection to topology graph.")?;
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
    /// If the graph is invalid, if an admission point lists a heartbeat recipient its output does not feed, or if any
    /// of the components couldn't be built, an error is returned.
    async fn build(self, name: String, resource_registry: &ResourceRegistry) -> Result<BuiltTopology, GenericError> {
        self.graph.validate().error_context("Failed to build topology graph.")?;
        self.check_heartbeat_recipients()?;

        let mut sources = HashMap::new();
        for (id, builder) in self.sources {
            let component_context = ComponentContext::source(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let source = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build source '{}'.", id))?;

            sources.insert(component_context, (source, self.component_registry.clone()));
        }

        let mut relays = HashMap::new();
        for (id, builder) in self.relays {
            let component_context = ComponentContext::relay(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let relay = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build relay '{}'.", id))?;

            relays.insert(component_context, (relay, self.component_registry.clone()));
        }

        let mut decoders = HashMap::new();
        for (id, builder) in self.decoders {
            let component_context = ComponentContext::decoder(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let decoder = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build decoder '{}'.", id))?;

            decoders.insert(component_context, (decoder, self.component_registry.clone()));
        }

        // A transform whose input holds buffers of fewer events than an events buffer checks, in a debug build, that
        // its senders keep to that, since the bounds count no more per buffer.
        let input_buffer_events = self
            .transforms
            .iter()
            .map(|(id, builder)| (id.clone(), input_buffer_events(builder.as_ref())))
            .filter(|(_, events)| *events < DEFAULT_EVENTS_BUFFER_CAPACITY)
            .collect::<HashMap<_, _>>();
        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let component_context = ComponentContext::transform(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let transform = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build transform '{}'.", id))?;

            transforms.insert(component_context, (transform, self.component_registry.clone()));
        }

        // A destination with an input gate is skipped by its senders while the gate is closed.
        let input_gates = self
            .destinations
            .iter()
            .filter_map(|(id, builder)| builder.input_gate().map(|gate| (id.clone(), gate)))
            .collect::<HashMap<_, _>>();
        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let component_context = ComponentContext::destination(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let destination = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build destination '{}'.", id))?;

            destinations.insert(component_context, (destination, self.component_registry.clone()));
        }

        let mut encoders = HashMap::new();
        for (id, builder) in self.encoders {
            let component_context = ComponentContext::encoder(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let encoder = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build encoder '{}'.", id))?;

            encoders.insert(component_context, (encoder, self.component_registry.clone()));
        }

        let mut forwarders = HashMap::new();
        for (id, builder) in self.forwarders {
            let component_context = ComponentContext::forwarder(&self.topology_id, id.clone());
            let allocation_token = self
                .component_registry
                .get_resource_group_token(&component_context.identity());
            let forwarder = builder
                .build(BuildContext::new(component_context.clone(), resource_registry.clone()))
                .track_resources(allocation_token)
                .await
                .with_error_context(|| format!("Failed to build forwarder '{}'.", id))?;

            forwarders.insert(component_context, (forwarder, self.component_registry.clone()));
        }

        let topology_token = self.component_registry.get_resource_group_token(&self.topology_id);

        Ok(BuiltTopology::from_parts(
            name,
            self.topology_id,
            self.graph,
            sources,
            relays,
            decoders,
            transforms,
            destinations,
            encoders,
            forwarders,
            self.admission_points,
            topology_token,
            self.interconnect_capacity,
            self.input_capacities,
            input_gates,
            input_buffer_events,
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

        let resource_registry = self
            .resource_registry
            .clone()
            .ok_or_else(|| generic_error!("Topology blueprint is missing its resource registry."))?;

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
        let built = build_state.build(self.name.clone(), &resource_registry).await?;

        Ok(Box::pin(async move {
            pin!(shutdown);

            // If a readiness signal was provided, wait for it before building the components, but remain responsive to
            // shutdown so we exit promptly if asked to stop before we've started.
            if let Some(environment_ready) = environment_ready {
                select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = environment_ready => {},
                }
            }

            // Build the topology supervisor: one dedicated supervisor per component, parented under a single topology
            // supervisor that owns failure detection and graceful shutdown.
            let mut topology_sup = built
                .spawn_inner(&health_registry, memory_limiter, dataspace.clone(), shutdown_timeout)
                .await?;

            // Signal that the topology has registered all of its components in the health registry, so any readiness
            // handle can begin waiting on those components. `spawn_inner` registers every component before returning, so
            // readiness can't be observed before the topology's components exist.
            if let Some(ready_signal) = ready_signal {
                let _ = ready_signal.send(());
            }

            // Run the topology supervisor, forwarding our own shutdown signal into it. We use the internal variant so we
            // can pass down the inherited dataspace (the public `run_with_shutdown` would create a fresh, empty one). The
            // topology supervisor returns `Ok(())` on an intentional shutdown that drained cleanly, and an error if any
            // component exits on its own (how a component failure fails the topology) or if a component had to be
            // forcefully aborted after ignoring graceful shutdown (`SupervisorError::ShutdownTimedOut`). That error is
            // preserved here as a `GenericError` so the root supervisor can still recover the aborted-worker count.
            let (topology_shutdown_trigger, topology_shutdown) = ShutdownHandle::paired();
            let run = topology_sup.run_with_shutdown_inner(topology_shutdown, Some(dataspace));
            pin!(run);

            let mut topology_shutdown_trigger = Some(topology_shutdown_trigger);
            loop {
                select! {
                    result = &mut run => return result.map_err(Into::into),
                    _ = &mut shutdown, if topology_shutdown_trigger.is_some() => {
                        info!("Topology received shutdown signal. Shutting down...");
                        topology_shutdown_trigger.take().expect("present per select guard").shutdown();
                    }
                }
            }
        }))
    }
}

/// Returns the number of events each buffer in the input of the transform `builder` builds holds at most, as the
/// topology counts it: what the builder declares, at least one and at most the capacity of an events buffer.
fn input_buffer_events(builder: &dyn TransformBuilder) -> usize {
    builder.input_buffer_events().clamp(1, DEFAULT_EVENTS_BUFFER_CAPACITY)
}

/// Describes a component's input and outputs, for errors about replacing it.
#[cfg(any(test, feature = "test-util"))]
fn describe_shape(component: &super::ComponentSnapshot) -> String {
    let input = match (component.input(), component.input_lanes()) {
        (None, _) => "no input".to_string(),
        (Some(input), Some(lanes)) if lanes.get() > 1 => format!("input {} on {} lanes", input, lanes),
        (Some(input), _) => format!("input {}", input),
    };
    let outputs = component
        .outputs()
        .iter()
        .map(|output| format!("'{}' ({})", output.id(), output.data_type()))
        .collect::<Vec<_>>();
    if outputs.is_empty() {
        format!("{} and no outputs", input)
    } else {
        format!("{} and outputs {}", input, outputs.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU16, NonZeroUsize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use saluki_error::GenericError;
    use tokio::sync::oneshot;

    use super::{TopologyBlueprint, TopologyReady, WorkerPoolConfiguration, DEFAULT_WORKER_POOL_THREADS};
    use crate::accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter};
    use crate::components::BuildContext;
    use crate::data_model::event::{metric::Metric, Event};
    use crate::data_model::payload::PayloadType;
    use crate::execution::{
        AdmissionMode, AdmissionPointConfig, AdmissionPointId, AdmissionRegistry, AdmissionSchedule, AdmissionTime,
        GenerationId, GenerationToken, ManualWallClock, ScheduleHandle,
    };
    use crate::runtime::{self, state::ResourceRegistry, Name};
    use crate::test_support::wait_until;
    use crate::topology::{ids::get_component_relative_identifier, topology_identifier, ComponentId};
    use crate::topology::{DataType, EventsBuffer, LaneEdge, OutputName, DEFAULT_EVENTS_BUFFER_CAPACITY};
    use crate::{
        components::{
            destinations::{Destination, DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
            transforms::{Transform, TransformBuilder, TransformContext},
            ComponentContext, ComponentType,
        },
        data_model::event::EventType,
        health::HealthRegistry,
        runtime::{
            InitializationError, RestartMode, RestartStrategy, Supervisable, Supervisor, SupervisorError,
            SupervisorFuture,
        },
        support::SubsystemIdentifier,
        topology::{
            test_util::{
                TestDecoderBuilder, TestDestinationBuilder, TestEncoderBuilder, TestForwarderBuilder, TestRelayBuilder,
                TestSourceBuilder, TestTransformBuilder,
            },
            OutputDefinition,
        },
    };

    /// A dynamic child worker for exercising [`SourceContext::spawn_handle`].
    ///
    /// Records that it started, then runs until its supervisor shuts it down.
    struct CountingChild {
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Supervisable for CountingChild {
        fn name(&self) -> &str {
            "child"
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            let started = Arc::clone(&self.started);
            Ok(Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                // Run until torn down, so the test can observe structured teardown of dynamic children.
                process_shutdown.await;
                Ok(())
            }))
        }
    }

    /// A source that runs until shutdown, optionally spawning a dynamic child on its ambient supervisor first.
    ///
    /// Records that it started (so tests can wait for readiness rather than sleeping) before running.
    struct ControlSource {
        started: Arc<AtomicUsize>,
        spawned_child: Option<Arc<AtomicUsize>>,
    }

    #[async_trait]
    impl Source for ControlSource {
        async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let shutdown = context.take_shutdown_handle();
            if let Some(started) = self.spawned_child {
                // Ambient spawning is how a component reaches its own supervisor: nothing is threaded to it, and the
                // child still has to land under this component rather than anywhere else.
                runtime::supervisable(CountingChild { started }).spawn();
            }
            shutdown.await;
            Ok(())
        }
    }

    struct ControlSourceBuilder {
        outputs: Vec<OutputDefinition<EventType>>,
        started: Arc<AtomicUsize>,
        spawned_child: Option<Arc<AtomicUsize>>,
    }

    impl ControlSourceBuilder {
        fn new(started: Arc<AtomicUsize>, spawned_child: Option<Arc<AtomicUsize>>) -> Self {
            Self {
                outputs: vec![OutputDefinition::default_output(EventType::EventD)],
                started,
                spawned_child,
            }
        }
    }

    #[async_trait]
    impl SourceBuilder for ControlSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
            Ok(Box::new(ControlSource {
                started: Arc::clone(&self.started),
                spawned_child: self.spawned_child.clone(),
            }))
        }
    }

    impl MemoryBounds for ControlSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that drains its input until the upstream closes (which happens when the source stops).
    struct DrainingDestination;

    #[async_trait]
    impl Destination for DrainingDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while context.events().next().await.is_some() {}
            Ok(())
        }
    }

    struct DrainingDestinationBuilder {
        input_event_ty: EventType,
    }

    #[async_trait]
    impl DestinationBuilder for DrainingDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            self.input_event_ty
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingDestination))
        }
    }

    impl MemoryBounds for DrainingDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that ignores both its input and shutdown, running forever until it is forcefully aborted.
    ///
    /// Records that it started (so tests can wait for it to actually be running before triggering shutdown).
    struct StuckDestination {
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Destination for StuckDestination {
        async fn run(self: Box<Self>, _context: DestinationContext) -> Result<(), GenericError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    struct StuckDestinationBuilder {
        input_event_ty: EventType,
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DestinationBuilder for StuckDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            self.input_event_ty
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(StuckDestination {
                started: Arc::clone(&self.started),
            }))
        }
    }

    impl MemoryBounds for StuckDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// Builds a connected `source` -> `destination` blueprint of long-running components.
    ///
    /// The source runs until shutdown; the destination drains until its upstream closes. If `spawned_child` is
    /// provided, the source spawns a dynamic child through its spawn handle that records when it starts.
    ///
    /// Returns the blueprint together with the source's "started" counter, so a test can wait for the source to
    /// actually be running (readiness polling) rather than sleeping for a fixed duration.
    fn long_running_blueprint(spawned_child: Option<Arc<AtomicUsize>>) -> (TopologyBlueprint, Arc<AtomicUsize>) {
        let source_started = Arc::new(AtomicUsize::new(0));
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "source",
                ControlSourceBuilder::new(Arc::clone(&source_started), spawned_child),
            )
            .expect("should not fail to add source")
            .add_destination(
                "destination",
                DrainingDestinationBuilder {
                    input_event_ty: EventType::EventD,
                },
            )
            .expect("should not fail to add destination");
        blueprint
            .connect_components_in_order(["source", "destination"])
            .expect("should not fail to connect components");
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());
        (blueprint, source_started)
    }

    /// Builds a connected `source` -> `destination` blueprint whose destination must be forcefully aborted on shutdown.
    ///
    /// The source stops cleanly when shutdown is signalled; the destination ignores shutdown and runs forever, so its
    /// per-component supervisor aborts it after `shutdown_timeout`. Exactly one component (the destination) is
    /// force-aborted, which lets a test assert a precise abort count.
    ///
    /// Returns the blueprint together with the destination's "started" counter, so a test can wait until the stuck
    /// destination is actually running before triggering shutdown.
    fn stuck_destination_blueprint(shutdown_timeout: Duration) -> (TopologyBlueprint, Arc<AtomicUsize>) {
        let destination_started = Arc::new(AtomicUsize::new(0));
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("source", ControlSourceBuilder::new(Arc::new(AtomicUsize::new(0)), None))
            .expect("should not fail to add source")
            .add_destination(
                "destination",
                StuckDestinationBuilder {
                    input_event_ty: EventType::EventD,
                    started: Arc::clone(&destination_started),
                },
            )
            .expect("should not fail to add destination");
        blueprint
            .connect_components_in_order(["source", "destination"])
            .expect("should not fail to connect components");
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_shutdown_timeout(shutdown_timeout);
        (blueprint, destination_started)
    }

    /// Spawns `blueprint` under a fresh `test-topology` supervisor, returning the shutdown sender and the run's join
    /// handle.
    ///
    /// This is the shared spawn/shutdown scaffold used by the topology-lifecycle tests below, replacing the
    /// copy-pasted supervisor construction repeated across them.
    fn spawn_supervised_blueprint(
        blueprint: TopologyBlueprint,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), SupervisorError>>,
    ) {
        let mut supervisor = Supervisor::new("test-topology").expect("should not fail to create supervisor");
        supervisor.add_worker(blueprint);

        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move { supervisor.run_with_shutdown(rx).await });
        (tx, handle)
    }

    /// Runs `blueprint` under a fresh `test-topology` supervisor with the given restart strategy until it exits on its
    /// own (no shutdown is ever signalled), returning the supervisor's result.
    async fn run_blueprint_until_exit(
        blueprint: TopologyBlueprint, strategy: RestartStrategy,
    ) -> Result<(), SupervisorError> {
        let mut supervisor = Supervisor::new("test-topology")
            .expect("should not fail to create supervisor")
            .with_restart_strategy(strategy);
        supervisor.add_worker(blueprint);

        // Hold the sender so shutdown is never triggered; the supervisor exits only when the topology does.
        let (_tx, rx) = oneshot::channel::<()>();
        tokio::time::timeout(Duration::from_secs(5), supervisor.run_with_shutdown(rx))
            .await
            .expect("supervisor should exit promptly")
    }

    /// Awaits a spawned topology-supervisor run to completion under a bounded timeout, unwrapping the join.
    async fn join_topology(
        handle: tokio::task::JoinHandle<Result<(), SupervisorError>>,
    ) -> Result<(), SupervisorError> {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervisor should exit promptly")
            .expect("supervisor task should not panic")
    }

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
                for (to, _) in targets {
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
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let result = run_blueprint_until_exit(
            blueprint,
            RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)),
        )
        .await;

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
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let result = run_blueprint_until_exit(blueprint, RestartStrategy::default()).await;

        assert!(matches!(result, Err(SupervisorError::FailedToInitialize { .. })));
    }

    #[tokio::test]
    async fn topology_waits_for_environment_readiness_before_starting() {
        // The topology must not start its components until the environment readiness signal resolves. We provide a
        // signal that never resolves, then trigger shutdown: the topology should exit cleanly without ever spawning
        // its components (which would otherwise finish immediately and fail the supervisor), and the supervisor should
        // shut down successfully.
        // A readiness gate that records when the topology first reaches it, then never resolves. This lets us wait for
        // the topology to actually be blocked on readiness before shutting down, rather than sleeping.
        let gate_reached = Arc::new(AtomicUsize::new(0));
        let gate = {
            let gate_reached = Arc::clone(&gate_reached);
            async move {
                gate_reached.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
        };

        let mut blueprint = connected_blueprint();
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_environment_readiness(gate);

        let (tx, handle) = spawn_supervised_blueprint(blueprint);

        // Once the topology is blocked on the readiness gate (and thus has NOT started its components), trigger
        // shutdown; it must exit cleanly.
        wait_until("the topology reached the readiness gate", || {
            gate_reached.load(Ordering::SeqCst) >= 1
        })
        .await;
        tx.send(()).expect("should send shutdown signal");

        let result = join_topology(handle).await;
        assert!(result.is_ok(), "supervisor should shut down cleanly, got: {:?}", result);
    }

    #[test]
    fn topology_ready_waits_for_registration_before_checking_readiness() {
        use tokio_test::{assert_pending, assert_ready, task::spawn};

        let health_registry = HealthRegistry::new();

        // Simulate an unrelated subsystem that has already registered and become ready. A naive readiness check against
        // the shared registry could resolve immediately here, even though the topology hasn't registered anything yet.
        let mut other = health_registry
            .register_component(&SubsystemIdentifier::from_dotted("env_provider.workload.foo"))
            .expect("should register component");
        other.mark_ready();

        let (registered_tx, registered_rx) = oneshot::channel();
        let topology_ready = TopologyReady {
            registered_rx,
            health_registry: health_registry.clone(),
            component_root: topology_identifier("primary"),
        };

        let mut wait = spawn(topology_ready.wait());

        // Despite no topology components being registered yet, `wait` must not resolve: it's gated on the registration
        // signal, which is precisely what prevents a false-ready observation.
        assert_pending!(wait.poll());

        // Now register a topology component (as the topology does when it spawns), but leave it not-ready.
        let mut source = health_registry
            .register_component(&SubsystemIdentifier::from_dotted("topology.primary.sources.in"))
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
            component_root: topology_identifier("primary"),
        };

        // Drop the sender without ever signaling, as happens when the topology is torn down before it registers its
        // components. `wait` should report that readiness will never be reached.
        drop(registered_tx);

        assert!(!topology_ready.wait().await);
    }

    #[tokio::test]
    async fn topology_clean_shutdown_returns_ok() {
        // A source that runs until shutdown feeds a destination that drains until its upstream closes. On shutdown,
        // the source stops (it observes its supervisor's shutdown signal), the destination drains and stops, and the
        // topology supervisor returns cleanly -- which is the intentional-shutdown path, distinct from a component
        // unexpectedly finishing.
        let (blueprint, source_started) = long_running_blueprint(None);
        let (tx, handle) = spawn_supervised_blueprint(blueprint);

        // Wait until the source is actually running before requesting shutdown, so we exercise shutting down live
        // components rather than shutting down before they start.
        wait_until("the source has started", || source_started.load(Ordering::SeqCst) == 1).await;
        tx.send(()).expect("should send shutdown signal");

        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);
    }

    #[tokio::test]
    async fn component_can_spawn_dynamic_child_via_spawn_handle() {
        // The source spawns a dynamic child through its context's spawn handle. The child records that it started and
        // then runs until torn down. We verify the child actually ran, then shut the topology down cleanly. The clean
        // shutdown also tears the (still-running) dynamic child down with its component's supervisor -- if it didn't,
        // draining would hang and the test would time out.
        let child_started = Arc::new(AtomicUsize::new(0));
        let (blueprint, _source_started) = long_running_blueprint(Some(Arc::clone(&child_started)));
        let (tx, handle) = spawn_supervised_blueprint(blueprint);

        // Wait until the dynamic child has started.
        wait_until("the dynamic child has started", || {
            child_started.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            child_started.load(Ordering::SeqCst),
            1,
            "dynamic child should have started exactly once"
        );

        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);
    }

    #[tokio::test]
    async fn forced_abort_count_survives_topology_boundary_to_root() {
        // A component that ignores graceful shutdown is forcefully aborted, and that abort count must survive the
        // `TopologyBlueprint` boundary -- where the topology supervisor's `SupervisorError` is flattened into a
        // `GenericError` -- and be recovered by the root supervisor as `ShutdownTimedOut`. This exercises the
        // `downcast` recovery path in `reported_abort_count`, which the production topology depends on but the direct
        // nested-supervisor tests in `supervisor.rs` do not cover: those hit the structured `WorkerError` variant,
        // never the flattened-`GenericError` path taken here.
        //
        // The source stops cleanly on shutdown; the destination ignores shutdown and runs forever, so exactly one
        // component (the destination) is force-aborted after the (short) shutdown timeout.
        let (blueprint, destination_started) = stuck_destination_blueprint(Duration::from_millis(100));
        let (tx, handle) = spawn_supervised_blueprint(blueprint);

        // The forced abort only happens if the stuck destination is actually running when shutdown arrives, so wait
        // for it to start before triggering shutdown.
        wait_until("the stuck destination has started", || {
            destination_started.load(Ordering::SeqCst) == 1
        })
        .await;
        tx.send(()).expect("should send shutdown signal");

        let result = join_topology(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "a component that ignored shutdown must surface as an unclean shutdown with a count of 1, got {result:?}"
        );
    }

    #[test]
    fn component_identity_is_byte_identical_across_subsystems() {
        // We want to ensure that when using a component's canonical identity, we are able to use that canonical
        // identical to reference the same entity across subsystems like the health registry, resource accounting, the
        // supervision/process tree, and so on.

        for (topology_name, raw_id) in [("primary", "dsd_in"), ("secondary", "dsd_agg")] {
            // Calculate the root topology identifier, our component context, and the identifiers for the component.
            let topology_root = topology_identifier(topology_name);
            let component_context = ComponentContext::source(&topology_root, ComponentId::try_from(raw_id).unwrap());
            let component_canonical_id = component_context.identity().to_string();
            let component_relative_id =
                get_component_relative_identifier(component_context.component_type(), component_context.component_id());

            assert_eq!(
                component_canonical_id,
                format!("topology.{topology_name}.sources.{raw_id}")
            );

            // Resource accounting: the component is addressed by its canonical identity directly, so declaring bounds
            // for it must create a node whose full path -- segment by segment -- is exactly that canonical identity.
            let registry = ComponentRegistry::default();
            registry
                .bounds_builder(&component_context.identity())
                .firm()
                .with_fixed_amount("marker", 1);

            let bounds = registry.as_bounds();
            let mut node = &bounds;
            for segment in component_canonical_id.split('.') {
                node = node
                    .subcomponents()
                    .into_iter()
                    .find_map(|(name, child)| (name.as_str() == segment).then_some(child))
                    .expect("resource-accounting node path must match the canonical identity segment by segment");
            }
            assert_eq!(
                node.total_firm_limit_bytes(),
                1,
                "the resource-accounting node at the canonical path must hold the declared bounds"
            );

            // Supervision: when using relative subsystem identifiers in a nested fashion, we should end up with
            // a process name that matches the canonical identity.
            let topology_sup = Name::root(topology_root.to_string()).expect("topology name is non-empty");
            let process_name =
                Name::scoped(&topology_sup, component_relative_id.to_string()).expect("component name is non-empty");
            assert_eq!(
                &*process_name,
                component_canonical_id.as_str(),
                "per-component supervisor process name must match the canonical identity"
            );
        }
    }

    #[test]
    fn recalculate_bounds_accounts_for_interconnect_and_event_buffer_memory() {
        // `recalculate_bounds` sizes the topology's interconnect and event-buffer memory from the component counts and
        // the interconnect capacity. Build a source -> transform -> destination topology, then set a distinctive
        // interconnect capacity so the recalculation runs with all components present, and assert the exact byte
        // totals against the documented arithmetic.
        let interconnect_capacity = 4usize;
        let mut blueprint = blueprint_with_components();
        blueprint.with_interconnect_capacity(NonZeroUsize::new(interconnect_capacity).unwrap());

        // Component counts once all three components are registered.
        let (sources, transforms, destinations, decoders) = (1usize, 1usize, 1usize, 0usize);

        // Minimum: one preallocated interconnect (holding `capacity` event buffers) per non-source component (that is,
        // every transform and destination).
        let total_interconnect_capacity = interconnect_capacity * (transforms + destinations);
        let expected_min = total_interconnect_capacity * std::mem::size_of::<EventsBuffer>();

        // Firm: the maximum number of in-flight event buffers, each sized as one events-buffer container plus the
        // events it holds at the default per-buffer capacity. The firm total is additive with the minimum.
        let max_in_flight = ((transforms + destinations) * interconnect_capacity) + sources + decoders + transforms;
        let per_buffer =
            std::mem::size_of::<EventsBuffer>() + (std::mem::size_of::<Event>() * DEFAULT_EVENTS_BUFFER_CAPACITY);
        let expected_firm = expected_min + (max_in_flight * per_buffer);

        let bounds = {
            let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
            guard
                .as_ref()
                .expect("topology blueprint already initialized")
                .component_registry
                .as_bounds()
        };

        assert_eq!(
            bounds.total_minimum_required_bytes(),
            expected_min,
            "interconnect minimum bytes should be capacity * non-source components * size_of::<EventsBuffer>()"
        );
        assert_eq!(
            bounds.total_firm_limit_bytes(),
            expected_firm,
            "firm bytes should add the max in-flight event-buffer memory on top of the minimum"
        );
    }

    #[test]
    fn the_topology_bounds_count_every_component_including_the_last_one_added() {
        // Bounds are recalculated as each component is added, so a blueprint whose interconnect capacity is set before
        // its components are added must count all of them, the last one too.
        let interconnect_capacity = 4usize;
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint.with_interconnect_capacity(NonZeroUsize::new(interconnect_capacity).unwrap());
        blueprint
            .add_source("source", TestSourceBuilder::default_output(EventType::EventD))
            .unwrap()
            .add_transform(
                "transform",
                TestTransformBuilder::default_output(EventType::EventD, EventType::EventD),
            )
            .unwrap()
            .add_destination(
                "destination",
                TestDestinationBuilder::with_input_type(EventType::EventD),
            )
            .unwrap();

        // One interconnect for the transform and one for the destination, and the in-flight buffers of a source and a
        // transform besides those the interconnects hold.
        let expected_min = 2 * interconnect_capacity * std::mem::size_of::<EventsBuffer>();
        let per_buffer =
            std::mem::size_of::<EventsBuffer>() + (std::mem::size_of::<Event>() * DEFAULT_EVENTS_BUFFER_CAPACITY);
        let expected_firm = expected_min + ((2 * interconnect_capacity + 2) * per_buffer);

        let bounds = component_registry.as_bounds();
        assert_eq!(bounds.total_minimum_required_bytes(), expected_min);
        assert_eq!(bounds.total_firm_limit_bytes(), expected_firm);
    }

    /// A transform builder that declares `lanes` input lanes and input buffers of at most `buffer_events` events, and
    /// otherwise builds what `inner` builds.
    struct LanedTransformBuilder<B = ForwardingTransformBuilder> {
        lanes: NonZeroU16,
        buffer_events: usize,
        inner: B,
    }

    impl LanedTransformBuilder {
        fn forwarding(lanes: u16) -> Self {
            Self {
                lanes: NonZeroU16::new(lanes).unwrap(),
                buffer_events: DEFAULT_EVENTS_BUFFER_CAPACITY,
                inner: ForwardingTransformBuilder::new(),
            }
        }
    }

    #[async_trait]
    impl<B> TransformBuilder for LanedTransformBuilder<B>
    where
        B: TransformBuilder + Send + Sync,
    {
        fn input_event_type(&self) -> EventType {
            self.inner.input_event_type()
        }

        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            self.inner.outputs()
        }

        fn input_lanes(&self) -> NonZeroU16 {
            self.lanes
        }

        fn input_buffer_events(&self) -> usize {
            self.buffer_events
        }

        async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
            self.inner.build(context).await
        }
    }

    impl<B> MemoryBounds for LanedTransformBuilder<B> {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// Returns every edge of `blueprint`'s snapshot, with the routing it records.
    fn routed_edges(blueprint: &TopologyBlueprint) -> Vec<(String, String, Option<LaneEdge>)> {
        blueprint
            .snapshot()
            .unwrap()
            .edges()
            .iter()
            .map(|edge| (edge.from().to_string(), edge.to().to_string(), edge.routing()))
            .collect()
    }

    #[test]
    fn a_transform_with_lanes_is_connected_by_routed_edges_and_described_with_its_lanes() {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        assert_eq!(ForwardingTransformBuilder::new().input_lanes().get(), 1);
        assert_eq!(
            ForwardingTransformBuilder::new().input_buffer_events(),
            DEFAULT_EVENTS_BUFFER_CAPACITY
        );
        blueprint
            .add_source("source", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_source("other", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_transform("laned", LanedTransformBuilder::forwarding(3))
            .unwrap()
            .with_component_input_capacity("laned", NonZeroUsize::new(5).unwrap())
            .unwrap();

        // An edge into a component with lanes names its routing, and a lane the component has.
        let error = blueprint.connect_components("source", "laned").map(|_| ()).unwrap_err();
        assert!(
            format!("{error:#}").contains(
                "cannot connect 'source' to 'laned' without naming how it picks one of the component's 3 input lanes"
            ),
            "{error:#}"
        );
        let error = blueprint
            .connect_components_with("source", "laned", LaneEdge::Fixed(3))
            .map(|_| ())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot connect 'source' to lane 3 of 'laned', which has 3 input lane(s)"),
            "{error:#}"
        );
        blueprint
            .connect_components_with("source", "laned", LaneEdge::Fixed(2))
            .unwrap()
            .connect_components_with("other", "laned", LaneEdge::Spread)
            .unwrap();

        let snapshot = blueprint.snapshot().unwrap();
        let laned = snapshot.component("laned").unwrap();
        assert_eq!(laned.input_lanes(), NonZeroU16::new(3));
        assert_eq!(
            laned.input_capacity(),
            NonZeroUsize::new(5),
            "the capacity of each lane"
        );
        assert_eq!(snapshot.component("source").unwrap().input_lanes(), None);
        assert_eq!(
            routed_edges(&blueprint),
            vec![
                ("other".to_string(), "laned".to_string(), Some(LaneEdge::Spread)),
                ("source".to_string(), "laned".to_string(), Some(LaneEdge::Fixed(2))),
            ]
        );
    }

    #[test]
    fn an_edge_into_a_component_with_one_lane_leads_to_lane_zero_whatever_it_names() {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("source", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_source("other", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_source("third", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_transform("transform", ForwardingTransformBuilder::new())
            .unwrap();
        blueprint
            .connect_components_with("source", "transform", LaneEdge::Fixed(0))
            .unwrap()
            .connect_components_with("other", "transform", LaneEdge::Spread)
            .unwrap();
        let error = blueprint
            .connect_components_with("third", "transform", LaneEdge::Fixed(1))
            .map(|_| ())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot connect 'third' to lane 1 of 'transform', which has 1 input lane(s)"),
            "{error:#}"
        );
        assert_eq!(LaneEdge::Fixed(3).to_string(), "fixed to lane 3");
        // Edges into a component with one lane record no routing, whatever they named.
        assert_eq!(
            routed_edges(&blueprint),
            vec![
                ("other".to_string(), "transform".to_string(), None),
                ("source".to_string(), "transform".to_string(), None),
            ]
        );
    }

    #[test]
    fn a_replacement_keeps_the_input_lanes_of_the_transform_it_replaces() {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_transform("laned", LanedTransformBuilder::forwarding(2))
            .unwrap();
        let error = blueprint
            .replace_transform("laned", ForwardingTransformBuilder::new())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("but the component it replaces declares input"),
            "{}",
            error
        );
        assert!(error.contains("on 2 lanes"), "{}", error);
        blueprint
            .replace_transform("laned", LanedTransformBuilder::forwarding(2))
            .unwrap();
    }

    #[test]
    fn replacing_a_transform_recalculates_the_bounds_of_its_input_from_the_replacement() {
        // src -> laned (2 lanes) -> out at an interconnect capacity of 4: built with a transform whose input holds
        // buffers of 16 events, and built with one of 1,024 events that is then replaced by one of 16.
        let laned = |buffer_events| LanedTransformBuilder {
            buffer_events,
            ..LanedTransformBuilder::forwarding(2)
        };
        let blueprint_with = |registry: &ComponentRegistry, buffer_events| {
            let mut blueprint = TopologyBlueprint::new("test", registry);
            blueprint.with_interconnect_capacity(NonZeroUsize::new(4).unwrap());
            blueprint
                .add_source("src", TestSourceBuilder::default_output(EventType::Metric))
                .unwrap()
                .add_transform("laned", laned(buffer_events))
                .unwrap()
                .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
                .unwrap();
            blueprint
        };
        let fresh = ComponentRegistry::default();
        let _fresh = blueprint_with(&fresh, 16);
        let replaced = ComponentRegistry::default();
        let mut blueprint = blueprint_with(&replaced, DEFAULT_EVENTS_BUFFER_CAPACITY);
        assert_ne!(total_bytes(&replaced), total_bytes(&fresh));

        blueprint.replace_transform("laned", laned(16)).unwrap();
        assert_eq!(total_bytes(&replaced), total_bytes(&fresh));
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_transform_whose_input_holds_smaller_buffers_fails_on_a_buffer_allocated_for_more_events() {
        // The bounds count 4 events per slot of the input of a transform that declares buffers of at most 4 events, so
        // a buffer with storage for more holds memory they do not count, however few events it holds: a debug build
        // fails the transform on receiving one. Returns how the topology ended and the lengths of the buffers received.
        async fn run(source: ScriptedSourceBuilder) -> (Result<(), SupervisorError>, Vec<(usize, usize)>) {
            let received = Arc::new(StdMutex::new(Vec::new()));
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("test", &component_registry);
            blueprint
                .add_source("src", source)
                .unwrap()
                .add_transform(
                    "small",
                    LanedTransformBuilder {
                        lanes: NonZeroU16::MIN,
                        buffer_events: 4,
                        inner: LaneRecordingTransformBuilder {
                            received: Arc::clone(&received),
                            outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                        },
                    },
                )
                .unwrap()
                .connect_components("src", "small")
                .unwrap()
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_resource_registry(ResourceRegistry::new());
            let result = run_blueprint_until_exit(
                blueprint,
                RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)),
            )
            .await;
            let received = received.lock().unwrap().clone();
            (result, received)
        }
        let steps = |sizes: &[usize]| {
            sizes
                .iter()
                .map(|&events| ScriptStep {
                    output: None,
                    events,
                    at: AdmissionTime::from_unix_secs(100),
                })
                .collect::<Vec<_>>()
        };

        // A buffer allocated for its 4 events is received; one allocated for its 8 is not.
        let (result, received) = run(ScriptedSourceBuilder::new(steps(&[4, 8]), ManualWallClock::default())).await;
        assert!(matches!(result, Err(SupervisorError::Shutdown)), "{:?}", result);
        assert_eq!(received, vec![(0, 4)]);

        // A buffer of 2 events allocated for a full events buffer, as a decoder's is, is not received either.
        let source = ScriptedSourceBuilder::new(steps(&[2]), ManualWallClock::default())
            .allocating(DEFAULT_EVENTS_BUFFER_CAPACITY);
        let (result, received) = run(source).await;
        assert!(matches!(result, Err(SupervisorError::Shutdown)), "{:?}", result);
        assert_eq!(received, Vec::new());
    }

    #[test]
    fn the_bounds_count_an_interconnect_per_lane_holding_the_events_its_transform_declares_per_buffer() {
        // src -> laned (3 lanes of buffers of up to 256 events) -> plain -> out, at an interconnect capacity of 4.
        let interconnect_capacity = 4usize;
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint.with_interconnect_capacity(NonZeroUsize::new(interconnect_capacity).unwrap());
        blueprint
            .add_source("src", TestSourceBuilder::default_output(EventType::Metric))
            .unwrap()
            .add_transform(
                "laned",
                LanedTransformBuilder {
                    buffer_events: 256,
                    ..LanedTransformBuilder::forwarding(3)
                },
            )
            .unwrap()
            .add_transform("plain", ForwardingTransformBuilder::new())
            .unwrap()
            .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
            .unwrap();

        let buffer_bytes = |events: usize| std::mem::size_of::<EventsBuffer>() + std::mem::size_of::<Event>() * events;
        let expected = |laned_capacity: usize| {
            // The laned transform's three interconnects, and one each for the plain transform and the destination.
            let laned_slots = 3 * laned_capacity;
            let minimum = (laned_slots + 2 * interconnect_capacity) * std::mem::size_of::<EventsBuffer>();
            // Buffers of 1,024 events: the plain interconnects, and one in flight in the source and in each of the four
            // transform lanes; and the laned interconnects' buffers of 256 events.
            let full = 2 * interconnect_capacity + 1 + 4;
            let firm = minimum + full * buffer_bytes(DEFAULT_EVENTS_BUFFER_CAPACITY) + laned_slots * buffer_bytes(256);
            (minimum, firm)
        };
        assert_eq!(total_bytes(&component_registry), expected(interconnect_capacity));

        // A capacity of its own applies to each lane.
        blueprint
            .with_component_input_capacity("laned", NonZeroUsize::new(5).unwrap())
            .unwrap();
        assert_eq!(total_bytes(&component_registry), expected(5));

        // A transform's buffers hold at least one event and at most as many as an events buffer holds.
        let mut blueprint = TopologyBlueprint::new("clamped", &component_registry);
        blueprint.with_interconnect_capacity(NonZeroUsize::new(1).unwrap());
        let before = total_bytes(&component_registry);
        blueprint
            .add_transform(
                "huge",
                LanedTransformBuilder {
                    buffer_events: 1 << 20,
                    ..LanedTransformBuilder::forwarding(1)
                },
            )
            .unwrap()
            .add_transform(
                "empty",
                LanedTransformBuilder {
                    buffer_events: 0,
                    ..LanedTransformBuilder::forwarding(1)
                },
            )
            .unwrap();
        let after = total_bytes(&component_registry);
        let slot = std::mem::size_of::<EventsBuffer>();
        assert_eq!(
            (after.0 - before.0, after.1 - before.1),
            (
                2 * slot,
                2 * slot + 3 * buffer_bytes(DEFAULT_EVENTS_BUFFER_CAPACITY) + buffer_bytes(1)
            )
        );
    }

    /// A transform that records the lane and length of every buffer each of its lanes receives, until every lane's
    /// input closes.
    struct LaneRecordingTransform {
        received: Arc<StdMutex<Vec<(usize, usize)>>>,
    }

    #[async_trait]
    impl Transform for LaneRecordingTransform {
        async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
            let lanes = context
                .take_lane_inputs()
                .into_iter()
                .enumerate()
                .map(|(lane, mut input)| {
                    let received = Arc::clone(&self.received);
                    tokio::spawn(async move {
                        while let Some(buffer) = input.next().await {
                            received.lock().unwrap().push((lane, buffer.len()));
                        }
                    })
                })
                .collect::<Vec<_>>();
            for lane in lanes {
                lane.await
                    .map_err(|error| saluki_error::generic_error!("A lane task failed: {}", error))?;
            }
            Ok(())
        }
    }

    struct LaneRecordingTransformBuilder {
        received: Arc<StdMutex<Vec<(usize, usize)>>>,
        outputs: Vec<OutputDefinition<EventType>>,
    }

    #[async_trait]
    impl TransformBuilder for LaneRecordingTransformBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
            Ok(Box::new(LaneRecordingTransform {
                received: Arc::clone(&self.received),
            }))
        }
    }

    impl MemoryBounds for LaneRecordingTransformBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    #[tokio::test]
    async fn a_running_topology_feeds_each_lane_of_a_transform_by_the_routing_of_its_edges() {
        // fixed -> laned (lane 1); spread -> {laned (spread over two lanes), tap}. The sources' batches are told apart
        // by their sizes: the fixed source sends batches of 1 to 3 events, the spread source of 4 to 7.
        let steps = |sizes: std::ops::RangeInclusive<usize>| {
            sizes
                .map(|events| ScriptStep {
                    output: None,
                    events,
                    at: AdmissionTime::from_unix_secs(100),
                })
                .collect::<Vec<_>>()
        };
        let received = Arc::new(StdMutex::new(Vec::new()));
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "fixed",
                ScriptedSourceBuilder::new(steps(1..=3), ManualWallClock::default()),
            )
            .unwrap()
            .add_source(
                "spread",
                ScriptedSourceBuilder::new(steps(4..=7), ManualWallClock::default()),
            )
            .unwrap()
            .add_transform(
                "laned",
                LanedTransformBuilder {
                    lanes: NonZeroU16::new(2).unwrap(),
                    buffer_events: DEFAULT_EVENTS_BUFFER_CAPACITY,
                    inner: LaneRecordingTransformBuilder {
                        received: Arc::clone(&received),
                        outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                    },
                },
            )
            .unwrap();
        let tap = add_collector(&mut blueprint, "tap");
        blueprint
            .connect_components_with("fixed", "laned", LaneEdge::Fixed(1))
            .unwrap()
            .connect_components_with("spread", "laned", LaneEdge::Spread)
            .unwrap()
            .connect_components("spread", "tap")
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        wait_until("every lane and the tap received their batches", || {
            received.lock().unwrap().len() == 7 && tap.lock().unwrap().len() == 4
        })
        .await;
        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);

        let received = received.lock().unwrap().clone();
        let from = |sizes: std::ops::RangeInclusive<usize>, lane: usize| {
            received
                .iter()
                .filter(|(at, events)| *at == lane && sizes.contains(events))
                .map(|(_, events)| *events)
                .collect::<Vec<_>>()
        };
        // Every batch of the fixed source reaches lane 1, in order; the spread source's alternate, starting at lane 0.
        assert_eq!(from(1..=3, 0), Vec::<usize>::new());
        assert_eq!(from(1..=3, 1), vec![1, 2, 3]);
        assert_eq!(from(4..=7, 0), vec![4, 6]);
        assert_eq!(from(4..=7, 1), vec![5, 7]);
        // The tap, with one lane, receives every batch of the spread source.
        let tap_sizes = tap.lock().unwrap().iter().map(EventsBuffer::len).collect::<Vec<_>>();
        assert_eq!(tap_sizes, vec![4, 5, 6, 7]);
    }

    #[test]
    fn a_dedicated_worker_pool_can_be_sized() {
        let mut blueprint = blueprint_with_components();
        blueprint.with_dedicated_worker_pool(NonZeroUsize::new(12).unwrap());
        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        let config = &guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .worker_pool_config;
        assert!(matches!(config, WorkerPoolConfiguration::Dedicated { threads } if threads.get() == 12));
    }

    #[test]
    fn owned_points_shared_clocks_and_heartbeats_in_each_second_are_never_a_dispatchers() {
        let schedule = AdmissionSchedule::Steady(Arc::new(GenerationToken::new(GenerationId::new(1)))).into_handle();
        let config = |id| {
            AdmissionPointConfig::new(
                AdmissionPointId::new(id),
                AdmissionMode::Admit,
                Arc::clone(&schedule),
                Duration::from_secs(300),
            )
        };
        let mut blueprint = blueprint_with_components();
        let error = blueprint
            .with_admission_point("source", config(1).owned())
            .err()
            .expect("an owned point is refused")
            .to_string();
        assert!(error.contains("is owned"), "{}", error);
        let clock = crate::execution::SourceClock::new(crate::execution::AdmissionClock::new(Duration::from_secs(300)));
        let error = blueprint
            .with_admission_point("source", config(2).with_source_clock(clock))
            .err()
            .expect("a shared clock is refused")
            .to_string();
        assert!(error.contains("shares a source clock"), "{}", error);
        let error = blueprint
            .with_admission_point(
                "source",
                config(3)
                    .with_heartbeat(Duration::from_secs(1))
                    .with_heartbeat_each_second(Duration::from_millis(100)),
            )
            .err()
            .expect("a heartbeat in each second is refused")
            .to_string();
        assert!(error.contains("heartbeats in each second"), "{}", error);
    }

    #[test]
    fn worker_pool_configuration_defaults_to_dedicated() {
        let blueprint = blueprint_with_components();
        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        let config = &guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .worker_pool_config;
        assert!(
            matches!(config, WorkerPoolConfiguration::Dedicated { threads } if *threads == DEFAULT_WORKER_POOL_THREADS),
            "the default worker-pool configuration must be dedicated"
        );
    }

    #[test]
    fn with_ambient_worker_pool_selects_ambient() {
        let mut blueprint = blueprint_with_components();
        blueprint.with_ambient_worker_pool();

        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        let config = &guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .worker_pool_config;
        assert!(matches!(config, WorkerPoolConfiguration::Ambient));
    }

    /// One batch a [`ScriptedSource`] sends: `events` metrics on `output` (the default output if `None`), with the
    /// source's wall clock set to `at` first.
    #[derive(Clone)]
    struct ScriptStep {
        output: Option<&'static str>,
        events: usize,
        at: AdmissionTime,
    }

    /// A source that sends a scripted sequence of batches, each in a buffer allocated for its events or for `allocated`
    /// events, whichever is more, then runs until shutdown.
    struct ScriptedSource {
        script: Vec<ScriptStep>,
        wall: ManualWallClock,
        allocated: usize,
    }

    #[async_trait]
    impl Source for ScriptedSource {
        async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            let shutdown = context.take_shutdown_handle();
            for step in &self.script {
                self.wall.set(step.at);
                let mut buffer = EventsBuffer::with_capacity(step.events.max(self.allocated));
                for value in 0..step.events {
                    assert!(buffer
                        .try_push(Event::Metric(Metric::gauge("m", value as f64)))
                        .is_none());
                }
                match step.output {
                    None => context.dispatcher().dispatch(buffer).await?,
                    Some(name) => context.dispatcher().dispatch_named(name, buffer).await?,
                }
            }
            shutdown.await;
            Ok(())
        }
    }

    /// Builds a [`ScriptedSource`] with a default metrics output and a named `other` metrics output.
    struct ScriptedSourceBuilder {
        outputs: Vec<OutputDefinition<EventType>>,
        script: Vec<ScriptStep>,
        wall: ManualWallClock,
        allocated: usize,
    }

    impl ScriptedSourceBuilder {
        fn new(script: Vec<ScriptStep>, wall: ManualWallClock) -> Self {
            Self {
                outputs: vec![
                    OutputDefinition::default_output(EventType::Metric),
                    OutputDefinition::named_output("other", EventType::Metric),
                ],
                script,
                wall,
                allocated: 0,
            }
        }

        /// Returns this builder with every buffer allocated for at least `events` events, as a decoder allocates a full
        /// buffer however many events it fills it with.
        fn allocating(mut self, events: usize) -> Self {
            self.allocated = events;
            self
        }
    }

    #[async_trait]
    impl SourceBuilder for ScriptedSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
            Ok(Box::new(ScriptedSource {
                script: self.script.clone(),
                wall: self.wall.clone(),
                allocated: self.allocated,
            }))
        }
    }

    impl MemoryBounds for ScriptedSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A transform that forwards every buffer it receives, unchanged, until its input closes.
    struct ForwardingTransform;

    #[async_trait]
    impl Transform for ForwardingTransform {
        async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
            while let Some(buffer) = context.events().next().await {
                context.dispatcher().dispatch(buffer).await?;
            }
            Ok(())
        }
    }

    struct ForwardingTransformBuilder {
        outputs: Vec<OutputDefinition<EventType>>,
    }

    impl ForwardingTransformBuilder {
        fn new() -> Self {
            Self {
                outputs: vec![OutputDefinition::default_output(EventType::Metric)],
            }
        }
    }

    #[async_trait]
    impl TransformBuilder for ForwardingTransformBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
            Ok(Box::new(ForwardingTransform))
        }
    }

    impl MemoryBounds for ForwardingTransformBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that records every buffer it receives, until its input closes.
    struct CollectingDestination {
        received: Arc<StdMutex<Vec<EventsBuffer>>>,
    }

    #[async_trait]
    impl Destination for CollectingDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while let Some(buffer) = context.events().next().await {
                self.received.lock().unwrap().push(buffer);
            }
            Ok(())
        }
    }

    struct CollectingDestinationBuilder {
        received: Arc<StdMutex<Vec<EventsBuffer>>>,
        gate: Option<crate::topology::interconnect::InputGate>,
    }

    #[async_trait]
    impl DestinationBuilder for CollectingDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        fn input_gate(&self) -> Option<crate::topology::interconnect::InputGate> {
            self.gate.clone()
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(CollectingDestination {
                received: Arc::clone(&self.received),
            }))
        }
    }

    impl MemoryBounds for CollectingDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// Adds a collecting destination to `blueprint` and returns what it will record.
    fn add_collector(blueprint: &mut TopologyBlueprint, id: &str) -> Arc<StdMutex<Vec<EventsBuffer>>> {
        add_gated_collector(blueprint, id, None)
    }

    /// Adds a collecting destination with the input gate `gate`, if any, to `blueprint` and returns what it will
    /// record.
    fn add_gated_collector(
        blueprint: &mut TopologyBlueprint, id: &str, gate: Option<crate::topology::interconnect::InputGate>,
    ) -> Arc<StdMutex<Vec<EventsBuffer>>> {
        let received = Arc::new(StdMutex::new(Vec::new()));
        blueprint
            .add_destination(
                id,
                CollectingDestinationBuilder {
                    received: Arc::clone(&received),
                    gate,
                },
            )
            .expect("should not fail to add destination");
        received
    }

    /// A received buffer's `(t_a in seconds, admission point, generation, event count)`.
    type Summary = (Option<u64>, Option<u32>, Option<u64>, usize);

    /// Summarizes received buffers.
    fn summarize(received: &StdMutex<Vec<EventsBuffer>>) -> Vec<Summary> {
        received
            .lock()
            .unwrap()
            .iter()
            .map(|buffer| {
                let header = buffer.header();
                (
                    header.admitted_at.map(AdmissionTime::as_unix_secs),
                    header.admission_point.map(AdmissionPointId::get),
                    header.generation_id().map(GenerationId::get),
                    buffer.len(),
                )
            })
            .collect()
    }

    fn steady_schedule(generation: u64) -> ScheduleHandle {
        AdmissionSchedule::Steady(Arc::new(GenerationToken::new(GenerationId::new(generation)))).into_handle()
    }

    fn admission_config(id: u32, mode: AdmissionMode, schedule: &ScheduleHandle) -> AdmissionPointConfig {
        AdmissionPointConfig::new(
            AdmissionPointId::new(id),
            mode,
            Arc::clone(schedule),
            Duration::from_secs(600),
        )
    }

    /// Builds a blueprint holding one component of every type, connected to nothing.
    fn blueprint_with_every_component_type() -> TopologyBlueprint {
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap()
            .add_relay("relay", TestRelayBuilder::default_output(PayloadType::Raw))
            .unwrap()
            .add_decoder(
                "dec",
                TestDecoderBuilder::with_input_and_output_type(PayloadType::Raw, EventType::Metric),
            )
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_transform("fwd_two", ForwardingTransformBuilder::new())
            .unwrap()
            .add_encoder(
                "enc",
                TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Http),
            )
            .unwrap()
            .add_forwarder("fwdr", TestForwarderBuilder::with_input_type(PayloadType::Http))
            .unwrap()
            .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
            .unwrap();
        blueprint
    }

    #[test]
    fn admission_points_must_be_event_outputs_of_existing_components() {
        let schedule = steady_schedule(1);
        let mut blueprint = blueprint_with_every_component_type();

        // Event outputs of sources, decoders, and transforms can be admission points.
        for (id, output) in [(1, "src"), (2, "src.other"), (3, "dec"), (4, "fwd")] {
            blueprint
                .with_admission_point(output, admission_config(id, AdmissionMode::Admit, &schedule))
                .unwrap_or_else(|e| panic!("{output} should be accepted: {e}"));
        }

        let rejected = [
            (
                "not an id",
                5,
                "Invalid admission point output 'not an id': component output IDs may only contain",
            ),
            (
                "missing",
                5,
                "Cannot mark 'missing' as an admission point: nonexistent component ID 'missing'.",
            ),
            (
                "src.absent",
                5,
                "Cannot mark 'src.absent' as an admission point: nonexistent component output ID 'src.absent'.",
            ),
            (
                "out",
                5,
                "Cannot mark 'out' as an admission point: nonexistent component output ID 'out'.",
            ),
            (
                "fwdr",
                5,
                "Cannot mark 'fwdr' as an admission point: nonexistent component output ID 'fwdr'.",
            ),
            (
                "relay",
                5,
                "Cannot mark 'relay' as an admission point: it is a payload(Raw) output.",
            ),
            (
                "enc",
                5,
                "Cannot mark 'enc' as an admission point: it is a payload(HTTP) output.",
            ),
            (
                "src",
                5,
                "Output 'src' is already an admission point. Each output can have at most one.",
            ),
            (
                "fwd_two",
                1,
                "Cannot mark 'fwd_two' as an admission point: admission point ID 1 is already used by output 'src'.",
            ),
        ];
        for (output, id, expected) in rejected {
            let error = blueprint
                .with_admission_point(output, admission_config(id, AdmissionMode::Readmit, &schedule))
                .map(|_| ())
                .expect_err(output);
            assert!(
                error.to_string().starts_with(expected),
                "{output}: expected '{expected}', got '{error}'"
            );
        }

        // Heartbeat periods from 1 ms through 1 hour are accepted; a zero period would send a heartbeat on every
        // timer tick, and a huge one would overflow the driver's deadline.
        for period in [
            Duration::ZERO,
            Duration::from_micros(999),
            Duration::from_secs(3_600) + Duration::from_nanos(1),
            Duration::from_secs(u64::MAX),
        ] {
            let error = blueprint
                .with_admission_point(
                    "fwd_two",
                    admission_config(5, AdmissionMode::Readmit, &schedule).with_heartbeat(period),
                )
                .map(|_| ())
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "The heartbeat period of admission point 'fwd_two' is {:?}, outside the allowed range of 1ms \
                     through 3600s. Use a period within that range, or leave heartbeats disabled.",
                    period
                )
            );
        }

        // Rejected calls change nothing.
        let marked = blueprint
            .snapshot()
            .unwrap()
            .admission_points()
            .iter()
            .map(|point| (point.output().to_string(), point.id().get()))
            .collect::<Vec<_>>();
        assert_eq!(
            marked,
            vec![
                ("dec".to_string(), 3),
                ("fwd".to_string(), 4),
                ("src".to_string(), 1),
                ("src.other".to_string(), 2)
            ]
        );
    }

    #[test]
    fn the_snapshot_describes_components_outputs_connections_and_admission_points() {
        let schedule = steady_schedule(1);
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_forwarder("fwdr", TestForwarderBuilder::with_input_type(PayloadType::Http))
            .unwrap()
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap()
            .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_encoder(
                "enc",
                TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Http),
            )
            .unwrap()
            .connect_components("src.other", "enc")
            .unwrap()
            .connect_components_in_order(["src", "fwd", "out"])
            .unwrap()
            .connect_components("enc", "fwdr")
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(1, AdmissionMode::Admit, &schedule).with_heartbeat(Duration::from_secs(5)),
            )
            .unwrap()
            .with_admission_point("fwd", admission_config(2, AdmissionMode::Readmit, &schedule))
            .unwrap();

        let snapshot = blueprint.snapshot().expect("the blueprint is not initialized");

        let metrics = DataType::Event(EventType::Metric);
        let http = DataType::Payload(PayloadType::Http);
        let components = snapshot
            .components()
            .iter()
            .map(|component| {
                (
                    component.id().to_string(),
                    component.component_type(),
                    component.input(),
                    component
                        .outputs()
                        .iter()
                        .map(|output| (output.id().to_string(), output.data_type()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            components,
            vec![
                (
                    "enc".to_string(),
                    ComponentType::Encoder,
                    Some(metrics),
                    vec![("enc".to_string(), http)]
                ),
                (
                    "fwd".to_string(),
                    ComponentType::Transform,
                    Some(metrics),
                    vec![("fwd".to_string(), metrics)]
                ),
                ("fwdr".to_string(), ComponentType::Forwarder, Some(http), vec![]),
                ("out".to_string(), ComponentType::Destination, Some(metrics), vec![]),
                (
                    "src".to_string(),
                    ComponentType::Source,
                    None,
                    vec![("src".to_string(), metrics), ("src.other".to_string(), metrics)]
                ),
            ]
        );

        let edges = snapshot
            .edges()
            .iter()
            .map(|edge| (edge.from().to_string(), edge.to().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            edges,
            vec![
                ("enc".to_string(), "fwdr".to_string()),
                ("fwd".to_string(), "out".to_string()),
                ("src".to_string(), "fwd".to_string()),
                ("src.other".to_string(), "enc".to_string()),
            ]
        );

        let points = snapshot
            .admission_points()
            .iter()
            .map(|point| {
                (
                    point.output().to_string(),
                    point.id().get(),
                    point.mode(),
                    point.heartbeat(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            points,
            vec![
                ("fwd".to_string(), 2, AdmissionMode::Readmit, None),
                ("src".to_string(), 1, AdmissionMode::Admit, Some(Duration::from_secs(5))),
            ]
        );

        let source = snapshot.component("src").expect("the source is in the snapshot");
        assert_eq!(source.outputs()[1].name(), OutputName::Given("other".into()));
        assert!(snapshot.component("absent").is_none());

        // Assembling the same structure in another order gives an equal snapshot.
        let mut reordered = TopologyBlueprint::new("test", &component_registry);
        reordered
            .add_encoder(
                "enc",
                TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Http),
            )
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
            .unwrap()
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap()
            .add_forwarder("fwdr", TestForwarderBuilder::with_input_type(PayloadType::Http))
            .unwrap()
            .connect_components("enc", "fwdr")
            .unwrap()
            .connect_components("fwd", "out")
            .unwrap()
            .connect_components("src", "fwd")
            .unwrap()
            .connect_components("src.other", "enc")
            .unwrap()
            .with_admission_point("fwd", admission_config(2, AdmissionMode::Readmit, &schedule))
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(1, AdmissionMode::Admit, &schedule).with_heartbeat(Duration::from_secs(5)),
            )
            .unwrap();
        assert_eq!(reordered.snapshot(), Some(snapshot));
    }

    #[test]
    fn the_snapshot_is_unavailable_once_the_blueprint_is_initialized() {
        let blueprint = blueprint_with_components();
        assert!(blueprint.snapshot().is_some());

        // Initialization takes the build state.
        let _ = blueprint.build_state.lock().unwrap().take();
        assert!(blueprint.snapshot().is_none());
    }

    #[test]
    fn connecting_the_same_components_twice_is_rejected() {
        let mut blueprint = blueprint_with_sources_and_destinations(&["source"], &["dest"]);
        blueprint.connect_components("source", "dest").unwrap();

        let error = blueprint
            .connect_components("source", ["dest"])
            .map(|_| ())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("duplicate connection from 'source' to 'dest'"),
            "got: {error:#}"
        );
        assert_eq!(
            connected_pairs(&blueprint),
            vec![("source".to_string(), "dest".to_string())]
        );
    }

    #[tokio::test]
    async fn a_running_topology_stamps_buffers_at_its_admission_points() {
        // src (admit, point 1) -> direct
        //     ................ -> fwd (readmit, point 2) -> readmitted
        // src.other (no admission point) -> plain
        let schedule = steady_schedule(7);
        let source_wall = ManualWallClock::default();
        let script = vec![
            ScriptStep {
                output: None,
                events: 2,
                at: AdmissionTime::from_unix_secs(100),
            },
            ScriptStep {
                output: None,
                events: 1,
                at: AdmissionTime::from_unix_secs(101),
            },
            ScriptStep {
                output: Some("other"),
                events: 3,
                at: AdmissionTime::from_unix_secs(102),
            },
        ];

        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("src", ScriptedSourceBuilder::new(script, source_wall.clone()))
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap();
        let direct = add_collector(&mut blueprint, "direct");
        let readmitted = add_collector(&mut blueprint, "readmitted");
        let plain = add_collector(&mut blueprint, "plain");
        blueprint
            .connect_components("src", ["direct", "fwd"])
            .unwrap()
            .connect_components("fwd", "readmitted")
            .unwrap()
            .connect_components("src.other", "plain")
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(1, AdmissionMode::Admit, &schedule).with_wall_clock(source_wall),
            )
            .unwrap()
            .with_admission_point(
                "fwd",
                admission_config(2, AdmissionMode::Readmit, &schedule)
                    .with_wall_clock(ManualWallClock::new(AdmissionTime::from_unix_secs(500))),
            )
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        wait_until("every destination received its batches", || {
            direct.lock().unwrap().len() == 2
                && readmitted.lock().unwrap().len() == 2
                && plain.lock().unwrap().len() == 1
        })
        .await;
        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);

        assert_eq!(
            summarize(&direct),
            vec![(Some(100), Some(1), Some(7), 2), (Some(101), Some(1), Some(7), 1)]
        );
        assert_eq!(
            summarize(&readmitted),
            vec![(Some(500), Some(2), Some(7), 2), (Some(500), Some(2), Some(7), 1)]
        );
        // Outputs that are not admission points leave buffers as they are.
        assert_eq!(summarize(&plain), vec![(None, None, None, 3)]);
    }

    #[tokio::test]
    async fn a_tap_with_a_closed_input_gate_is_skipped_and_still_counts_what_it_was_offered() {
        // src -> {owner, tap}, where the tap's builder gives its input a gate, closed in one run and open in the
        // other. The tap receives buffers only while the gate is open, and its received-events series, like the
        // source's sent-events series, are the same in both runs.
        let received_total = |recorder: &saluki_metrics::test::TestRecorder, id: &'static str| {
            recorder.counter((
                "component_events_received_total",
                &[("component_id", id), ("component_type", "destination")][..],
            ))
        };
        let mut series = Vec::new();
        for open in [false, true] {
            let recorder = saluki_metrics::test::TestRecorder::default();
            let _local = metrics::set_default_local_recorder(&recorder);
            let gate = crate::topology::interconnect::InputGate::closed();
            if open {
                gate.open();
            }
            let script = (0..3)
                .map(|step| ScriptStep {
                    output: None,
                    events: step + 1,
                    at: AdmissionTime::from_unix_secs(100),
                })
                .collect();
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("test", &component_registry);
            blueprint
                .add_source("src", ScriptedSourceBuilder::new(script, ManualWallClock::default()))
                .unwrap();
            let owner = add_collector(&mut blueprint, "owner");
            let tap = add_gated_collector(&mut blueprint, "tap", Some(gate));
            blueprint
                .connect_components("src", ["owner", "tap"])
                .unwrap()
                .with_ambient_worker_pool()
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_resource_registry(ResourceRegistry::new());

            let (tx, handle) = spawn_supervised_blueprint(blueprint);
            wait_until("the owner received every batch", || owner.lock().unwrap().len() == 3).await;
            if open {
                wait_until("the open tap received every batch", || tap.lock().unwrap().len() == 3).await;
            }
            tx.send(()).expect("should send shutdown signal");
            assert!(join_topology(handle).await.is_ok());

            let tapped = tap.lock().unwrap().iter().map(EventsBuffer::len).collect::<Vec<_>>();
            assert_eq!(tapped, if open { vec![1, 2, 3] } else { vec![] }, "open: {}", open);
            series.push((received_total(&recorder, "owner"), received_total(&recorder, "tap")));
        }
        assert_eq!(series, [(Some(6), Some(6)), (Some(6), Some(6))]);
    }

    #[tokio::test]
    async fn a_running_topology_sends_heartbeats_from_idle_admission_points() {
        let schedule = steady_schedule(1);
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap();
        let received = add_collector(&mut blueprint, "out");
        let other = add_collector(&mut blueprint, "other_out");
        blueprint
            .connect_components("src", "out")
            .unwrap()
            .connect_components("src.other", "other_out")
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(3, AdmissionMode::Admit, &schedule)
                    .with_heartbeat(Duration::from_millis(10))
                    .with_wall_clock(ManualWallClock::new(AdmissionTime::from_unix_secs(1_000))),
            )
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        wait_until("the destination received heartbeats", || {
            received.lock().unwrap().len() >= 2
        })
        .await;

        // The driver stops with the component, so it never holds up shutdown.
        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);

        // Heartbeats carry the generation the schedule assigns, as data does.
        for heartbeat in summarize(&received) {
            assert_eq!(heartbeat, (Some(1_000), Some(3), Some(1), 0));
        }
        // Heartbeats only travel on the admission point's own output.
        assert!(other.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_running_topology_registers_its_points_and_sends_heartbeats_only_to_their_recipients() {
        // src (admit, point 4, heartbeats to `owner` only) -> owner
        //                                                  -> tap
        let schedule = steady_schedule(1);
        let registry = AdmissionRegistry::new();
        let script = vec![ScriptStep {
            output: None,
            events: 2,
            at: AdmissionTime::from_unix_secs(1_000),
        }];
        let wall = ManualWallClock::new(AdmissionTime::from_unix_secs(1_000));
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("src", ScriptedSourceBuilder::new(script, wall.clone()))
            .unwrap();
        let owner = add_collector(&mut blueprint, "owner");
        let tap = add_collector(&mut blueprint, "tap");
        blueprint
            .connect_components("src", ["owner", "tap"])
            .unwrap()
            .connect_components("src.other", "tap")
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(4, AdmissionMode::Admit, &schedule)
                    .with_heartbeat(Duration::from_millis(10))
                    .with_heartbeat_recipients([ComponentId::try_from("owner").unwrap()])
                    .with_registry(registry.clone())
                    .with_wall_clock(wall),
            )
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());
        assert!(registry.ids().is_empty(), "points register when the topology starts");

        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        wait_until("the owner received heartbeats", || owner.lock().unwrap().len() >= 3).await;
        assert_eq!(registry.ids(), vec![AdmissionPointId::new(4)]);
        let status = registry
            .status_within(AdmissionPointId::new(4), Duration::from_secs(1))
            .await
            .expect("the registered point reports its status");
        assert_eq!(status.last_admitted_at, Some(AdmissionTime::from_unix_secs(1_000)));

        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);
        // The registry does not keep a stopped topology's points.
        assert!(registry.ids().is_empty());

        // The owner received the data and then heartbeats; the tap received only the data.
        let owner = summarize(&owner);
        assert_eq!(owner[0], (Some(1_000), Some(4), Some(1), 2));
        assert!(owner[1..]
            .iter()
            .all(|heartbeat| *heartbeat == (Some(1_000), Some(4), Some(1), 0)));
        assert_eq!(summarize(&tap), vec![(Some(1_000), Some(4), Some(1), 2)]);
    }

    #[tokio::test]
    async fn a_heartbeat_recipient_that_the_output_does_not_feed_fails_the_build() {
        // `tap` is fed by `src.other`, not by `src`, whose admission point lists it.
        let schedule = steady_schedule(1);
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap();
        add_collector(&mut blueprint, "owner");
        add_collector(&mut blueprint, "tap");
        blueprint
            .connect_components("src", "owner")
            .unwrap()
            .connect_components("src.other", "tap")
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(1, AdmissionMode::Admit, &schedule)
                    .with_heartbeat(Duration::from_secs(1))
                    .with_heartbeat_recipients([ComponentId::try_from("tap").unwrap()]),
            )
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());

        let result = run_blueprint_until_exit(
            blueprint,
            RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)),
        )
        .await;
        match result {
            Err(SupervisorError::FailedToInitialize {
                source: InitializationError::Failed { source },
                ..
            }) => assert_eq!(
                format!("{:#}", source),
                "Admission point 'src' sends heartbeats to 'tap', which the output does not feed. List only components \
                 connected to the output as heartbeat recipients."
            ),
            other => panic!("the topology should fail to build, got: {:?}", other),
        }
    }

    /// A destination builder that declares `bytes` of minimum memory and builds a destination that drains its input.
    struct SizedDestinationBuilder {
        bytes: usize,
    }

    #[async_trait]
    impl DestinationBuilder for SizedDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingDestination))
        }
    }

    impl MemoryBounds for SizedDestinationBuilder {
        fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
            builder.minimum().with_array::<u8>("buffer", self.bytes);
        }
    }

    /// Returns the minimum memory every component of `blueprint` declares, with the interconnects.
    fn total_minimum_bytes(blueprint: &TopologyBlueprint) -> usize {
        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .component_registry
            .as_bounds()
            .total_minimum_required_bytes()
    }

    #[tokio::test]
    async fn a_replaced_component_keeps_its_connections_admission_points_and_takes_over_its_bounds() {
        // src (admit, point 1) -> fwd -> out, then `src` and `out` are replaced.
        let schedule = steady_schedule(3);
        let source_wall = ManualWallClock::default();
        let step = |events| ScriptStep {
            output: None,
            events,
            at: AdmissionTime::from_unix_secs(100),
        };
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source("src", ScriptedSourceBuilder::new(vec![step(5)], source_wall.clone()))
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_destination("out", SizedDestinationBuilder { bytes: 4096 })
            .unwrap()
            .connect_components_in_order(["src", "fwd", "out"])
            .unwrap()
            .with_admission_point(
                "src",
                admission_config(1, AdmissionMode::Admit, &schedule).with_wall_clock(source_wall.clone()),
            )
            .unwrap();
        let before = blueprint.snapshot().unwrap();
        let minimum_before = total_minimum_bytes(&blueprint);

        // The replacements declare what they replace, so the structure is unchanged, and the replaced destination's
        // bounds are gone with it.
        let received = Arc::new(StdMutex::new(Vec::new()));
        blueprint
            .replace_source("src", ScriptedSourceBuilder::new(vec![step(2), step(1)], source_wall))
            .unwrap()
            .replace_destination(
                "out",
                CollectingDestinationBuilder {
                    received: Arc::clone(&received),
                    gate: None,
                },
            )
            .unwrap();
        assert_eq!(blueprint.snapshot().unwrap(), before);
        assert_eq!(total_minimum_bytes(&blueprint), minimum_before - 4096);

        // The topology runs the replacements: the replacement source's batches reach the replacement destination
        // through the transform, stamped by the source's admission point.
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());
        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        wait_until("the replacement destination received both batches", || {
            received.lock().unwrap().len() == 2
        })
        .await;
        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);
        assert_eq!(
            summarize(&received),
            vec![(Some(100), Some(1), Some(3), 2), (Some(100), Some(1), Some(3), 1)]
        );
    }

    #[test]
    fn a_replacement_must_declare_what_it_replaces() {
        let mut blueprint = blueprint_with_every_component_type();
        let before = blueprint.snapshot().unwrap();

        // A source with fewer outputs, a destination of another input type, an encoder of another payload type, a
        // forwarder of another payload type, and a transform with another input type.
        let source = blueprint
            .replace_source("src", TestSourceBuilder::default_output(EventType::Metric))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            source,
            "Cannot replace source 'src': the replacement declares no input and outputs 'src' (event(Metric)), but the \
             component it replaces declares no input and outputs 'src' (event(Metric)), 'src.other' (event(Metric)). \
             A replacement must declare what it replaces, so that every connection stays valid."
        );
        let rejected = |error: Option<GenericError>| error.expect("the replacement is rejected").to_string();
        let shapes = |what: &str, id: &str, replacement: &str, existing: &str| {
            format!(
                "Cannot replace {} '{}': the replacement declares {}, but the component it replaces declares {}. A \
                 replacement must declare what it replaces, so that every connection stays valid.",
                what, id, replacement, existing
            )
        };
        assert_eq!(
            rejected(
                blueprint
                    .replace_destination("out", TestDestinationBuilder::with_input_type(EventType::Log))
                    .err()
            ),
            shapes(
                "destination",
                "out",
                "input event(Log) and no outputs",
                "input event(Metric) and no outputs"
            )
        );
        assert_eq!(
            rejected(
                blueprint
                    .replace_encoder(
                        "enc",
                        TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Raw),
                    )
                    .err()
            ),
            shapes(
                "encoder",
                "enc",
                "input event(Metric) and outputs 'enc' (payload(Raw))",
                "input event(Metric) and outputs 'enc' (payload(HTTP))"
            )
        );
        assert_eq!(
            rejected(
                blueprint
                    .replace_forwarder("fwdr", TestForwarderBuilder::with_input_type(PayloadType::Raw))
                    .err()
            ),
            shapes(
                "forwarder",
                "fwdr",
                "input payload(Raw) and no outputs",
                "input payload(HTTP) and no outputs"
            )
        );
        assert_eq!(
            rejected(
                blueprint
                    .replace_transform(
                        "fwd",
                        TestTransformBuilder::default_output(EventType::Log, EventType::Metric),
                    )
                    .err()
            ),
            shapes(
                "transform",
                "fwd",
                "input event(Log) and outputs 'fwd' (event(Metric))",
                "input event(Metric) and outputs 'fwd' (event(Metric))"
            )
        );

        // A component that is not there, or not of the replaced type, cannot be replaced.
        let missing = blueprint
            .replace_destination("nowhere", TestDestinationBuilder::with_input_type(EventType::Metric))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            missing,
            "Cannot replace destination 'nowhere': the blueprint has no such destination. Add it before replacing it."
        );
        let wrong_type = blueprint
            .replace_destination("fwd", TestDestinationBuilder::with_input_type(EventType::Metric))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            wrong_type,
            "Cannot replace destination 'fwd': the blueprint has no such destination. Add it before replacing it."
        );

        // Nothing changed, and a replacement that declares what it replaces is accepted.
        assert_eq!(blueprint.snapshot().unwrap(), before);
        blueprint
            .replace_encoder(
                "enc",
                TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Http),
            )
            .unwrap()
            .replace_forwarder("fwdr", TestForwarderBuilder::with_input_type(PayloadType::Http))
            .unwrap()
            .replace_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap();
        assert_eq!(blueprint.snapshot().unwrap(), before);
    }

    #[test]
    fn wrapping_an_encoder_keeps_its_builder_and_place() {
        let mut blueprint = blueprint_with_every_component_type();
        let before = blueprint.snapshot().unwrap();

        let missing = blueprint
            .wrap_encoder("nowhere", |encoder| encoder)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            missing,
            "Cannot wrap encoder 'nowhere': the blueprint has no such encoder. Add it before wrapping it."
        );

        blueprint.wrap_encoder("enc", |encoder| encoder).unwrap();
        assert_eq!(blueprint.snapshot().unwrap(), before);
    }

    #[tokio::test]
    async fn a_wrapped_encoder_is_built_by_its_builder_and_then_wrapped() {
        let mut blueprint = blueprint_with_every_component_type();
        let wraps = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&wraps);
        blueprint
            .wrap_encoder("enc", move |encoder| {
                counted.fetch_add(1, Ordering::SeqCst);
                encoder
            })
            .unwrap();

        let id = ComponentId::try_from("enc").unwrap();
        let builder = blueprint.state_mut().encoders.remove(&id).unwrap();
        assert_eq!(
            (builder.input_event_type(), builder.output_payload_type()),
            (EventType::Metric, PayloadType::Http)
        );
        let context = BuildContext::new(ComponentContext::test_encoder("enc"), ResourceRegistry::new());
        assert_eq!(
            wraps.load(Ordering::SeqCst),
            0,
            "nothing is wrapped before the encoder is built"
        );
        assert!(builder.build(context).await.is_ok());
        assert_eq!(wraps.load(Ordering::SeqCst), 1);
    }

    /// Returns each component of `blueprint`'s snapshot with the capacity of its input, if it has one.
    fn input_capacities(blueprint: &TopologyBlueprint) -> Vec<(String, Option<usize>)> {
        blueprint
            .snapshot()
            .unwrap()
            .components()
            .iter()
            .map(|component| {
                (
                    component.id().to_string(),
                    component.input_capacity().map(NonZeroUsize::get),
                )
            })
            .collect()
    }

    /// Returns the minimum and firm bytes `registry` declares in all.
    fn total_bytes(registry: &ComponentRegistry) -> (usize, usize) {
        let bounds = registry.as_bounds();
        (bounds.total_minimum_required_bytes(), bounds.total_firm_limit_bytes())
    }

    #[test]
    fn a_component_input_capacity_replaces_the_interconnect_capacity_for_that_component_alone() {
        let capacity = |capacity: usize| NonZeroUsize::new(capacity).unwrap();
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint.with_interconnect_capacity(capacity(4));
        blueprint
            .add_source(
                "src",
                ScriptedSourceBuilder::new(Vec::new(), ManualWallClock::default()),
            )
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_transform("wide", ForwardingTransformBuilder::new())
            .unwrap()
            .add_destination("out", TestDestinationBuilder::with_input_type(EventType::Metric))
            .unwrap()
            .add_encoder(
                "enc",
                TestEncoderBuilder::with_input_and_output_type(EventType::Metric, PayloadType::Http),
            )
            .unwrap()
            .connect_components_in_order(["src", "fwd", "wide", "out"])
            .unwrap()
            .connect_components("src.other", "enc")
            .unwrap();
        assert_eq!(blueprint.interconnect_capacity(), capacity(4));

        blueprint
            .with_component_input_capacity("wide", capacity(3))
            .unwrap()
            .with_component_input_capacity("wide", capacity(10))
            .unwrap()
            .with_component_input_capacity("out", capacity(7))
            .unwrap();
        // The last capacity set is the component's; the others keep the topology's, and a source has no input.
        let expected = |default: usize| {
            vec![
                ("enc".to_string(), Some(default)),
                ("fwd".to_string(), Some(default)),
                ("out".to_string(), Some(7)),
                ("src".to_string(), None),
                ("wide".to_string(), Some(10)),
            ]
        };
        assert_eq!(input_capacities(&blueprint), expected(4));

        // The bounds count each transform's and destination's interconnect at its own capacity, and the in-flight
        // buffers of the source and of each transform besides (the encoder's interconnect is not counted, as before).
        let per_buffer =
            std::mem::size_of::<EventsBuffer>() + (std::mem::size_of::<Event>() * DEFAULT_EVENTS_BUFFER_CAPACITY);
        let bytes = |interconnects: usize| {
            let minimum = interconnects * std::mem::size_of::<EventsBuffer>();
            (minimum, minimum + (interconnects + 1 + 2) * per_buffer)
        };
        assert_eq!(total_bytes(&component_registry), bytes(4 + 10 + 7));

        // A new interconnect capacity applies to every component without one of its own.
        blueprint.with_interconnect_capacity(capacity(6));
        assert_eq!(input_capacities(&blueprint), expected(6));
        assert_eq!(total_bytes(&component_registry), bytes(6 + 10 + 7));

        // A replaced transform keeps its capacity.
        blueprint
            .replace_transform("wide", ForwardingTransformBuilder::new())
            .unwrap();
        assert_eq!(input_capacities(&blueprint), expected(6));
        assert_eq!(total_bytes(&component_registry), bytes(6 + 10 + 7));
    }

    #[test]
    fn only_transforms_and_destinations_can_have_an_input_capacity_of_their_own() {
        let mut blueprint = blueprint_with_every_component_type();
        let before = input_capacities(&blueprint);
        let capacity = NonZeroUsize::new(512).unwrap();
        let only = |what: &str, id: &str| {
            format!(
                "Cannot set the input capacity of {} '{}': only transforms and destinations can have an input \
                 capacity of their own, since the topology's memory bounds account for their input interconnects \
                 only.",
                what, id
            )
        };
        let rejected = [
            (
                "missing",
                "Cannot set the input capacity of 'missing': the blueprint has no such component. Add the component \
                 before setting its input capacity."
                    .to_string(),
            ),
            ("src", only("source", "src")),
            ("relay", only("relay", "relay")),
            ("dec", only("decoder", "dec")),
            ("enc", only("encoder", "enc")),
            ("fwdr", only("forwarder", "fwdr")),
        ];
        for (id, expected) in rejected {
            let error = blueprint
                .with_component_input_capacity(id, capacity)
                .map(|_| ())
                .expect_err(id);
            assert_eq!(error.to_string(), expected, "{}", id);
        }
        let invalid = blueprint
            .with_component_input_capacity("not an id", capacity)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            invalid.starts_with("Cannot set the input capacity of 'not an id': it is not a valid component ID:"),
            "{}",
            invalid
        );
        assert_eq!(input_capacities(&blueprint), before);

        // Transforms and destinations can.
        blueprint
            .with_component_input_capacity("fwd", capacity)
            .unwrap()
            .with_component_input_capacity("out", capacity)
            .unwrap();
        let changed = input_capacities(&blueprint)
            .into_iter()
            .filter(|(id, input)| !before.contains(&(id.clone(), *input)))
            .collect::<Vec<_>>();
        assert_eq!(
            changed,
            vec![("fwd".to_string(), Some(512)), ("out".to_string(), Some(512))]
        );
    }

    /// A source that sends `batches` buffers of one metric each, counting each send once it completes, and then runs
    /// until shutdown.
    struct CountingSource {
        batches: usize,
        sent: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Source for CountingSource {
        async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            let shutdown = context.take_shutdown_handle();
            for value in 0..self.batches {
                let mut buffer = EventsBuffer::with_capacity(1);
                assert!(buffer
                    .try_push(Event::Metric(Metric::gauge("m", value as f64)))
                    .is_none());
                context.dispatcher().dispatch(buffer).await?;
                self.sent.fetch_add(1, Ordering::SeqCst);
            }
            shutdown.await;
            Ok(())
        }
    }

    struct CountingSourceBuilder {
        outputs: Vec<OutputDefinition<EventType>>,
        batches: usize,
        sent: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SourceBuilder for CountingSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
            Ok(Box::new(CountingSource {
                batches: self.batches,
                sent: Arc::clone(&self.sent),
            }))
        }
    }

    impl MemoryBounds for CountingSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that reads nothing until its gate opens, and then counts the events it receives until its input
    /// closes.
    struct GatedDestination {
        gate: Arc<tokio::sync::Semaphore>,
        received: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Destination for GatedDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            self.gate.acquire().await.expect("the gate is never closed").forget();
            while let Some(buffer) = context.events().next().await {
                self.received.fetch_add(buffer.len(), Ordering::SeqCst);
            }
            Ok(())
        }
    }

    struct GatedDestinationBuilder {
        gate: Arc<tokio::sync::Semaphore>,
        received: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DestinationBuilder for GatedDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(GatedDestination {
                gate: Arc::clone(&self.gate),
                received: Arc::clone(&self.received),
            }))
        }
    }

    impl MemoryBounds for GatedDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_topology_takes_what_its_input_capacities_hold_before_its_source_waits() {
        // src -> fwd (input capacity 3) -> out (input capacity 5), with `out` reading nothing until its gate opens.
        // The source sends until both inputs are full and the transform holds one more buffer, waiting to send it.
        let batches = 20;
        let sent = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let capacity = |capacity: usize| NonZeroUsize::new(capacity).unwrap();
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("test", &component_registry);
        blueprint
            .add_source(
                "src",
                CountingSourceBuilder {
                    outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                    batches,
                    sent: Arc::clone(&sent),
                },
            )
            .unwrap()
            .add_transform("fwd", ForwardingTransformBuilder::new())
            .unwrap()
            .add_destination(
                "out",
                GatedDestinationBuilder {
                    gate: Arc::clone(&gate),
                    received: Arc::clone(&received),
                },
            )
            .unwrap()
            .connect_components_in_order(["src", "fwd", "out"])
            .unwrap()
            .with_component_input_capacity("fwd", capacity(3))
            .unwrap()
            .with_component_input_capacity("out", capacity(5))
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_ambient_worker_pool();

        let (tx, handle) = spawn_supervised_blueprint(blueprint);
        let held = 3 + 1 + 5;
        wait_until("the source sent what the topology holds", || {
            sent.load(Ordering::SeqCst) == held
        })
        .await;
        // The clock is paused: the sleep ends once every task waits, so nothing more is sent after it.
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(sent.load(Ordering::SeqCst), held);
        assert_eq!(received.load(Ordering::SeqCst), 0);

        // Once the destination reads, everything arrives.
        gate.add_permits(1);
        wait_until("the destination received every batch", || {
            received.load(Ordering::SeqCst) == batches
        })
        .await;
        assert_eq!(sent.load(Ordering::SeqCst), batches);
        tx.send(()).expect("should send shutdown signal");
        let result = join_topology(handle).await;
        assert!(result.is_ok(), "topology should shut down cleanly, got: {:?}", result);
    }

    #[tokio::test]
    async fn with_explicit_worker_pool_selects_explicit() {
        let mut blueprint = blueprint_with_components();
        blueprint.with_explicit_worker_pool(tokio::runtime::Handle::current());

        let guard = blueprint.build_state.lock().expect("topology blueprint mutex poisoned");
        let config = &guard
            .as_ref()
            .expect("topology blueprint already initialized")
            .worker_pool_config;
        assert!(matches!(config, WorkerPoolConfiguration::Explicit(_)));
    }
}

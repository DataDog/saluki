use std::{
    collections::HashMap,
    num::{NonZeroU16, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use saluki_common::resource_tracking::ResourceGroupToken;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{runtime::Handle, select, sync::mpsc};
use tracing::debug;

use crate::accounting::{ComponentRegistry, MemoryLimiter};

/// Period over which the topology supervisor's restart intensity is measured.
///
/// The topology supervisor uses a restart intensity of 0, so any component failure shuts the topology
/// down on the first occurrence; the period only needs to be a sane non-zero value.
const TOPOLOGY_RESTART_PERIOD: Duration = Duration::from_secs(5);

/// The number of worker threads of the dedicated worker pool a topology builds by default.
pub const DEFAULT_WORKER_POOL_THREADS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

/// Returns the number of worker threads for the dedicated worker pool of a process whose primary runtime has
/// `primary_workers` worker threads: the default of [`DEFAULT_WORKER_POOL_THREADS`] (8), or one per primary worker
/// when there are more.
///
/// A topology whose components spawn compute-bound subtasks that grow in number with the cores, such as one encoder
/// stream per output fed by components with lanes, passes this to
/// [`TopologyBlueprint::with_dedicated_worker_pool`][super::TopologyBlueprint::with_dedicated_worker_pool], so that
/// those subtasks are never limited to fewer threads than the default gives them, nor to 8 threads on a larger host.
pub fn worker_pool_threads(primary_workers: usize) -> NonZeroUsize {
    NonZeroUsize::new(primary_workers).map_or(DEFAULT_WORKER_POOL_THREADS, |workers| {
        workers.max(DEFAULT_WORKER_POOL_THREADS)
    })
}

/// Configuration for the worker pool used by the topology.
///
/// This controls where tasks spawned by components themselves are executed. When components
/// have heavy, compute-bound tasks that they need to execute, they should generally be spawned on the worker pool (also known as the "global thread pool") to
/// avoid scheduling contention/starvation with the core component tasks.
pub enum WorkerPoolConfiguration {
    /// Use the ambient Tokio runtime (that's, `Handle::current()`).
    ///
    /// Component subtasks are spawned on whatever runtime is currently active.
    /// Useful when the topology is embedded in an application that already
    /// manages its own runtime, such as a Lambda extension.
    Ambient,

    /// Create a dedicated, multi-threaded Tokio runtime with `threads` worker threads.
    ///
    /// This is the default behavior, with [`DEFAULT_WORKER_POOL_THREADS`] (8) threads.
    Dedicated {
        /// The number of worker threads.
        threads: NonZeroUsize,
    },

    /// Use an externally provided runtime.
    ///
    /// Component subtasks are spawned on the runtime associated with the given handle.
    Explicit(Handle),
}

use super::component_worker::{
    ComponentWorker, DecoderRunnable, DestinationRunnable, EncoderRunnable, ForwarderRunnable, RelayRunnable,
    RunnableComponent, SourceRunnable, TransformRunnable,
};
use super::{
    graph::{Graph, OutboundEdge},
    ComponentId, ComponentOutputId, EventsBuffer, EventsConsumer, LaneEdge, OutputName, PayloadsConsumer,
    TypedComponentId,
};
use crate::execution::{AdmissionPoint, AdmissionPointConfig};
use crate::health::{Health, HealthRegistry};
use crate::runtime::state::DataspaceRegistry;
use crate::runtime::{self, AutoShutdown, RestartMode, RestartStrategy, Supervisor};
use crate::support::SubsystemIdentifier;
use crate::topology::ids::get_component_relative_identifier;
use crate::topology::interconnect::{Consumer, Dispatchable, InputGate, ReceivedMetrics};
use crate::{
    components::{
        decoders::{Decoder, DecoderContext},
        destinations::{Destination, DestinationContext},
        encoders::{Encoder, EncoderContext},
        forwarders::{Forwarder, ForwarderContext},
        relays::{Relay, RelayContext},
        sources::{Source, SourceContext},
        transforms::{Transform, TransformContext},
        ComponentContext, ComponentType,
    },
    topology::{context::TopologyContext, EventsDispatcher, PayloadsBuffer, PayloadsDispatcher},
};

/// A component output marked as an admission point, with the configuration of its point.
#[derive(Clone, Debug)]
pub(crate) struct AdmissionPointBinding {
    /// The output that is the admission point.
    pub(crate) output: ComponentOutputId,

    /// The type of the component that owns the output.
    pub(crate) component_type: ComponentType,

    /// The configuration of the admission point.
    pub(crate) config: AdmissionPointConfig,
}

/// A built topology.
///
/// Built topologies represent a topology blueprint where each configured component, along with their associated
/// connections to other components, was validated and built successfully.
///
/// A built topology is spawned internally via [`spawn_inner`][Self::spawn_inner] when the owning
/// [`TopologyBlueprint`][super::TopologyBlueprint] is initialized by a supervisor.
pub(crate) struct BuiltTopology {
    name: String,
    topology_id: SubsystemIdentifier,
    graph: Graph,
    sources: HashMap<ComponentContext, (Box<dyn Source + Send>, ComponentRegistry)>,
    relays: HashMap<ComponentContext, (Box<dyn Relay + Send>, ComponentRegistry)>,
    decoders: HashMap<ComponentContext, (Box<dyn Decoder + Send>, ComponentRegistry)>,
    transforms: HashMap<ComponentContext, (Box<dyn Transform + Send>, ComponentRegistry)>,
    destinations: HashMap<ComponentContext, (Box<dyn Destination + Send>, ComponentRegistry)>,
    encoders: HashMap<ComponentContext, (Box<dyn Encoder + Send>, ComponentRegistry)>,
    forwarders: HashMap<ComponentContext, (Box<dyn Forwarder + Send>, ComponentRegistry)>,
    admission_points: Vec<AdmissionPointBinding>,
    component_token: ResourceGroupToken,
    interconnect_capacity: NonZeroUsize,
    input_capacities: HashMap<ComponentId, NonZeroUsize>,
    input_gates: HashMap<ComponentId, InputGate>,
    input_buffer_events: HashMap<ComponentId, usize>,
    worker_pool_config: WorkerPoolConfiguration,
}

impl BuiltTopology {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        name: String, topology_id: SubsystemIdentifier, graph: Graph,
        sources: HashMap<ComponentContext, (Box<dyn Source + Send>, ComponentRegistry)>,
        relays: HashMap<ComponentContext, (Box<dyn Relay + Send>, ComponentRegistry)>,
        decoders: HashMap<ComponentContext, (Box<dyn Decoder + Send>, ComponentRegistry)>,
        transforms: HashMap<ComponentContext, (Box<dyn Transform + Send>, ComponentRegistry)>,
        destinations: HashMap<ComponentContext, (Box<dyn Destination + Send>, ComponentRegistry)>,
        encoders: HashMap<ComponentContext, (Box<dyn Encoder + Send>, ComponentRegistry)>,
        forwarders: HashMap<ComponentContext, (Box<dyn Forwarder + Send>, ComponentRegistry)>,
        admission_points: Vec<AdmissionPointBinding>, component_token: ResourceGroupToken,
        interconnect_capacity: NonZeroUsize, input_capacities: HashMap<ComponentId, NonZeroUsize>,
        input_gates: HashMap<ComponentId, InputGate>, input_buffer_events: HashMap<ComponentId, usize>,
        worker_pool_config: WorkerPoolConfiguration,
    ) -> Self {
        Self {
            name,
            topology_id,
            graph,
            sources,
            relays,
            decoders,
            transforms,
            destinations,
            encoders,
            forwarders,
            admission_points,
            component_token,
            interconnect_capacity,
            input_capacities,
            input_gates,
            input_buffer_events,
            worker_pool_config,
        }
    }

    fn resolve_worker_pool_handle(&self) -> Result<Handle, GenericError> {
        match &self.worker_pool_config {
            WorkerPoolConfiguration::Ambient => Ok(Handle::current()),
            WorkerPoolConfiguration::Dedicated { threads } => {
                let thread_pool = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(threads.get())
                    .enable_all()
                    .build()
                    .error_context("Failed to build asynchronous thread pool runtime.")?;
                let handle = thread_pool.handle().clone();

                std::thread::spawn(move || {
                    thread_pool.block_on(std::future::pending::<()>());
                });

                Ok(handle)
            }
            WorkerPoolConfiguration::Explicit(handle) => Ok(handle.clone()),
        }
    }

    /// Spawns the topology.
    ///
    /// Each component runs as the sole, significant child of its own dedicated [`Supervisor`], and all of
    /// those per-component supervisors are children of a single topology supervisor, which is returned
    /// (configured but not yet running). The caller runs it to start the topology.
    ///
    /// The worker pool used by components to spawn compute-heavy subtasks is determined by the
    /// [`WorkerPoolConfiguration`] carried over from the blueprint (defaulting to a dedicated,
    /// multi-threaded Tokio runtime with 8 threads).
    ///
    /// `shutdown_timeout` bounds how long each component is given to stop gracefully before it is aborted.
    ///
    /// ## Errors
    ///
    /// If an error occurs while building the topology supervisor, an error is returned.
    pub(crate) async fn spawn_inner(
        self, health_registry: &HealthRegistry, memory_limiter: MemoryLimiter, dataspace: DataspaceRegistry,
        shutdown_timeout: Duration,
    ) -> Result<Supervisor, GenericError> {
        let _guard = self.component_token.enter();

        let thread_pool_handle = self.resolve_worker_pool_handle()?;
        let topology_context = TopologyContext::new(
            Arc::from(self.name.as_str()),
            memory_limiter,
            health_registry.clone(),
            thread_pool_handle,
            dataspace,
        );

        // Build our interconnects, which we'll grab from piecemeal as we build our components.
        let mut interconnects = ComponentInterconnects::from_graph(
            self.interconnect_capacity,
            self.input_capacities,
            self.input_gates,
            self.input_buffer_events,
            &self.topology_id,
            &self.graph,
            &self.admission_points,
        )
        .error_context("Failed to build component interconnects.")?;

        // The topology supervisor parents one dedicated supervisor per component. Shutdown signals every
        // component at once, so sources stop immediately and the downstream cascade drains in parallel
        // (bounded by `shutdown_timeout`) rather than one component at a time. A `OneForOne` strategy with
        // intensity 0 turns any single component failure into a supervisor shutdown on the first
        // occurrence, which fails the topology as a whole -- preserving the previous behavior where a
        // component finishing unexpectedly brings the topology down.
        let mut topology_sup = Supervisor::new(self.topology_id.to_string())?
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, TOPOLOGY_RESTART_PERIOD));

        // Build our sources.
        for (component_context, (component, component_registry)) in self.sources {
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let mut component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, || SourceRunnable {
                    component,
                    context: SourceContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                    ),
                })?;
            add_heartbeat_drivers(
                &mut component_sup,
                interconnects.take_heartbeat_drivers(&component_context),
            );
            topology_sup.add_worker(component_sup);
        }

        // Build our relays.
        for (component_context, (component, component_registry)) in self.relays {
            let dispatcher = interconnects.take_payloads_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup = build_component_supervisor(&component_context, shutdown_timeout, || RelayRunnable {
                component,
                context: RelayContext::new(
                    &topology_context,
                    &component_context,
                    component_registry,
                    health_handle,
                    dispatcher,
                ),
            })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our decoders.
        for (component_context, (component, component_registry)) in self.decoders {
            let consumer = interconnects.take_payloads_consumer(&component_context)?;
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let mut component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, || DecoderRunnable {
                    component,
                    context: DecoderContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        consumer,
                    ),
                })?;
            add_heartbeat_drivers(
                &mut component_sup,
                interconnects.take_heartbeat_drivers(&component_context),
            );
            topology_sup.add_worker(component_sup);
        }

        // Build our transforms, each with one input consumer per lane.
        for (component_context, (component, component_registry)) in self.transforms {
            let consumers = interconnects.take_events_lane_consumers(&component_context)?;
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let mut component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, || TransformRunnable {
                    component,
                    context: TransformContext::with_lane_inputs(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        consumers,
                    ),
                })?;
            add_heartbeat_drivers(
                &mut component_sup,
                interconnects.take_heartbeat_drivers(&component_context),
            );
            topology_sup.add_worker(component_sup);
        }

        // Build our destinations.
        for (component_context, (component, component_registry)) in self.destinations {
            let consumer = interconnects.take_events_consumer(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, || DestinationRunnable {
                    component,
                    context: DestinationContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        consumer,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our encoders.
        for (component_context, (component, component_registry)) in self.encoders {
            let consumer = interconnects.take_events_consumer(&component_context)?;
            let dispatcher = interconnects.take_payloads_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup = build_component_supervisor(&component_context, shutdown_timeout, || EncoderRunnable {
                component,
                context: EncoderContext::new(
                    &topology_context,
                    &component_context,
                    component_registry,
                    health_handle,
                    dispatcher,
                    consumer,
                ),
            })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our forwarders.
        for (component_context, (component, component_registry)) in self.forwarders {
            let consumer = interconnects.take_payloads_consumer(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, || ForwarderRunnable {
                    component,
                    context: ForwarderContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        consumer,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        Ok(topology_sup)
    }
}

/// The input channels of a component that consumes events: one per input lane, in lane order.
struct EventsInput {
    senders: Vec<mpsc::Sender<EventsBuffer>>,
    consumers: Vec<EventsConsumer>,
}

impl EventsInput {
    /// Creates `lanes` channels, each of `capacity` buffers, whose consumers record what they receive as received by
    /// the component `component_context`, and check, in a debug build, that each buffer has storage for at most
    /// `buffer_events` events, if the component declares that its buffers hold fewer than an events buffer.
    fn new(
        component_context: &ComponentContext, capacity: NonZeroUsize, lanes: NonZeroU16, buffer_events: Option<usize>,
    ) -> Self {
        let (senders, consumers) = (0..lanes.get())
            .map(|_| {
                let (sender, consumer) = build_consumer_pair(component_context.clone(), capacity);
                match buffer_events {
                    Some(events) => (sender, consumer.with_max_allocated_items(events)),
                    None => (sender, consumer),
                }
            })
            .unzip();
        Self { senders, consumers }
    }
}

struct ComponentInterconnects {
    interconnect_capacity: NonZeroUsize,
    input_capacities: HashMap<ComponentId, NonZeroUsize>,
    input_gates: HashMap<ComponentId, InputGate>,
    input_buffer_events: HashMap<ComponentId, usize>,
    topology_id: SubsystemIdentifier,
    events_dispatchers: HashMap<ComponentContext, EventsDispatcher>,
    payloads_dispatchers: HashMap<ComponentContext, PayloadsDispatcher>,
    events_consumers: HashMap<ComponentContext, EventsInput>,
    payloads_consumers: HashMap<ComponentContext, (mpsc::Sender<PayloadsBuffer>, PayloadsConsumer)>,
    heartbeat_drivers: HashMap<ComponentContext, Vec<Arc<AdmissionPoint>>>,
}

impl ComponentInterconnects {
    /// Creates the interconnects of `graph`: each component's input holds `interconnect_capacity` items, or the
    /// component's own capacity in `input_capacities`, in each of its input lanes, and the senders to a component with
    /// a gate in `input_gates` skip it while the gate is closed. A component in `input_buffer_events` declares that
    /// each buffer it receives has storage for at most that many events, which its consumers check in a debug build.
    fn from_graph(
        interconnect_capacity: NonZeroUsize, input_capacities: HashMap<ComponentId, NonZeroUsize>,
        input_gates: HashMap<ComponentId, InputGate>, input_buffer_events: HashMap<ComponentId, usize>,
        topology_id: &SubsystemIdentifier, graph: &Graph, admission_points: &[AdmissionPointBinding],
    ) -> Result<Self, GenericError> {
        let mut interconnects = Self {
            interconnect_capacity,
            input_capacities,
            input_gates,
            input_buffer_events,
            topology_id: topology_id.clone(),
            events_dispatchers: HashMap::new(),
            payloads_dispatchers: HashMap::new(),
            events_consumers: HashMap::new(),
            payloads_consumers: HashMap::new(),
            heartbeat_drivers: HashMap::new(),
        };

        interconnects.generate_interconnects(graph)?;
        interconnects.set_admission_points(admission_points)?;
        Ok(interconnects)
    }

    /// Creates the admission point of every marked output, attaches it to the output's dispatcher, and registers it in
    /// the registry its configuration names, if any.
    ///
    /// This runs after every sender is attached, since each point captures its output's senders for heartbeats.
    fn set_admission_points(&mut self, admission_points: &[AdmissionPointBinding]) -> Result<(), GenericError> {
        for binding in admission_points {
            let component_context =
                ComponentContext::new(&self.topology_id, binding.output.component_id(), binding.component_type);
            let dispatcher = self.events_dispatchers.get_mut(&component_context).ok_or_else(|| {
                generic_error!(
                    "No events dispatcher found for admission point output '{}'.",
                    binding.output
                )
            })?;

            let point = Arc::new(AdmissionPoint::new(binding.config.clone()));
            dispatcher
                .set_admission_point(&binding.output.output(), Arc::clone(&point))
                .with_error_context(|| format!("Failed to set the admission point of output '{}'.", binding.output))?;
            if let Some(registry) = binding.config.registry() {
                registry.register(&point).with_error_context(|| {
                    format!("Failed to register the admission point of output '{}'.", binding.output)
                })?;
            }

            if point.heartbeat_period().is_some() {
                self.heartbeat_drivers.entry(component_context).or_default().push(point);
            }
        }

        Ok(())
    }

    fn take_heartbeat_drivers(&mut self, component_context: &ComponentContext) -> Vec<Arc<AdmissionPoint>> {
        self.heartbeat_drivers.remove(component_context).unwrap_or_default()
    }

    fn take_events_dispatcher(
        &mut self, component_context: &ComponentContext,
    ) -> Result<EventsDispatcher, GenericError> {
        self.events_dispatchers.remove(component_context).ok_or_else(|| {
            generic_error!(
                "No events dispatcher found for {} component '{}'",
                component_context.component_type().as_str(),
                component_context.component_id()
            )
        })
    }

    fn take_payloads_dispatcher(
        &mut self, component_context: &ComponentContext,
    ) -> Result<PayloadsDispatcher, GenericError> {
        self.payloads_dispatchers.remove(component_context).ok_or_else(|| {
            generic_error!(
                "No payloads dispatcher found for {} component '{}'",
                component_context.component_type().as_str(),
                component_context.component_id()
            )
        })
    }

    /// Takes the one input consumer of a component that consumes events and has one lane, such as a destination.
    fn take_events_consumer(&mut self, component_context: &ComponentContext) -> Result<EventsConsumer, GenericError> {
        let mut consumers = self.take_events_lane_consumers(component_context)?;
        match consumers.len() {
            1 => Ok(consumers.remove(0)),
            lanes => Err(generic_error!(
                "{} component '{}' has {} input lanes, but only transforms can have more than one.",
                component_context.component_type().as_str(),
                component_context.component_id(),
                lanes
            )),
        }
    }

    /// Takes the input consumers of a component that consumes events, one per input lane, in lane order.
    fn take_events_lane_consumers(
        &mut self, component_context: &ComponentContext,
    ) -> Result<Vec<EventsConsumer>, GenericError> {
        self.events_consumers
            .remove(component_context)
            .map(|input| input.consumers)
            .ok_or_else(|| {
                generic_error!(
                    "No events consumer found for {} component '{}'",
                    component_context.component_type().as_str(),
                    component_context.component_id()
                )
            })
    }

    fn take_payloads_consumer(
        &mut self, component_context: &ComponentContext,
    ) -> Result<PayloadsConsumer, GenericError> {
        self.payloads_consumers
            .remove(component_context)
            .map(|(_, consumer)| consumer)
            .ok_or_else(|| {
                generic_error!(
                    "No payloads consumer found for {} component '{}'",
                    component_context.component_type().as_str(),
                    component_context.component_id()
                )
            })
    }

    fn generate_interconnects(&mut self, graph: &Graph) -> Result<(), GenericError> {
        // Collect and iterate over each outbound edge in the topology graph.
        //
        // For each upstream component ("from" side of the edge), we attach each downstream component ("to" side of the edge) to it,
        // creating the relevant dispatcher or consumer if necessary.
        let outbound_edges = graph.get_outbound_directed_edges();
        for (upstream_id, output_map) in outbound_edges {
            match upstream_id.component_type() {
                ComponentType::Source | ComponentType::Decoder | ComponentType::Transform => {
                    self.generate_event_interconnect(graph, upstream_id, output_map)?;
                }
                ComponentType::Relay | ComponentType::Encoder => {
                    self.generate_payload_interconnect(upstream_id, output_map)?
                }
                _ => panic!(
                    "Only sources, decoders, transforms, relays, and encoders can dispatch events/payloads to downstream components."
                ),
            }
        }

        Ok(())
    }

    fn generate_event_interconnect(
        &mut self, graph: &Graph, upstream_id: TypedComponentId, output_map: HashMap<OutputName, Vec<OutboundEdge>>,
    ) -> Result<(), GenericError> {
        for (upstream_output_id, downstream) in output_map {
            let mut senders = Vec::new();
            for (downstream_id, routing) in downstream {
                debug!(
                    upstream_id = %upstream_id.component_id(),
                    %upstream_output_id,
                    downstream_id = %downstream_id.component_id(),
                    ?routing,
                    "Adding dispatcher output."
                );
                let component_id = downstream_id.component_id().clone();
                let gate = self.input_gates.get(&component_id).cloned();
                let lanes = graph.input_lanes(&component_id);
                let (lane_senders, received) = self.get_or_create_events_senders(downstream_id, lanes);
                // An edge into a component with one lane records no routing: it leads to that lane.
                let edge = routing.unwrap_or(LaneEdge::Fixed(0));
                senders.push((component_id, lane_senders, edge, gate.map(|gate| (gate, received))));
            }

            let dispatcher = self.get_or_create_events_dispatcher(upstream_id.clone());
            dispatcher.add_output(upstream_output_id.clone())?;

            // Each sender records the component it feeds, so that an admission point on the output can send its
            // heartbeats to some of them only, and one sender per lane of that component, which the output picks
            // among by the edge's routing. A sender to a component with an input gate skips it while the gate is
            // closed, and counts what it skipped in the counters of that component's consumer.
            for (component_id, lane_senders, edge, gate) in senders {
                match gate {
                    Some((gate, received)) => dispatcher.attach_gated_lanes_to_output(
                        &upstream_output_id,
                        component_id,
                        lane_senders,
                        edge,
                        gate,
                        received,
                    )?,
                    None => dispatcher.attach_component_lanes_to_output(
                        &upstream_output_id,
                        component_id,
                        lane_senders,
                        edge,
                    )?,
                }
            }
        }

        Ok(())
    }

    fn generate_payload_interconnect(
        &mut self, upstream_id: TypedComponentId, output_map: HashMap<OutputName, Vec<OutboundEdge>>,
    ) -> Result<(), GenericError> {
        for (upstream_output_id, downstream) in output_map {
            let mut senders = Vec::new();
            // Components that consume payloads have one lane.
            for (downstream_id, _) in downstream {
                debug!(upstream_id = %upstream_id.component_id(), %upstream_output_id, downstream_id = %downstream_id.component_id(), "Adding dispatcher output.");
                let sender = self.get_or_create_payloads_sender(downstream_id);
                senders.push(sender);
            }

            let dispatcher = self.get_or_create_payloads_dispatcher(upstream_id.clone());
            dispatcher.add_output(upstream_output_id.clone())?;

            for sender in senders {
                dispatcher.attach_sender_to_output(&upstream_output_id, sender)?;
            }
        }

        Ok(())
    }

    fn get_or_create_events_dispatcher(&mut self, component_id: TypedComponentId) -> &mut EventsDispatcher {
        let (component_id, component_type) = component_id.into_parts();
        let component_context = ComponentContext::new(&self.topology_id, component_id.clone(), component_type);

        match component_type {
            ComponentType::Source | ComponentType::Decoder | ComponentType::Transform => self
                .events_dispatchers
                .entry(component_context.clone())
                .or_insert_with(|| EventsDispatcher::new(component_context)),
            _ => {
                panic!("Only sources, decoders, and transforms can dispatch events to downstream components.")
            }
        }
    }

    /// Returns the capacity of a component's input: its own, or else the interconnect capacity.
    fn input_capacity(&self, component_id: &ComponentId) -> NonZeroUsize {
        self.input_capacities
            .get(component_id)
            .copied()
            .unwrap_or(self.interconnect_capacity)
    }

    /// Returns a sender to each of the `lanes` input lanes of a component that consumes events, in lane order, and the
    /// counters its consumers record what they receive in.
    fn get_or_create_events_senders(
        &mut self, component_id: TypedComponentId, lanes: NonZeroU16,
    ) -> (Vec<mpsc::Sender<EventsBuffer>>, ReceivedMetrics) {
        let (component_id, component_type) = component_id.into_parts();
        let interconnect_capacity = self.input_capacity(&component_id);
        let buffer_events = self.input_buffer_events.get(&component_id).copied();
        let component_context = ComponentContext::new(&self.topology_id, component_id, component_type);

        let input = match component_type {
            ComponentType::Transform | ComponentType::Destination | ComponentType::Encoder => self
                .events_consumers
                .entry(component_context.clone())
                .or_insert_with(|| EventsInput::new(&component_context, interconnect_capacity, lanes, buffer_events)),
            _ => panic!("Only transforms, destinations, and encoders can consume events."),
        };

        // Every lane's consumer records into the component's one set of counters.
        (input.senders.clone(), input.consumers[0].received_metrics())
    }

    fn get_or_create_payloads_dispatcher(&mut self, component_id: TypedComponentId) -> &mut PayloadsDispatcher {
        let (component_id, component_type) = component_id.into_parts();
        let component_context = ComponentContext::new(&self.topology_id, component_id.clone(), component_type);

        match component_type {
            ComponentType::Relay | ComponentType::Encoder => self
                .payloads_dispatchers
                .entry(component_context.clone())
                .or_insert_with(|| PayloadsDispatcher::new(component_context)),
            _ => {
                panic!("Only relays and encoders can dispatch payloads to downstream components.")
            }
        }
    }

    fn get_or_create_payloads_sender(&mut self, component_id: TypedComponentId) -> mpsc::Sender<PayloadsBuffer> {
        let (component_id, component_type) = component_id.into_parts();
        let interconnect_capacity = self.input_capacity(&component_id);
        let component_context = ComponentContext::new(&self.topology_id, component_id, component_type);

        let (sender, _) = match component_type {
            ComponentType::Decoder | ComponentType::Forwarder => self
                .payloads_consumers
                .entry(component_context.clone())
                .or_insert_with(|| build_consumer_pair(component_context, interconnect_capacity)),
            _ => panic!("Only decoders and forwarders can consume payloads."),
        };

        sender.clone()
    }
}

fn build_consumer_pair<T: Dispatchable>(
    component_context: ComponentContext, interconnect_capacity: NonZeroUsize,
) -> (mpsc::Sender<T>, Consumer<T>) {
    let (sender, receiver) = mpsc::channel(interconnect_capacity.get());
    let consumer = Consumer::new(component_context, receiver);
    (sender, consumer)
}

/// Builds a dedicated supervisor for a single component.
///
/// For every component, we follow the pattern of creating a dedicated supervisor where the component task itself is the
/// sole (initial) child process, set as a significant child such that when it terminates, the supervisor shuts down as
/// well. This provides us with a decent approximation of structured concurrency for components and their subtasks.
fn build_component_supervisor<C, F>(
    context: &ComponentContext, shutdown_timeout: Duration, make_runnable: F,
) -> Result<Supervisor, GenericError>
where
    C: RunnableComponent,
    F: FnOnce() -> C,
{
    // The supervisor owns the shutdown deadline for its whole subtree: neither the component nor anything it spawns
    // carries a timeout of its own, and the budget bounds all of them together. Whatever is still running when it
    // elapses is aborted and named individually, so an overrun still says which task was responsible.
    let component_sup_id = get_component_relative_identifier(context.component_type(), context.component_id());
    let mut component_sup = Supervisor::new(component_sup_id.to_string())?
        .with_auto_shutdown(AutoShutdown::AnySignificant)
        .with_shutdown_budget(shutdown_timeout);

    let runnable = make_runnable();
    component_sup.add_worker(
        runtime::supervisable(ComponentWorker::new(context.clone(), runnable))
            .temporary()
            .with_significant(true)
            .build(),
    );

    Ok(component_sup)
}

/// Adds a heartbeat driver for each of the given admission points to a component's supervisor.
///
/// Each driver runs beside the component and stops when the component's supervisor shuts down, or earlier, once the
/// component's senders are gone. Drivers are neither restarted nor significant: one stopping never stops the component.
fn add_heartbeat_drivers(component_sup: &mut Supervisor, points: Vec<Arc<AdmissionPoint>>) {
    for point in points {
        component_sup.add_worker(
            runtime::worker_with_shutdown("admission_heartbeat", move |shutdown| async move {
                select! {
                    _ = shutdown => {},
                    _ = point.run_heartbeats() => {},
                }
            })
            .build(),
        );
    }
}

fn build_health_handle(
    health_registry: &HealthRegistry, component_context: &ComponentContext,
) -> Result<Health, GenericError> {
    // Register under the component's canonical identity, so the health registry name is byte-identical to the
    // resource-accounting and process-tree names for the same component. `TopologyReady` derives its match prefix
    // from the same `topology_root` that the identity is built from, so the two stay in sync.
    let maybe_handle = health_registry.register_component(&component_context.identity());

    match maybe_handle {
        Some(handle) => Ok(handle),
        None => Err(generic_error!(
            "duplicate {} component ID in health registry: {}",
            component_context.component_type().as_str(),
            component_context.component_id()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use futures::FutureExt as _;

    use super::*;
    use crate::data_model::event::{metric::Metric, Event, EventType};
    use crate::data_model::payload::PayloadType;
    use crate::execution::{
        AdmissionMode, AdmissionPointId, AdmissionRegistry, AdmissionSchedule, GenerationId, GenerationToken,
        HeartbeatOutcome,
    };
    use crate::topology::{graph::Graph, ComponentId};

    #[test]
    fn component_interconnects_adds_output_before_attaching() {
        let mut graph = Graph::default();
        let topology_id = SubsystemIdentifier::from_segments(["test"]);

        // Create a set of components and connect them together.
        graph
            .with_source("source1", EventType::EventD)
            .with_transform("transform1", EventType::EventD, EventType::EventD)
            .with_encoder("encoder1", EventType::EventD, PayloadType::Raw)
            .with_forwarder("forwarder1", PayloadType::Raw)
            .with_destination("dest1", EventType::EventD)
            .with_edge("source1", "transform1")
            .with_edge("transform1", "encoder1")
            .with_edge("encoder1", "forwarder1")
            .with_edge("transform1", "dest1");

        // Ensure we can properly build the interconnects for them, which requires adding the outputs
        // before attaching senders to them:
        let interconnect_capacity = NonZeroUsize::new(10).unwrap();
        let _ = ComponentInterconnects::from_graph(
            interconnect_capacity,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            &topology_id,
            &graph,
            &[],
        )
        .expect("should build interconnects successfully");
    }

    /// Builds the interconnects of `source -> {owner, tap}`, with an admission point on the source's output that
    /// registers in `registry` and sends heartbeats only to `owner`.
    fn interconnects_with_point(registry: &AdmissionRegistry) -> Result<ComponentInterconnects, GenericError> {
        let mut graph = Graph::default();
        graph
            .with_source("source", EventType::Metric)
            .with_destination("owner", EventType::Metric)
            .with_destination("tap", EventType::Metric)
            .with_edge("source", "owner")
            .with_edge("source", "tap");
        let schedule = AdmissionSchedule::Steady(Arc::new(GenerationToken::new(GenerationId::new(1)))).into_handle();
        let config = AdmissionPointConfig::new(
            AdmissionPointId::new(1),
            AdmissionMode::Admit,
            schedule,
            Duration::from_secs(600),
        )
        .with_registry(registry.clone())
        .with_heartbeat_recipients([ComponentId::try_from("owner").unwrap()]);
        let binding = AdmissionPointBinding {
            output: ComponentOutputId::try_from("source").unwrap(),
            component_type: ComponentType::Source,
            config,
        };
        ComponentInterconnects::from_graph(
            NonZeroUsize::new(4).unwrap(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            &SubsystemIdentifier::from_segments(["test"]),
            &graph,
            &[binding],
        )
    }

    fn destination_context(id: &str) -> ComponentContext {
        ComponentContext::new(
            &SubsystemIdentifier::from_segments(["test"]),
            ComponentId::try_from(id).unwrap(),
            ComponentType::Destination,
        )
    }

    #[tokio::test]
    async fn a_destination_with_an_input_gate_receives_only_while_it_is_open() {
        // `source -> {owner, tap}`, with a gate on the tap's input.
        let mut graph = Graph::default();
        graph
            .with_source("source", EventType::Metric)
            .with_destination("owner", EventType::Metric)
            .with_destination("tap", EventType::Metric)
            .with_edge("source", "owner")
            .with_edge("source", "tap");
        let gate = InputGate::closed();
        let mut interconnects = ComponentInterconnects::from_graph(
            NonZeroUsize::new(4).unwrap(),
            HashMap::new(),
            HashMap::from([(ComponentId::try_from("tap").unwrap(), gate.clone())]),
            HashMap::new(),
            &SubsystemIdentifier::from_segments(["test"]),
            &graph,
            &[],
        )
        .expect("should build interconnects");
        let mut owner = interconnects
            .take_events_consumer(&destination_context("owner"))
            .unwrap();
        let mut tap = interconnects.take_events_consumer(&destination_context("tap")).unwrap();
        let dispatcher = interconnects
            .take_events_dispatcher(&ComponentContext::source(
                &SubsystemIdentifier::from_segments(["test"]),
                ComponentId::try_from("source").unwrap(),
            ))
            .unwrap();

        dispatcher.dispatch(EventsBuffer::default()).await.unwrap();
        assert!(owner.next().now_or_never().flatten().is_some());
        assert!(tap.next().now_or_never().is_none(), "the closed tap is skipped");

        gate.open();
        dispatcher.dispatch(EventsBuffer::default()).await.unwrap();
        assert!(owner.next().now_or_never().flatten().is_some());
        assert!(tap.next().now_or_never().flatten().is_some(), "the open tap receives");
    }

    #[tokio::test]
    async fn interconnects_register_admission_points_that_send_heartbeats_only_to_their_recipients() {
        let registry = AdmissionRegistry::new();
        let mut interconnects = interconnects_with_point(&registry).expect("should build interconnects");
        let point = registry
            .get(AdmissionPointId::new(1))
            .expect("the point registers when its interconnects are built");
        let mut owner = interconnects
            .take_events_consumer(&destination_context("owner"))
            .unwrap();
        let mut tap = interconnects.take_events_consumer(&destination_context("tap")).unwrap();

        // The dispatcher, which the interconnects still hold, keeps the point's output open.
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        let heartbeat = owner
            .next()
            .now_or_never()
            .flatten()
            .expect("the owner receives the heartbeat");
        assert!(heartbeat.is_empty() && heartbeat.header().is_stamped());
        assert!(tap.next().now_or_never().is_none(), "the tap receives no heartbeat");

        // Data still reaches both components.
        let dispatcher = interconnects
            .take_events_dispatcher(&ComponentContext::source(
                &SubsystemIdentifier::from_segments(["test"]),
                ComponentId::try_from("source").unwrap(),
            ))
            .unwrap();
        dispatcher.dispatch(EventsBuffer::default()).await.unwrap();
        assert!(owner.next().now_or_never().flatten().is_some());
        assert!(tap.next().now_or_never().flatten().is_some());

        // A second topology whose point uses an identifier the registry already holds fails to build, while the first
        // point lives; once it is gone, the identifier is free.
        let error = match interconnects_with_point(&registry) {
            Ok(_) => panic!("a registered identifier is refused"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{:#}", error),
            "Failed to register the admission point of output 'source'.: Admission point 1 is already registered. Give \
             every admission point of one registry a unique ID."
        );
        drop((dispatcher, interconnects, point));
        assert!(registry.get(AdmissionPointId::new(1)).is_none());
        assert!(interconnects_with_point(&registry).is_ok());
    }

    /// Builds a component-less `BuiltTopology` carrying only the given worker-pool configuration, for exercising
    /// [`BuiltTopology::resolve_worker_pool_handle`] in isolation.
    fn empty_built_topology(worker_pool_config: WorkerPoolConfiguration) -> BuiltTopology {
        BuiltTopology::from_parts(
            "test".to_string(),
            SubsystemIdentifier::from_segments(["test"]),
            Graph::default(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
            ResourceGroupToken::root(),
            NonZeroUsize::new(128).unwrap(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            worker_pool_config,
        )
    }

    /// Spawns a task on `handle` and returns the name of the OS thread it ran on.
    async fn spawned_task_thread_name(handle: Handle) -> Option<String> {
        handle
            .spawn(async { std::thread::current().name().map(String::from) })
            .await
            .expect("spawned task should complete")
    }

    #[test]
    fn explicit_worker_pool_resolves_to_provided_runtime() {
        // `WorkerPoolConfiguration::Explicit` resolves to the exact handle it was given: a task spawned on the resolved
        // handle runs on the explicitly-provided runtime's (distinctly named) worker thread.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("explicit-pool")
            .enable_all()
            .build()
            .expect("should build explicit runtime");

        let built = empty_built_topology(WorkerPoolConfiguration::Explicit(rt.handle().clone()));
        let resolved = built
            .resolve_worker_pool_handle()
            .expect("explicit handle should resolve");

        let thread_name = rt.block_on(spawned_task_thread_name(resolved));
        assert_eq!(thread_name.as_deref(), Some("explicit-pool"));
    }

    #[test]
    fn ambient_worker_pool_resolves_to_current_runtime() {
        // `WorkerPoolConfiguration::Ambient` resolves to `Handle::current()`: a task spawned on the resolved handle
        // runs on the ambient runtime that was active when it was resolved.
        let ambient = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("ambient-pool")
            .enable_all()
            .build()
            .expect("should build ambient runtime");

        let thread_name = ambient.block_on(async {
            let built = empty_built_topology(WorkerPoolConfiguration::Ambient);
            let resolved = built
                .resolve_worker_pool_handle()
                .expect("ambient handle should resolve");
            spawned_task_thread_name(resolved).await
        });
        assert_eq!(thread_name.as_deref(), Some("ambient-pool"));
    }

    #[test]
    fn dedicated_worker_pool_resolves_to_new_runtime() {
        // `WorkerPoolConfiguration::Dedicated` resolves to a *new*, separate multi-threaded Tokio runtime: a task
        // spawned on the resolved handle runs on that runtime's own (default-named) worker threads, not the ambient
        // runtime it was resolved from.
        let ambient = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("ambient-pool")
            .enable_all()
            .build()
            .expect("should build ambient runtime");

        let thread_name = ambient.block_on(async {
            let built = empty_built_topology(WorkerPoolConfiguration::Dedicated {
                threads: DEFAULT_WORKER_POOL_THREADS,
            });
            let resolved = built
                .resolve_worker_pool_handle()
                .expect("dedicated handle should resolve");
            spawned_task_thread_name(resolved).await
        });

        let thread_name = thread_name.expect("dedicated pool worker threads are named");
        assert_ne!(
            thread_name, "ambient-pool",
            "dedicated pool must not reuse the ambient runtime"
        );
        assert!(
            thread_name.contains("worker"),
            "dedicated pool should run on its own Tokio worker thread, got {thread_name}"
        );
    }

    /// Returns a buffer of `events` metrics.
    fn buffer_of(events: usize) -> EventsBuffer {
        let mut buffer = EventsBuffer::with_capacity(events);
        for value in 0..events {
            assert!(buffer
                .try_push(Event::Metric(Metric::gauge("m", value as f64)))
                .is_none());
        }
        buffer
    }

    /// Returns the length of every buffer `consumer` holds now.
    fn queued_lengths(consumer: &mut EventsConsumer) -> Vec<usize> {
        let mut lengths = Vec::new();
        while let Some(Some(buffer)) = consumer.next().now_or_never() {
            lengths.push(buffer.len());
        }
        lengths
    }

    #[tokio::test]
    async fn interconnects_give_each_lane_of_a_transform_a_channel_of_the_transforms_input_capacity() {
        // spread -> laned (3 lanes, input capacity 2, spread), fixed -> laned (lane 1), laned -> out.
        let topology_id = SubsystemIdentifier::from_segments(["test"]);
        let mut graph = Graph::default();
        graph
            .with_source("spread", EventType::Metric)
            .with_source("fixed", EventType::Metric)
            .with_laned_transform("laned", 3)
            .with_destination("out", EventType::Metric)
            .with_lane_edge("spread", "laned", LaneEdge::Spread)
            .with_lane_edge("fixed", "laned", LaneEdge::Fixed(1))
            .with_edge("laned", "out");
        let laned_id = ComponentId::try_from("laned").unwrap();
        let mut interconnects = ComponentInterconnects::from_graph(
            NonZeroUsize::new(8).unwrap(),
            HashMap::from([(laned_id.clone(), NonZeroUsize::new(2).unwrap())]),
            HashMap::new(),
            HashMap::new(),
            &topology_id,
            &graph,
            &[],
        )
        .expect("should build interconnects");

        let laned = ComponentContext::transform(&topology_id, laned_id);
        let mut lanes = interconnects
            .take_events_lane_consumers(&laned)
            .expect("the transform has inputs");
        assert_eq!(lanes.len(), 3);
        let source = |id: &str| ComponentContext::source(&topology_id, ComponentId::try_from(id).unwrap());
        let spread = interconnects.take_events_dispatcher(&source("spread")).unwrap();
        let fixed = interconnects.take_events_dispatcher(&source("fixed")).unwrap();

        // Each lane holds two buffers: the spread source sends six, one lane after another, and then every lane is
        // full, so the seventh waits, and so does a buffer for lane 1.
        for events in 1..=6 {
            spread
                .dispatch(buffer_of(events))
                .now_or_never()
                .expect("a lane has room")
                .unwrap();
        }
        assert!(
            spread.dispatch(buffer_of(7)).now_or_never().is_none(),
            "every lane is full"
        );
        assert!(fixed.dispatch(buffer_of(8)).now_or_never().is_none(), "lane 1 is full");
        assert_eq!(
            lanes.iter_mut().map(queued_lengths).collect::<Vec<_>>(),
            vec![vec![1, 4], vec![2, 5], vec![3, 6]]
        );
        fixed.dispatch(buffer_of(8)).await.unwrap();
        assert_eq!(
            lanes.iter_mut().map(queued_lengths).collect::<Vec<_>>(),
            vec![vec![], vec![8], vec![]]
        );

        // A destination has one lane.
        let out = interconnects
            .take_events_consumer(&destination_context("out"))
            .expect("the destination has one input");
        drop(out);
    }

    #[test]
    fn a_worker_pool_sized_for_the_primary_runtime_has_the_default_threads_or_one_per_primary_worker() {
        for (workers, threads) in [(0, 8), (1, 8), (4, 8), (8, 8), (9, 9), (16, 16), (64, 64)] {
            assert_eq!(worker_pool_threads(workers).get(), threads, "{} workers", workers);
        }
    }

    #[tokio::test]
    async fn a_tap_fed_by_outputs_fixed_to_lanes_is_gated_on_every_one_of_them() {
        // Two outputs, each fixed to its lane of `laned` (as a source's lane outputs are), both feed a gated `tap`.
        let topology_id = SubsystemIdentifier::from_segments(["test"]);
        let mut graph = Graph::default();
        graph
            .with_source("lane_0", EventType::Metric)
            .with_source("lane_1", EventType::Metric)
            .with_laned_transform("laned", 2)
            .with_destination("tap", EventType::Metric)
            .with_lane_edge("lane_0", "laned", LaneEdge::Fixed(0))
            .with_lane_edge("lane_1", "laned", LaneEdge::Fixed(1))
            .with_edge("lane_0", "tap")
            .with_edge("lane_1", "tap");
        let gate = InputGate::closed();
        let mut interconnects = ComponentInterconnects::from_graph(
            NonZeroUsize::new(4).unwrap(),
            HashMap::new(),
            HashMap::from([(ComponentId::try_from("tap").unwrap(), gate.clone())]),
            HashMap::new(),
            &topology_id,
            &graph,
            &[],
        )
        .expect("should build interconnects");
        let laned = ComponentContext::transform(&topology_id, ComponentId::try_from("laned").unwrap());
        let mut lanes = interconnects.take_events_lane_consumers(&laned).unwrap();
        let mut tap = interconnects.take_events_consumer(&destination_context("tap")).unwrap();
        let source = |id: &str| ComponentContext::source(&topology_id, ComponentId::try_from(id).unwrap());
        let outputs = [
            interconnects.take_events_dispatcher(&source("lane_0")).unwrap(),
            interconnects.take_events_dispatcher(&source("lane_1")).unwrap(),
        ];

        for (events, output) in (1..).zip(&outputs) {
            output.dispatch(buffer_of(events)).await.unwrap();
        }
        assert_eq!(
            lanes.iter_mut().map(queued_lengths).collect::<Vec<_>>(),
            vec![vec![1], vec![2]]
        );
        assert!(
            queued_lengths(&mut tap).is_empty(),
            "the closed tap is skipped on both outputs"
        );

        gate.open();
        for (events, output) in (3..).zip(&outputs) {
            output.dispatch(buffer_of(events)).await.unwrap();
        }
        assert_eq!(
            lanes.iter_mut().map(queued_lengths).collect::<Vec<_>>(),
            vec![vec![3], vec![4]]
        );
        assert_eq!(
            queued_lengths(&mut tap),
            vec![3, 4],
            "the open tap receives from both outputs"
        );
    }
}

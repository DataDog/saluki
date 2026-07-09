use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_common::resource_tracking::ResourceGroupToken;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{runtime::Handle, sync::mpsc};
use tracing::debug;

use crate::accounting::{ComponentRegistry, MemoryLimiter};

/// Period over which the topology supervisor's restart intensity is measured.
///
/// The topology supervisor uses a restart intensity of 0, so any component failure shuts the topology
/// down on the first occurrence; the period only needs to be a sane non-zero value.
const TOPOLOGY_RESTART_PERIOD: Duration = Duration::from_secs(5);

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

    /// Create a dedicated, multi-threaded Tokio runtime with 8 worker threads.
    ///
    /// This is the default behavior.
    Dedicated,

    /// Use an externally provided runtime.
    ///
    /// Component subtasks are spawned on the runtime associated with the given handle.
    Explicit(Handle),
}

use super::component_worker::{
    ComponentWorker, DecoderRunnable, DestinationRunnable, EncoderRunnable, ForwarderRunnable, RelayRunnable,
    RunnableComponent, SourceRunnable, TransformRunnable,
};
use super::{graph::Graph, EventsBuffer, EventsConsumer, OutputName, PayloadsConsumer, TypedComponentId};
use crate::health::{Health, HealthRegistry};
use crate::runtime::state::DataspaceRegistry;
use crate::runtime::{
    AutoShutdown, ChildSpecification, RestartMode, RestartStrategy, RestartType, ShutdownMode, Supervisor,
    SupervisorHandle,
};
use crate::support::SubsystemIdentifier;
use crate::topology::ids::get_component_relative_identifier;
use crate::topology::interconnect::{Consumer, Dispatchable};
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
    component_token: ResourceGroupToken,
    interconnect_capacity: NonZeroUsize,
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
        component_token: ResourceGroupToken, interconnect_capacity: NonZeroUsize,
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
            component_token,
            interconnect_capacity,
            worker_pool_config,
        }
    }

    fn resolve_worker_pool_handle(&self) -> Result<Handle, GenericError> {
        match &self.worker_pool_config {
            WorkerPoolConfiguration::Ambient => Ok(Handle::current()),
            WorkerPoolConfiguration::Dedicated => {
                let thread_pool = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(8)
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
        let mut interconnects =
            ComponentInterconnects::from_graph(self.interconnect_capacity, &self.topology_id, &self.graph)
                .error_context("Failed to build component interconnects.")?;

        // The topology supervisor parents one dedicated supervisor per component. `Concurrent` shutdown
        // signals every component at once, so sources stop immediately and the downstream cascade drains in
        // parallel (bounded by `shutdown_timeout`) rather than one component at a time. A `OneForOne`
        // strategy with intensity 0 turns any single component failure into a supervisor shutdown on the
        // first occurrence, which fails the topology as a whole -- preserving the previous behavior where a
        // component finishing unexpectedly brings the topology down.
        let mut topology_sup = Supervisor::new(self.topology_id.to_string())?
            .with_shutdown_mode(ShutdownMode::Concurrent)
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, TOPOLOGY_RESTART_PERIOD));

        // Build our sources.
        for (component_context, (component, component_registry)) in self.sources {
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| SourceRunnable {
                    component,
                    context: SourceContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our relays.
        for (component_context, (component, component_registry)) in self.relays {
            let dispatcher = interconnects.take_payloads_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| RelayRunnable {
                    component,
                    context: RelayContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our decoders.
        for (component_context, (component, component_registry)) in self.decoders {
            let consumer = interconnects.take_payloads_consumer(&component_context)?;
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| DecoderRunnable {
                    component,
                    context: DecoderContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        consumer,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our transforms.
        for (component_context, (component, component_registry)) in self.transforms {
            let consumer = interconnects.take_events_consumer(&component_context)?;
            let dispatcher = interconnects.take_events_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| TransformRunnable {
                    component,
                    context: TransformContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        consumer,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our destinations.
        for (component_context, (component, component_registry)) in self.destinations {
            let consumer = interconnects.take_events_consumer(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| DestinationRunnable {
                    component,
                    context: DestinationContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        consumer,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our encoders.
        for (component_context, (component, component_registry)) in self.encoders {
            let consumer = interconnects.take_events_consumer(&component_context)?;
            let dispatcher = interconnects.take_payloads_dispatcher(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| EncoderRunnable {
                    component,
                    context: EncoderContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        dispatcher,
                        consumer,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        // Build our forwarders.
        for (component_context, (component, component_registry)) in self.forwarders {
            let consumer = interconnects.take_payloads_consumer(&component_context)?;
            let health_handle = build_health_handle(health_registry, &component_context)?;

            let component_sup =
                build_component_supervisor(&component_context, shutdown_timeout, |handle| ForwarderRunnable {
                    component,
                    context: ForwarderContext::new(
                        &topology_context,
                        &component_context,
                        component_registry,
                        health_handle,
                        consumer,
                        handle,
                    ),
                })?;
            topology_sup.add_worker(component_sup);
        }

        Ok(topology_sup)
    }
}

struct ComponentInterconnects {
    interconnect_capacity: NonZeroUsize,
    topology_id: SubsystemIdentifier,
    events_dispatchers: HashMap<ComponentContext, EventsDispatcher>,
    payloads_dispatchers: HashMap<ComponentContext, PayloadsDispatcher>,
    events_consumers: HashMap<ComponentContext, (mpsc::Sender<EventsBuffer>, EventsConsumer)>,
    payloads_consumers: HashMap<ComponentContext, (mpsc::Sender<PayloadsBuffer>, PayloadsConsumer)>,
}

impl ComponentInterconnects {
    fn from_graph(
        interconnect_capacity: NonZeroUsize, topology_id: &SubsystemIdentifier, graph: &Graph,
    ) -> Result<Self, GenericError> {
        let mut interconnects = Self {
            interconnect_capacity,
            topology_id: topology_id.clone(),
            events_dispatchers: HashMap::new(),
            payloads_dispatchers: HashMap::new(),
            events_consumers: HashMap::new(),
            payloads_consumers: HashMap::new(),
        };

        interconnects.generate_interconnects(graph)?;
        Ok(interconnects)
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

    fn take_events_consumer(&mut self, component_context: &ComponentContext) -> Result<EventsConsumer, GenericError> {
        self.events_consumers
            .remove(component_context)
            .map(|(_, consumer)| consumer)
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
                    self.generate_event_interconnect(upstream_id, output_map)?;
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
        &mut self, upstream_id: TypedComponentId, output_map: HashMap<OutputName, Vec<TypedComponentId>>,
    ) -> Result<(), GenericError> {
        for (upstream_output_id, downstream_ids) in output_map {
            let mut senders = Vec::new();
            for downstream_id in downstream_ids {
                debug!(upstream_id = %upstream_id.component_id(), %upstream_output_id, downstream_id = %downstream_id.component_id(), "Adding dispatcher output.");
                let sender = self.get_or_create_events_sender(downstream_id);
                senders.push(sender);
            }

            let dispatcher = self.get_or_create_events_dispatcher(upstream_id.clone());
            dispatcher.add_output(upstream_output_id.clone())?;

            for sender in senders {
                dispatcher.attach_sender_to_output(&upstream_output_id, sender)?;
            }
        }

        Ok(())
    }

    fn generate_payload_interconnect(
        &mut self, upstream_id: TypedComponentId, output_map: HashMap<OutputName, Vec<TypedComponentId>>,
    ) -> Result<(), GenericError> {
        for (upstream_output_id, downstream_ids) in output_map {
            let mut senders = Vec::new();
            for downstream_id in downstream_ids {
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

    fn get_or_create_events_sender(&mut self, component_id: TypedComponentId) -> mpsc::Sender<EventsBuffer> {
        let (component_id, component_type) = component_id.into_parts();
        let component_context = ComponentContext::new(&self.topology_id, component_id.clone(), component_type);
        let interconnect_capacity = self.interconnect_capacity;

        let (sender, _) = match component_type {
            ComponentType::Transform | ComponentType::Destination | ComponentType::Encoder => self
                .events_consumers
                .entry(component_context.clone())
                .or_insert_with(|| build_consumer_pair(component_context, interconnect_capacity)),
            _ => panic!("Only transforms, destinations, and encoders can consume events."),
        };

        sender.clone()
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
        let component_context = ComponentContext::new(&self.topology_id, component_id.clone(), component_type);
        let interconnect_capacity = self.interconnect_capacity;

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
    F: FnOnce(SupervisorHandle) -> C,
{
    let component_sup_id = get_component_relative_identifier(context.component_type(), context.component_id());
    let mut component_sup = Supervisor::new(component_sup_id.to_string())?
        .with_auto_shutdown(AutoShutdown::AnySignificant)
        .with_shutdown_mode(ShutdownMode::Concurrent);

    let runnable = make_runnable(component_sup.handle());
    component_sup.add_worker(
        ChildSpecification::worker(ComponentWorker::new(context.clone(), shutdown_timeout, runnable))
            .with_restart_type(RestartType::Temporary)
            .with_significant(true),
    );

    Ok(component_sup)
}

fn build_health_handle(
    health_registry: &HealthRegistry, component_context: &ComponentContext,
) -> Result<Health, GenericError> {
    // Register under the component's canonical identity, so the health registry name is byte-identical to the
    // resource-accounting and process-tree names for the same component. `TopologyReady` derives its match prefix
    // from the same `topology_root` that the identity is built from, so the two stay in sync.
    let maybe_handle = health_registry.register_component(component_context.identity().to_string());

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

    use super::*;
    use crate::data_model::event::EventType;
    use crate::data_model::payload::PayloadType;
    use crate::topology::graph::Graph;

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
        let _ = ComponentInterconnects::from_graph(interconnect_capacity, &topology_id, &graph)
            .expect("should build interconnects successfully");
    }
}

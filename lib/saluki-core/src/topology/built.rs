use std::collections::HashMap;

use memory_accounting::{
    allocator::{AllocationGroupToken, Tracked},
    MemoryLimiter,
};
use saluki_common::task::JoinSetExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::{
    sync::mpsc,
    task::{AbortHandle, JoinSet},
};
use tracing::{debug, error_span};

use super::{
    graph::Graph,
    interconnect::{Dispatcher, EventStream, FixedSizeEventBuffer, FixedSizeEventBufferInner},
    running::RunningTopology,
    shutdown::ComponentShutdownCoordinator,
    ComponentId, RegisteredComponent,
};
use crate::{
    components::{
        destinations::{Destination, DestinationContext},
        sources::{Source, SourceContext},
        transforms::{Transform, TransformContext},
        ComponentContext,
    },
    pooling::OnDemandObjectPool,
    topology::TopologyConfiguration,
};

/// A built topology.
///
/// Built topologies represent a topology blueprint where each configured component, along with their associated
/// connections to other components, was validated and built successfully.
///
/// A built topology must be spawned via [`spawn`][Self::spawn].
pub struct BuiltTopology<T> {
    name: String,
    config: T,
    graph: Graph,
    event_buffer_pool_buffer_size: usize,
    sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
    component_token: AllocationGroupToken,
}

impl<T: TopologyConfiguration> BuiltTopology<T> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        name: String, config: T, graph: Graph, event_buffer_pool_buffer_size: usize,
        sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
        transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
        destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
        component_token: AllocationGroupToken,
    ) -> Self {
        Self {
            name,
            config,
            graph,
            event_buffer_pool_buffer_size,
            sources,
            transforms,
            destinations,
            component_token,
        }
    }

    fn create_component_interconnects(
        &self, event_buffer_pool: OnDemandObjectPool<FixedSizeEventBuffer>,
    ) -> (HashMap<ComponentId, Dispatcher>, HashMap<ComponentId, EventStream>) {
        // Collect all of the outbound edges in our topology graph.
        //
        // This gives us a mapping of components which send events to another component, grouped by output name.
        let outbound_edges = self.graph.get_outbound_directed_edges();

        let mut dispatchers = HashMap::new();
        let mut event_streams = HashMap::new();
        let mut event_stream_senders: HashMap<ComponentId, mpsc::Sender<FixedSizeEventBuffer>> = HashMap::new();

        for (upstream_id, output_map) in outbound_edges {
            // Get a reference to the dispatcher for the current upstream component
            let dispatcher = dispatchers.entry(upstream_id.clone()).or_insert_with(|| {
                // TODO: This is wrong, because an upstream component is simply any component that can dispatch, which is
                // either a source or transform.
                let component_context = ComponentContext::source(upstream_id.clone());
                Dispatcher::new(component_context, event_buffer_pool.clone())
            });

            for (upstream_output_id, downstream_ids) in output_map {
                // For each downstream component mapped to this upstream component's output, we need to grab a copy of
                // the sender we'll use to actually send to them... so we either clone it here or we do the initial
                // creation.
                for downstream_id in downstream_ids {
                    let sender = match event_stream_senders.get(&downstream_id) {
                        Some(sender) => sender.clone(),
                        None => {
                            let (sender, receiver) = build_interconnect_channel(&self.config);

                            // TODO: Similarly broken here, since a downstream component is any component that can
                            // receive events, which is either a transform or destination.
                            let component_context = ComponentContext::destination(downstream_id.clone());
                            let event_stream = EventStream::new(component_context, receiver);

                            event_streams.insert(downstream_id.clone(), event_stream);
                            event_stream_senders.insert(downstream_id.clone(), sender.clone());
                            sender
                        }
                    };

                    debug!(%upstream_id, %upstream_output_id, %downstream_id, "Adding dispatcher output.");
                    dispatcher.add_output(upstream_output_id.clone(), sender);
                }
            }
        }

        (dispatchers, event_streams)
    }

    /// Spawns the topology.
    ///
    /// A handle is returned that can be used to trigger the topology to shutdown.
    ///
    /// ## Errors
    ///
    /// If an error occurs while spawning the topology, an error is returned.
    pub async fn spawn(
        self, health_registry: &HealthRegistry, memory_limiter: MemoryLimiter,
    ) -> Result<RunningTopology, GenericError> {
        let root_component_name = format!("topology.{}", self.name);

        let _guard = self.component_token.enter();

        let thread_pool = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .enable_all()
            .build()
            .error_context("Failed to build asynchronous thread pool runtime.")?;
        let thread_pool_handle = thread_pool.handle().clone();

        let mut component_tasks = JoinSet::new();
        let mut component_task_map = HashMap::new();

        // Build our interconnects, which we'll grab from piecemeal as we spawn our components.
        let event_buffer_pool = OnDemandObjectPool::with_builder("global_event_buffers", move || {
            FixedSizeEventBufferInner::with_capacity(self.event_buffer_pool_buffer_size)
        });

        let (mut forwarders, mut event_streams) = self.create_component_interconnects(event_buffer_pool.clone());

        let mut shutdown_coordinator = ComponentShutdownCoordinator::default();

        // Spawn our sources.
        for (component_id, source) in self.sources {
            let (source, component_registry) = source.into_parts();

            let forwarder = forwarders
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No forwarder found for component '{}'", component_id))?;

            let shutdown_handle = shutdown_coordinator.register();
            let health_handle = health_registry
                .register_component(format!("{}.sources.{}", root_component_name, component_id))
                .expect("duplicate source component ID in health registry");

            let component_context = ComponentContext::source(component_id.clone());
            let context = SourceContext::new(
                component_context,
                shutdown_handle,
                forwarder,
                event_buffer_pool.clone(),
                memory_limiter.clone(),
                component_registry,
                health_handle,
                health_registry.clone(),
                thread_pool_handle.clone(),
            );

            let task_handle = spawn_source(&mut component_tasks, source, context);
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our transforms.
        for (component_id, transform) in self.transforms {
            let (transform, component_registry) = transform.into_parts();

            let forwarder = forwarders
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No forwarder found for component '{}'", component_id))?;

            let event_stream = event_streams
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No event stream found for component '{}'", component_id))?;

            let health_handle = health_registry
                .register_component(format!("{}.transforms.{}", root_component_name, component_id))
                .expect("duplicate transform component ID in health registry");

            let component_context = ComponentContext::transform(component_id.clone());
            let context = TransformContext::new(
                component_context,
                forwarder,
                event_stream,
                event_buffer_pool.clone(),
                memory_limiter.clone(),
                component_registry,
                health_handle,
                health_registry.clone(),
                thread_pool_handle.clone(),
            );

            let task_handle = spawn_transform(&mut component_tasks, transform, context);
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our destinations.
        for (component_id, destination) in self.destinations {
            let (destination, component_registry) = destination.into_parts();

            let event_stream = event_streams
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No event stream found for component '{}'", component_id))?;

            let health_handle = health_registry
                .register_component(format!("{}.destinations.{}", root_component_name, component_id))
                .expect("duplicate destination component ID in health registry");

            let component_context = ComponentContext::destination(component_id.clone());
            let context = DestinationContext::new(
                component_context,
                event_stream,
                memory_limiter.clone(),
                component_registry,
                health_handle,
                health_registry.clone(),
                thread_pool_handle.clone(),
            );

            let task_handle = spawn_destination(&mut component_tasks, destination, context);
            component_task_map.insert(task_handle.id(), component_id);
        }

        Ok(RunningTopology::from_parts(
            thread_pool,
            shutdown_coordinator,
            component_tasks,
            component_task_map,
        ))
    }
}

fn spawn_source(
    join_set: &mut JoinSet<Result<(), GenericError>>, source: Tracked<Box<dyn Source + Send>>, context: SourceContext,
) -> AbortHandle {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type().as_str(),
        id = %context.component_context().component_id(),
    );

    let (component_token, source) = source.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    let component_task_name = format!("topology-source-{}", context.component_context().component_id());
    join_set.spawn_traced_named(component_task_name, async move { source.run(context).await })
}

fn spawn_transform(
    join_set: &mut JoinSet<Result<(), GenericError>>, transform: Tracked<Box<dyn Transform + Send>>,
    context: TransformContext,
) -> AbortHandle {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type().as_str(),
        id = %context.component_context().component_id(),
    );

    let (component_token, transform) = transform.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    let component_task_name = format!("topology-transform-{}", context.component_context().component_id());
    join_set.spawn_traced_named(component_task_name, async move { transform.run(context).await })
}

fn spawn_destination(
    join_set: &mut JoinSet<Result<(), GenericError>>, destination: Tracked<Box<dyn Destination + Send>>,
    context: DestinationContext,
) -> AbortHandle {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type().as_str(),
        id = %context.component_context().component_id(),
    );

    let (component_token, destination) = destination.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    let component_task_name = format!("topology-destination-{}", context.component_context().component_id());
    join_set.spawn_traced_named(component_task_name, async move { destination.run(context).await })
}

fn build_interconnect_channel<T: TopologyConfiguration>(
    config: &T,
) -> (mpsc::Sender<FixedSizeEventBuffer>, mpsc::Receiver<FixedSizeEventBuffer>) {
    mpsc::channel(config.interconnect_capacity().get())
}

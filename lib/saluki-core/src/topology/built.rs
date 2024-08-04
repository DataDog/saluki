use std::collections::HashMap;

use memory_accounting::{
    allocator::{Tracked, TrackingToken},
    MemoryLimiter,
};
use saluki_error::{generic_error, GenericError};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, error_span};

use super::{
    graph::Graph,
    interconnect::{EventBuffer, EventStream, Forwarder},
    running::RunningTopology,
    shutdown::ComponentShutdownCoordinator,
    ComponentId,
};
use crate::{
    components::{
        destinations::{Destination, DestinationContext},
        sources::{Source, SourceContext},
        transforms::{Transform, TransformContext},
        ComponentContext,
    },
    pooling::FixedSizeObjectPool,
    spawn_traced,
};

/// A built topology.
///
/// Built topologies represent a topology blueprint where each configured component, along with their associated
/// connections to other components, was validated and built successfully.
///
/// A built topology must be spawned via [`spawn`][Self::spawn].
pub struct BuiltTopology {
    graph: Graph,
    sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
    component_token: TrackingToken,
}

impl BuiltTopology {
    pub(crate) fn from_parts(
        graph: Graph, sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
        transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
        destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
        component_token: TrackingToken,
    ) -> Self {
        Self {
            graph,
            sources,
            transforms,
            destinations,
            component_token,
        }
    }

    fn create_component_interconnects(
        &self, event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
    ) -> (HashMap<ComponentId, Forwarder>, HashMap<ComponentId, EventStream>) {
        // Collect all of the outbound edges in our topology graph.
        //
        // This gives us a mapping of components which send events to another component, grouped by output name.
        let outbound_edges = self.graph.get_outbound_directed_edges();

        let mut forwarders = HashMap::new();
        let mut event_streams = HashMap::new();
        let mut event_stream_senders: HashMap<ComponentId, mpsc::Sender<EventBuffer>> = HashMap::new();

        for (upstream_id, output_map) in outbound_edges {
            // Get a reference to the forwarder for the current upstream component
            let forwarder: &mut Forwarder = forwarders.entry(upstream_id.clone()).or_insert_with(|| {
                // TODO: This is wrong, because an upstream component is simply any component that can forward, which is
                // either a source or transform.
                let component_context = ComponentContext::source(upstream_id.clone());
                Forwarder::new(component_context, event_buffer_pool.clone())
            });

            for (upstream_output_id, downstream_ids) in output_map {
                // For each downstream component mapped to this upstream component's output, we need to grab a copy of
                // the sender we'll use to actually send to them... so we either clone it here or we do the initial
                // creation.
                for downstream_id in downstream_ids {
                    let sender = match event_stream_senders.get(&downstream_id) {
                        Some(sender) => sender.clone(),
                        None => {
                            let (sender, receiver) = build_interconnect_channel();

                            // TODO: Similarly broken here, since a downstream component is any component that can
                            // receive events, which is either a transform or destination.
                            let component_context = ComponentContext::destination(downstream_id.clone());
                            let event_stream = EventStream::new(component_context, receiver);

                            event_streams.insert(downstream_id.clone(), event_stream);
                            event_stream_senders.insert(downstream_id.clone(), sender.clone());
                            sender
                        }
                    };

                    debug!(%upstream_id, %upstream_output_id, %downstream_id, "Adding forwarder output.");
                    forwarder.add_output(upstream_output_id.clone(), sender);
                }
            }
        }

        (forwarders, event_streams)
    }

    /// Spawns the topology.
    ///
    /// A handle is returned that can be used to trigger the topology to shutdown.
    ///
    /// ## Errors
    ///
    /// If an error occurs while spawning the topology, an error is returned.
    pub async fn spawn(self, memory_limiter: MemoryLimiter) -> Result<RunningTopology, GenericError> {
        let _guard = self.component_token.enter();

        // Build our interconnects, which we'll grab from piecemeal as we spawn our components.
        let event_buffer_pool = FixedSizeObjectPool::with_capacity(1024);
        let (mut forwarders, mut event_streams) = self.create_component_interconnects(event_buffer_pool.clone());

        let mut shutdown_coordinator = ComponentShutdownCoordinator::default();

        // Spawn our sources.
        let mut source_handles = Vec::new();

        for (component_id, source) in self.sources {
            let (source, component_registry) = source.into_parts();

            let forwarder = forwarders
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No forwarder found for component '{}'", component_id))?;

            let shutdown_handle = shutdown_coordinator.register();

            let component_context = ComponentContext::source(component_id);
            let context = SourceContext::new(
                component_context,
                shutdown_handle,
                forwarder,
                event_buffer_pool.clone(),
                memory_limiter.clone(),
                component_registry,
            );

            source_handles.push(spawn_source(source, context));
        }

        // Spawn our transforms.
        let mut transform_handles = Vec::new();

        for (component_id, transform) in self.transforms {
            let (transform, component_registry) = transform.into_parts();

            let forwarder = forwarders
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No forwarder found for component '{}'", component_id))?;

            let event_stream = event_streams
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No event stream found for component '{}'", component_id))?;

            let component_context = ComponentContext::transform(component_id);
            let context = TransformContext::new(
                component_context,
                forwarder,
                event_stream,
                event_buffer_pool.clone(),
                memory_limiter.clone(),
                component_registry,
            );

            transform_handles.push(spawn_transform(transform, context));
        }

        // Spawn our destinations.
        let mut destination_handles = Vec::new();

        for (component_id, destination) in self.destinations {
            let (destination, component_registry) = destination.into_parts();

            let event_stream = event_streams
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No event stream found for component '{}'", component_id))?;

            let component_context = ComponentContext::destination(component_id);
            let context = DestinationContext::new(
                component_context,
                event_stream,
                memory_limiter.clone(),
                component_registry,
            );

            destination_handles.push(spawn_destination(destination, context));
        }

        Ok(RunningTopology::from_parts(
            shutdown_coordinator,
            source_handles,
            transform_handles,
            destination_handles,
        ))
    }
}

fn spawn_source(source: Tracked<Box<dyn Source + Send>>, context: SourceContext) -> JoinHandle<Result<(), ()>> {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type(),
        id = %context.component_context().component_id(),
    );

    let (component_token, source) = source.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    spawn_traced(async move { source.run(context).await })
}

fn spawn_transform(
    transform: Tracked<Box<dyn Transform + Send>>, context: TransformContext,
) -> JoinHandle<Result<(), ()>> {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type(),
        id = %context.component_context().component_id(),
    );

    let (component_token, transform) = transform.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    spawn_traced(async move { transform.run(context).await })
}

fn spawn_destination(
    destination: Tracked<Box<dyn Destination + Send>>, context: DestinationContext,
) -> JoinHandle<Result<(), ()>> {
    let component_span = error_span!(
        "component",
        "type" = context.component_context().component_type(),
        id = %context.component_context().component_id(),
    );

    let (component_token, destination) = destination.into_parts();

    let _span = component_span.enter();
    let _guard = component_token.enter();

    spawn_traced(async move { destination.run(context).await })
}

fn build_interconnect_channel() -> (mpsc::Sender<EventBuffer>, mpsc::Receiver<EventBuffer>) {
    mpsc::channel(128)
}

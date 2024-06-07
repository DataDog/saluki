use std::collections::HashMap;

use memory_accounting::limiter::MemoryLimiter;
use saluki_error::{generic_error, GenericError};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    components::{
        destinations::{Destination, DestinationContext},
        sources::{Source, SourceContext},
        transforms::{Transform, TransformContext},
        ComponentContext,
    },
    pooling::FixedSizeObjectPool,
};

use super::{
    graph::Graph,
    interconnect::{EventBuffer, EventStream, Forwarder},
    running::RunningTopology,
    shutdown::ComponentShutdownCoordinator,
    ComponentId,
};

/// A built topology.
///
/// Built topologies represent a topology blueprint where each configured component, along with their associated
/// connections to other components, was validated and built successfully.
///
/// A built topology must be spawned via [`spawn`].
pub struct BuiltTopology {
    graph: Graph,
    sources: HashMap<ComponentId, Box<dyn Source + Send>>,
    transforms: HashMap<ComponentId, Box<dyn Transform + Send>>,
    destinations: HashMap<ComponentId, Box<dyn Destination + Send>>,
}

impl BuiltTopology {
    pub(crate) fn from_parts(
        graph: Graph, sources: HashMap<ComponentId, Box<dyn Source + Send>>,
        transforms: HashMap<ComponentId, Box<dyn Transform + Send>>,
        destinations: HashMap<ComponentId, Box<dyn Destination + Send>>,
    ) -> Self {
        Self {
            graph,
            sources,
            transforms,
            destinations,
        }
    }

    fn create_component_interconnects(&self) -> (HashMap<ComponentId, Forwarder>, HashMap<ComponentId, EventStream>) {
        let outbound_edges = self.graph.get_outbound_directed_edges();

        let mut forwarders = HashMap::new();
        let mut event_streams = HashMap::new();
        let mut event_stream_senders: HashMap<ComponentId, mpsc::Sender<EventBuffer>> = HashMap::new();

        for (source_cid, output_map) in outbound_edges {
            let forwarder: &mut Forwarder = forwarders.entry(source_cid.clone()).or_insert_with(|| {
                let component_context = ComponentContext::source(source_cid);
                Forwarder::new(component_context)
            });

            for (output_id, destination_cids) in output_map {
                for destination_cid in destination_cids {
                    let sender = match event_stream_senders.get(&destination_cid) {
                        Some(sender) => sender.clone(),
                        None => {
                            let (sender, receiver) = build_interconnect_channel();

                            let component_context = ComponentContext::destination(destination_cid.clone());
                            let event_stream = EventStream::new(component_context, receiver);

                            event_streams.insert(destination_cid.clone(), event_stream);
                            event_stream_senders.insert(destination_cid, sender.clone());
                            sender
                        }
                    };

                    forwarder.add_output(output_id.clone(), sender);
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
    pub async fn spawn(self, memory_limiter: MemoryLimiter) -> Result<RunningTopology, TopologyError> {
        // Build our interconnects, which we'll grab from piecemeal as we spawn our components.
        let (mut forwarders, mut event_streams) = self.create_component_interconnects();
        let event_buffer_pool = FixedSizeObjectPool::with_capacity(1024);

        let mut shutdown_coordinator = ComponentShutdownCoordinator::default();

        // Spawn our sources.
        let mut source_handles = Vec::new();

        for (component_id, source) in self.sources {
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
            );

            source_handles.push(spawn_source(source, context));
        }

        // Spawn our transforms.
        let mut transform_handles = Vec::new();

        for (component_id, transform) in self.transforms {
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
            );

            transform_handles.push(spawn_transform(transform, context));
        }

        // Spawn our destinations.
        let mut destination_handles = Vec::new();

        for (component_id, destination) in self.destinations {
            let event_stream = event_streams
                .remove(&component_id)
                .ok_or_else(|| generic_error!("No event stream found for component '{}'", component_id))?;

            let component_context = ComponentContext::destination(component_id);
            let context = DestinationContext::new(component_context, event_stream, memory_limiter.clone());

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

fn spawn_source(source: Box<dyn Source + Send>, context: SourceContext) -> JoinHandle<Result<(), ()>> {
    tokio::spawn(async move { source.run(context).await })
}

fn spawn_transform(transform: Box<dyn Transform + Send>, context: TransformContext) -> JoinHandle<Result<(), ()>> {
    tokio::spawn(async move { transform.run(context).await })
}

fn spawn_destination(
    destination: Box<dyn Destination + Send>, context: DestinationContext,
) -> JoinHandle<Result<(), ()>> {
    tokio::spawn(async move { destination.run(context).await })
}

fn build_interconnect_channel() -> (mpsc::Sender<EventBuffer>, mpsc::Receiver<EventBuffer>) {
    mpsc::channel(128)
}

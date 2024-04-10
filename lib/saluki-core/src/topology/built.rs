use std::collections::HashMap;

use snafu::{OptionExt as _, Snafu};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    buffers::FixedSizeBufferPool,
    components::{
        ComponentContext, Destination, DestinationContext, Source, SourceContext, Transform, TransformContext,
    },
};

use super::{
    graph::Graph,
    interconnect::{EventBuffer, EventStream, Forwarder},
    running::RunningTopology,
    shutdown::ComponentShutdownCoordinator,
    ComponentId,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum TopologyError {
    #[snafu(display("missing forwarder for '{}'", component_id))]
    MissingForwarder { component_id: ComponentId },
    #[snafu(display("missing event stream for '{}'", component_id))]
    MissingEventStream { component_id: ComponentId },
}

pub struct BuiltTopology {
    graph: Graph,
    sources: HashMap<ComponentId, Box<dyn Source + Send>>,
    transforms: HashMap<ComponentId, Box<dyn Transform + Send>>,
    destinations: HashMap<ComponentId, Box<dyn Destination + Send>>,
}

impl BuiltTopology {
    pub fn from_parts(
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

    pub async fn spawn(self) -> Result<RunningTopology, TopologyError> {
        // Build our interconnects, which we'll grab from piecemeal as we spawn our components.
        let (mut forwarders, mut event_streams) = self.create_component_interconnects();
        let event_buffer_pool = FixedSizeBufferPool::with_capacity(1024);

        let mut shutdown_coordinator = ComponentShutdownCoordinator::default();

        // Spawn our sources.
        let mut source_handles = Vec::new();

        for (component_id, source) in self.sources {
            let forwarder = forwarders
                .remove(&component_id)
                .ok_or(TopologyError::MissingForwarder {
                    component_id: component_id.clone(),
                })?;

            let shutdown_handle = shutdown_coordinator.register();

            let component_context = ComponentContext::source(component_id);
            let context = SourceContext::new(component_context, shutdown_handle, forwarder, event_buffer_pool.clone());

            source_handles.push(spawn_source(source, context));
        }

        // Spawn our transforms.
        let mut transform_handles = Vec::new();

        for (component_id, transform) in self.transforms {
            let forwarder = forwarders.remove(&component_id).context(MissingForwarder {
                component_id: component_id.clone(),
            })?;

            let event_stream = event_streams.remove(&component_id).context(MissingEventStream {
                component_id: component_id.clone(),
            })?;

            let component_context = ComponentContext::transform(component_id);
            let context = TransformContext::new(component_context, forwarder, event_stream, event_buffer_pool.clone());

            transform_handles.push(spawn_transform(transform, context));
        }

        // Spawn our destinations.
        let mut destination_handles = Vec::new();

        for (component_id, destination) in self.destinations {
            let event_stream = event_streams.remove(&component_id).context(MissingEventStream {
                component_id: component_id.clone(),
            })?;

            let component_context = ComponentContext::destination(component_id);
            let context = DestinationContext::new(component_context, event_stream);

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

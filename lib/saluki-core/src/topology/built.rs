use std::{collections::HashMap, future::Future, num::NonZeroUsize};

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
    graph::Graph, running::RunningTopology, shutdown::ComponentShutdownCoordinator, ComponentId, EventsBuffer,
    EventsConsumer, OutputName, PayloadsConsumer, RegisteredComponent, TypedComponentId,
};
use crate::{
    components::{
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
/// A built topology must be spawned via [`spawn`][Self::spawn].
pub struct BuiltTopology {
    name: String,
    graph: Graph,
    sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
    relays: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Relay + Send>>>>,
    transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
    destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
    encoders: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Encoder + Send>>>>,
    forwarders: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Forwarder + Send>>>>,
    component_token: AllocationGroupToken,
    interconnect_capacity: NonZeroUsize,
}

impl BuiltTopology {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        name: String, graph: Graph,
        sources: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Source + Send>>>>,
        relays: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Relay + Send>>>>,
        transforms: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Transform + Send>>>>,
        destinations: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Destination + Send>>>>,
        encoders: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Encoder + Send>>>>,
        forwarders: HashMap<ComponentId, RegisteredComponent<Tracked<Box<dyn Forwarder + Send>>>>,
        component_token: AllocationGroupToken, interconnect_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            name,
            graph,
            sources,
            relays,
            transforms,
            destinations,
            encoders,
            forwarders,
            component_token,
            interconnect_capacity,
        }
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

        std::thread::spawn(move || {
            thread_pool.block_on(std::future::pending::<()>());
        });

        let topology_context = TopologyContext::new(memory_limiter, health_registry.clone(), thread_pool_handle);

        let mut component_tasks = JoinSet::new();
        let mut component_task_map = HashMap::new();

        // Build our interconnects, which we'll grab from piecemeal as we spawn our components.
        let mut interconnects = ComponentInterconnects::from_graph(self.interconnect_capacity, &self.graph)
            .error_context("Failed to build component interconnects.")?;

        let mut shutdown_coordinator = ComponentShutdownCoordinator::default();

        // Spawn our sources.
        for (component_id, source) in self.sources {
            let (source, component_registry) = source.into_parts();

            let dispatcher = interconnects
                .take_source_dispatcher(&component_id)
                .ok_or_else(|| generic_error!("No events dispatcher found for source component '{}'", component_id))?;

            let shutdown_handle = shutdown_coordinator.register();
            let health_handle = health_registry
                .register_component(format!("{}.sources.{}", root_component_name, component_id))
                .expect("duplicate source component ID in health registry");

            let component_context = ComponentContext::source(component_id.clone());
            let context = SourceContext::new(
                &topology_context,
                &component_context,
                component_registry,
                shutdown_handle,
                health_handle,
                dispatcher,
            );

            let (alloc_group, source) = source.into_parts();
            let task_handle = spawn_component(
                &mut component_tasks,
                component_context,
                alloc_group,
                source.run(context),
            );
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our relays.
        for (component_id, relay) in self.relays {
            let (relay, component_registry) = relay.into_parts();

            let dispatcher = interconnects
                .take_relay_dispatcher(&component_id)
                .ok_or_else(|| generic_error!("No payloads dispatcher found for relay component '{}'", component_id))?;

            let shutdown_handle = shutdown_coordinator.register();
            let health_handle = health_registry
                .register_component(format!("{}.relays.{}", root_component_name, component_id))
                .expect("duplicate relay component ID in health registry");

            let component_context = ComponentContext::relay(component_id.clone());
            let context = RelayContext::new(
                &topology_context,
                &component_context,
                component_registry,
                shutdown_handle,
                health_handle,
                dispatcher,
            );

            let (alloc_group, relay) = relay.into_parts();
            let task_handle = spawn_component(&mut component_tasks, component_context, alloc_group, relay.run(context));
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our transforms.
        for (component_id, transform) in self.transforms {
            let (transform, component_registry) = transform.into_parts();

            let dispatcher = interconnects.take_transform_dispatcher(&component_id).ok_or_else(|| {
                generic_error!("No events dispatcher found for transform component '{}'", component_id)
            })?;

            let consumer = interconnects
                .take_transform_consumer(&component_id)
                .ok_or_else(|| generic_error!("No events consumer found for transform component '{}'", component_id))?;

            let health_handle = health_registry
                .register_component(format!("{}.transforms.{}", root_component_name, component_id))
                .expect("duplicate transform component ID in health registry");

            let component_context = ComponentContext::transform(component_id.clone());
            let context = TransformContext::new(
                &topology_context,
                &component_context,
                component_registry,
                health_handle,
                dispatcher,
                consumer,
            );

            let (alloc_group, transform) = transform.into_parts();
            let task_handle = spawn_component(
                &mut component_tasks,
                component_context,
                alloc_group,
                transform.run(context),
            );
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our destinations.
        for (component_id, destination) in self.destinations {
            let (destination, component_registry) = destination.into_parts();

            let consumer = interconnects.take_destination_consumer(&component_id).ok_or_else(|| {
                generic_error!("No events consumer found for destination component '{}'", component_id)
            })?;

            let health_handle = health_registry
                .register_component(format!("{}.destinations.{}", root_component_name, component_id))
                .expect("duplicate destination component ID in health registry");

            let component_context = ComponentContext::destination(component_id.clone());
            let context = DestinationContext::new(
                &topology_context,
                &component_context,
                component_registry,
                health_handle,
                consumer,
            );

            let (alloc_group, destination) = destination.into_parts();
            let task_handle = spawn_component(
                &mut component_tasks,
                component_context,
                alloc_group,
                destination.run(context),
            );
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our encoders.
        for (component_id, encoder) in self.encoders {
            let (encoder, component_registry) = encoder.into_parts();

            let dispatcher = interconnects.take_encoder_dispatcher(&component_id).ok_or_else(|| {
                generic_error!("No payloads dispatcher found for encoder component '{}'", component_id)
            })?;

            let consumer = interconnects
                .take_encoder_consumer(&component_id)
                .ok_or_else(|| generic_error!("No events consumer found for encoder component '{}'", component_id))?;

            let health_handle = health_registry
                .register_component(format!("{}.encoders.{}", root_component_name, component_id))
                .expect("duplicate encoder component ID in health registry");

            let component_context = ComponentContext::encoder(component_id.clone());
            let context = EncoderContext::new(
                &topology_context,
                &component_context,
                component_registry,
                health_handle,
                dispatcher,
                consumer,
            );

            let (alloc_group, encoder) = encoder.into_parts();
            let task_handle = spawn_component(
                &mut component_tasks,
                component_context,
                alloc_group,
                encoder.run(context),
            );
            component_task_map.insert(task_handle.id(), component_id);
        }

        // Spawn our forwarders.
        for (component_id, forwarder) in self.forwarders {
            let (forwarder, component_registry) = forwarder.into_parts();

            let consumer = interconnects.take_forwarder_consumer(&component_id).ok_or_else(|| {
                generic_error!("No payloads consumer found for forwarder component '{}'", component_id)
            })?;

            let health_handle = health_registry
                .register_component(format!("{}.forwarders.{}", root_component_name, component_id))
                .expect("duplicate forwarder component ID in health registry");

            let component_context = ComponentContext::forwarder(component_id.clone());
            let context = ForwarderContext::new(
                &topology_context,
                &component_context,
                component_registry,
                health_handle,
                consumer,
            );

            let (alloc_group, forwarder) = forwarder.into_parts();
            let task_handle = spawn_component(
                &mut component_tasks,
                component_context,
                alloc_group,
                forwarder.run(context),
            );
            component_task_map.insert(task_handle.id(), component_id);
        }

        Ok(RunningTopology::from_parts(
            shutdown_coordinator,
            component_tasks,
            component_task_map,
        ))
    }
}

struct ComponentInterconnects {
    interconnect_capacity: NonZeroUsize,
    source_dispatchers: HashMap<ComponentId, EventsDispatcher>,
    relay_dispatchers: HashMap<ComponentId, PayloadsDispatcher>,
    transform_consumers: HashMap<ComponentId, (mpsc::Sender<EventsBuffer>, EventsConsumer)>,
    transform_dispatchers: HashMap<ComponentId, EventsDispatcher>,
    destination_consumers: HashMap<ComponentId, (mpsc::Sender<EventsBuffer>, EventsConsumer)>,
    encoder_consumers: HashMap<ComponentId, (mpsc::Sender<EventsBuffer>, EventsConsumer)>,
    encoder_dispatchers: HashMap<ComponentId, PayloadsDispatcher>,
    forwarder_consumers: HashMap<ComponentId, (mpsc::Sender<PayloadsBuffer>, PayloadsConsumer)>,
}

impl ComponentInterconnects {
    fn from_graph(interconnect_capacity: NonZeroUsize, graph: &Graph) -> Result<Self, GenericError> {
        let mut interconnects = Self {
            interconnect_capacity,
            source_dispatchers: HashMap::new(),
            relay_dispatchers: HashMap::new(),
            transform_consumers: HashMap::new(),
            transform_dispatchers: HashMap::new(),
            destination_consumers: HashMap::new(),
            encoder_consumers: HashMap::new(),
            encoder_dispatchers: HashMap::new(),
            forwarder_consumers: HashMap::new(),
        };

        interconnects.generate_interconnects(graph)?;
        Ok(interconnects)
    }

    fn take_source_dispatcher(&mut self, component_id: &ComponentId) -> Option<EventsDispatcher> {
        self.source_dispatchers.remove(component_id)
    }

    fn take_relay_dispatcher(&mut self, component_id: &ComponentId) -> Option<PayloadsDispatcher> {
        self.relay_dispatchers.remove(component_id)
    }

    fn take_transform_dispatcher(&mut self, component_id: &ComponentId) -> Option<EventsDispatcher> {
        self.transform_dispatchers.remove(component_id)
    }

    fn take_encoder_dispatcher(&mut self, component_id: &ComponentId) -> Option<PayloadsDispatcher> {
        self.encoder_dispatchers.remove(component_id)
    }

    fn take_transform_consumer(&mut self, component_id: &ComponentId) -> Option<EventsConsumer> {
        self.transform_consumers
            .remove(component_id)
            .map(|(_, consumer)| consumer)
    }

    fn take_destination_consumer(&mut self, component_id: &ComponentId) -> Option<EventsConsumer> {
        self.destination_consumers
            .remove(component_id)
            .map(|(_, consumer)| consumer)
    }

    fn take_encoder_consumer(&mut self, component_id: &ComponentId) -> Option<EventsConsumer> {
        self.encoder_consumers
            .remove(component_id)
            .map(|(_, consumer)| consumer)
    }

    fn take_forwarder_consumer(&mut self, component_id: &ComponentId) -> Option<PayloadsConsumer> {
        self.forwarder_consumers
            .remove(component_id)
            .map(|(_, consumer)| consumer)
    }

    fn generate_interconnects(&mut self, graph: &Graph) -> Result<(), GenericError> {
        // Collect and iterate over each outbound edge in the topology graph.
        //
        // For each upstream component ("from" side of the edge), we attach each downstream component ("to" side of the edge) to it,
        // creating the relevant dispatcher or consumer if necessary.
        let outbound_edges = graph.get_outbound_directed_edges();
        for (upstream_id, output_map) in outbound_edges {
            match upstream_id.component_type() {
                ComponentType::Source | ComponentType::Transform => {
                    self.generate_event_interconnect(upstream_id, output_map)?;
                }
                ComponentType::Relay | ComponentType::Encoder => {
                    self.generate_payload_interconnect(upstream_id, output_map)?
                }
                _ => panic!(
                    "Only sources, transforms, relays, and encoders can dispatch events/payloads to downstream components."
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
        let (component_id, component_type, component_context) = component_id.into_parts();

        match component_type {
            ComponentType::Source => self
                .source_dispatchers
                .entry(component_id)
                .or_insert_with(|| EventsDispatcher::new(component_context)),
            ComponentType::Transform => self
                .transform_dispatchers
                .entry(component_id)
                .or_insert_with(|| EventsDispatcher::new(component_context)),
            _ => {
                panic!("Only sources and transforms can dispatch events to downstream components.")
            }
        }
    }

    fn get_or_create_events_sender(&mut self, component_id: TypedComponentId) -> mpsc::Sender<EventsBuffer> {
        let (component_id, component_type, component_context) = component_id.into_parts();
        let interconnect_capacity = self.interconnect_capacity;

        let (sender, _) = match component_type {
            ComponentType::Transform => self
                .transform_consumers
                .entry(component_id)
                .or_insert_with(|| build_events_consumer_pair(component_context, interconnect_capacity)),
            ComponentType::Destination => self
                .destination_consumers
                .entry(component_id)
                .or_insert_with(|| build_events_consumer_pair(component_context, interconnect_capacity)),
            ComponentType::Encoder => self
                .encoder_consumers
                .entry(component_id)
                .or_insert_with(|| build_events_consumer_pair(component_context, interconnect_capacity)),
            _ => panic!("Only transforms, destinations, and encoders can consume events."),
        };

        sender.clone()
    }

    fn get_or_create_payloads_dispatcher(&mut self, component_id: TypedComponentId) -> &mut PayloadsDispatcher {
        let (component_id, component_type, component_context) = component_id.into_parts();

        match component_type {
            ComponentType::Relay => self
                .relay_dispatchers
                .entry(component_id)
                .or_insert_with(|| PayloadsDispatcher::new(component_context)),
            ComponentType::Encoder => self
                .encoder_dispatchers
                .entry(component_id)
                .or_insert_with(|| PayloadsDispatcher::new(component_context)),
            _ => {
                panic!("Only relays and encoders can dispatch payloads to downstream components.")
            }
        }
    }

    fn get_or_create_payloads_sender(&mut self, component_id: TypedComponentId) -> mpsc::Sender<PayloadsBuffer> {
        let (component_id, component_type, component_context) = component_id.into_parts();
        let interconnect_capacity = self.interconnect_capacity;

        let (sender, _) = match component_type {
            ComponentType::Forwarder => self
                .forwarder_consumers
                .entry(component_id)
                .or_insert_with(|| build_payloads_consumer_pair(component_context, interconnect_capacity)),
            _ => panic!("Only forwarders can consume payloads."),
        };

        sender.clone()
    }
}

fn build_events_consumer_pair(
    component_context: ComponentContext, interconnect_capacity: NonZeroUsize,
) -> (mpsc::Sender<EventsBuffer>, EventsConsumer) {
    let (sender, receiver) = mpsc::channel(interconnect_capacity.get());
    let consumer = EventsConsumer::new(component_context, receiver);
    (sender, consumer)
}

fn build_payloads_consumer_pair(
    component_context: ComponentContext, interconnect_capacity: NonZeroUsize,
) -> (mpsc::Sender<PayloadsBuffer>, PayloadsConsumer) {
    let (sender, receiver) = mpsc::channel(interconnect_capacity.get());
    let consumer = PayloadsConsumer::new(component_context, receiver);
    (sender, consumer)
}

fn spawn_component<F>(
    join_set: &mut JoinSet<Result<(), GenericError>>, context: ComponentContext,
    allocation_group_token: AllocationGroupToken, component_future: F,
) -> AbortHandle
where
    F: Future<Output = Result<(), GenericError>> + Send + 'static,
{
    let component_span = error_span!(
        "component",
        "type" = context.component_type().as_str(),
        id = %context.component_id(),
    );

    let _span = component_span.enter();
    let _guard = allocation_group_token.enter();

    let component_task_name = format!(
        "topology-{}-{}",
        context.component_type().as_str(),
        context.component_id()
    );
    join_set.spawn_traced_named(component_task_name, component_future)
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
        let _ = ComponentInterconnects::from_graph(interconnect_capacity, &graph)
            .expect("should build interconnects successfully");
    }
}

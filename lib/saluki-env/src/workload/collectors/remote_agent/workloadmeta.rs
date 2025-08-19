use async_trait::async_trait;
use datadog_protos::agent::{Container, KubernetesPod, WorkloadmetaEventType};
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::origin::ExternalData;
use saluki_error::GenericError;
use saluki_health::Health;
use saluki_metrics::static_metrics;
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    MetaString,
};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace, warn};

use crate::{
    helpers::remote_agent::RemoteAgentClient,
    workload::{collectors::MetadataCollector, metadata::MetadataOperation, EntityId},
};

static_metrics!(
   name => Telemetry,
   prefix => remote_workloadmeta_metadata_collector,
   metrics => [
       counter(rpc_errors_total),
       counter(intern_failed_total),
       counter(events_kubernetes_pod_updated_total),
       counter(events_kubernetes_pod_removed_total),
       counter(events_container_updated_total),
       counter(events_container_removed_total),
   ],
);

#[derive(Clone, Copy)]
enum EventType {
    CreatedOrUpdated,
    Removed,
}

/// A workload provider that uses the remote workload metadata API from a Datadog Agent to provide workload information.
pub struct RemoteAgentWorkloadMetadataCollector {
    client: RemoteAgentClient,
    health: Health,
    interner: GenericMapInterner,
    telemetry: Telemetry,
}

impl RemoteAgentWorkloadMetadataCollector {
    /// Creates a new `RemoteAgentWorkloadMetadataCollector` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client cannot be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            client,
            health,
            interner,
            telemetry: Telemetry::new(),
        })
    }

    fn try_intern(&self, value: &str) -> Option<MetaString> {
        match self.interner.try_intern(value) {
            Some(interned) => Some(MetaString::from(interned)),
            None => {
                self.telemetry.intern_failed_total().increment(1);
                None
            }
        }
    }

    async fn handle_kubernetes_pod_event(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>, event_type: EventType,
        kubernetes_pod: KubernetesPod,
    ) -> Result<(), GenericError> {
        match event_type {
            EventType::CreatedOrUpdated => self.telemetry.events_kubernetes_pod_updated_total().increment(1),
            EventType::Removed => self.telemetry.events_kubernetes_pod_removed_total().increment(1),
        }

        let pod_uid = match kubernetes_pod.entity_id.as_ref() {
            Some(entity_id) => match self.try_intern(&entity_id.id) {
                Some(pod_uid) => pod_uid,
                None => {
                    trace!("Failed to intern pod UID for pod; skipping.");
                    return Ok(());
                }
            },
            None => {
                trace!("Received Kubernetes Pod event without UID; skipping.");
                return Ok(());
            }
        };

        match event_type {
            EventType::CreatedOrUpdated => {
                self.handle_kubernetes_pod_created_updated_event(operations_tx, pod_uid, kubernetes_pod)
                    .await
            }
            EventType::Removed => {
                self.handle_kubernetes_pod_removed_event(operations_tx, pod_uid, kubernetes_pod)
                    .await
            }
        }
    }

    async fn handle_kubernetes_pod_created_updated_event(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>, pod_uid: MetaString, kubernetes_pod: KubernetesPod,
    ) -> Result<(), GenericError> {
        // When a pod is created/updated, generate External Data entries for every container within the pod.
        let init_containers = kubernetes_pod.init_containers.iter().map(|container| (container, true));
        let non_init_containers = kubernetes_pod.containers.iter().map(|container| (container, false));

        for (container, is_init) in init_containers.chain(non_init_containers) {
            let container_name = match self.try_intern(&container.name) {
                Some(container_name) => container_name,
                None => {
                    trace!("Failed to intern name for container; skipping.");
                    continue;
                }
            };

            let entity_id = match self.try_intern(&container.id) {
                Some(entity_id) => EntityId::Container(entity_id),
                None => {
                    trace!("Failed to generate interned entity ID for container; skipping.");
                    continue;
                }
            };

            let external_data = ExternalData::new(pod_uid.clone(), container_name, is_init);
            let operation = MetadataOperation::attach_external_data(entity_id, external_data);
            send_metadata_operation(operations_tx, operation).await;
        }

        Ok(())
    }

    async fn handle_kubernetes_pod_removed_event(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>, pod_uid: MetaString, kubernetes_pod: KubernetesPod,
    ) -> Result<(), GenericError> {
        // When a pod is removed, generate deletion events for every container attached to it, as well for the pod itself.
        let operation = MetadataOperation::delete(EntityId::PodUid(pod_uid));
        send_metadata_operation(operations_tx, operation).await;

        let init_containers = kubernetes_pod.init_containers.iter();
        let non_init_containers = kubernetes_pod.containers.iter();

        for container in init_containers.chain(non_init_containers) {
            let container_entity_id = match self.try_intern(&container.id) {
                Some(entity_id) => EntityId::Container(entity_id),
                None => {
                    trace!("Failed to generate interned entity ID for container; skipping.");
                    continue;
                }
            };

            let operation = MetadataOperation::delete(container_entity_id);
            send_metadata_operation(operations_tx, operation).await;
        }

        Ok(())
    }

    async fn handle_container_event(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>, event_type: EventType, container: Container,
    ) -> Result<(), GenericError> {
        match event_type {
            EventType::CreatedOrUpdated => self.telemetry.events_container_updated_total().increment(1),
            EventType::Removed => self.telemetry.events_container_removed_total().increment(1),
        }

        let container_id = match container.entity_id.as_ref() {
            Some(entity_id) => match self.try_intern(&entity_id.id) {
                Some(container_id) => container_id,
                None => {
                    trace!("Failed to intern container ID for container; skipping.");
                    return Ok(());
                }
            },
            None => {
                trace!("Received Container event without ID; skipping.");
                return Ok(());
            }
        };

        match event_type {
            // We don't do anything for created/update events yet.
            EventType::CreatedOrUpdated => Ok(()),
            EventType::Removed => {
                self.handle_container_removed_event(operations_tx, container_id, container)
                    .await
            }
        }
    }

    async fn handle_container_removed_event(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>, container_id: MetaString, _container: Container,
    ) -> Result<(), GenericError> {
        // When a container is removed, generate a deletion event.
        let operation = MetadataOperation::delete(EntityId::Container(container_id));
        send_metadata_operation(operations_tx, operation).await;
        Ok(())
    }
}

#[async_trait]
impl MetadataCollector for RemoteAgentWorkloadMetadataCollector {
    fn name(&self) -> &'static str {
        "remote-agent-wmeta"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        let mut entity_stream = self.client.get_workloadmeta_stream();
        debug!("Established workload metadata entity stream.");

        loop {
            select! {
                _ = self.health.live() => {},
                result = entity_stream.next() => match result {
                    Some(Ok(response)) => {
                        trace!("Received workload metadata stream event.");

                        for event in response.events {
                            let event_type = match WorkloadmetaEventType::try_from(event.r#type) {
                                Ok(event_type) => match event_type {
                                    WorkloadmetaEventType::EventTypeSet => EventType::CreatedOrUpdated,
                                    WorkloadmetaEventType::EventTypeUnset => EventType::Removed,
                                    WorkloadmetaEventType::EventTypeAll => {
                                        warn!("Received WLM events with 'all' type, which should only be present in requests.");
                                        continue;
                                    }
                                },
                                Err(e) => {
                                    trace!("Failed to parse workload metadata event type: {}", e);
                                    continue;
                                },
                            };

                            if let Some(kubernetes_pod) = event.kubernetes_pod {
                                self.handle_kubernetes_pod_event(operations_tx, event_type, kubernetes_pod).await?;
                            }

                            if let Some(container) = event.container {
                                self.handle_container_event(operations_tx, event_type, container).await?;
                            }
                        }

                        trace!("Processed workload metadata stream event.");
                    },
                    Some(Err(e)) => {
                        self.telemetry.rpc_errors_total().increment(1);
                        return Err(e.into())
                    },
                    None => break,
                }
            }
        }

        self.health.mark_not_ready();

        Ok(())
    }
}

impl MemoryBounds for RemoteAgentWorkloadMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the gRPC client can really be bounded at the
        // moment.
        builder
            .firm()
            .with_fixed_amount("self struct", std::mem::size_of::<Self>());
    }
}

async fn send_metadata_operation(operations_tx: &mut mpsc::Sender<MetadataOperation>, operation: MetadataOperation) {
    if let Err(e) = operations_tx.send(operation).await {
        debug!(error = %e, "Failed to send metadata operation.");
    }
}

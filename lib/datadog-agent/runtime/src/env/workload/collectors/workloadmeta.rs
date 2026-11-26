use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use datadog_protos::agent::{Container, KubernetesPod, OrchestratorContainer, WorkloadmetaEventType};
use futures::{StreamExt as _, TryStreamExt as _};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    data_model::origin::ExternalData,
    health::Health,
};
use saluki_env::workload::{collectors::MetadataCollector, EntityId, MetadataOperation};
use saluki_error::GenericError;
use saluki_io::net::util::tonic::StatusError;
use saluki_metrics::{static_metrics, Counter};
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    MetaString,
};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace, warn};

#[static_metrics(prefix = remote_workloadmeta_metadata_collector)]
#[derive(Clone)]
struct Telemetry {
    rpc_errors_total: Counter,
    intern_failed_total: Counter,
    events_kubernetes_pod_updated_total: Counter,
    events_kubernetes_pod_removed_total: Counter,
    events_container_updated_total: Counter,
    events_container_removed_total: Counter,
}

#[derive(Clone, Copy)]
enum EventType {
    CreatedOrUpdated,
    Removed,
}

/// A workload provider that uses the remote workload metadata API from a Datadog Agent to provide workload information.
pub struct RemoteAgentWorkloadMetadataCollector {
    client: RemoteAgentClient,
    health: Health,
    event_handler: EventHandler,
}

impl RemoteAgentWorkloadMetadataCollector {
    /// Creates a new `RemoteAgentWorkloadMetadataCollector` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client can't be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn new(
        client_config: &RemoteAgentClientConfiguration, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::connect(client_config).await?;

        Ok(Self {
            client,
            health,
            event_handler: EventHandler::new(interner),
        })
    }
}

/// Turns workloadmeta events into metadata operations.
struct EventHandler {
    interner: GenericMapInterner,
    telemetry: Telemetry,
}

impl EventHandler {
    fn new(interner: GenericMapInterner) -> Self {
        Self {
            interner,
            telemetry: Telemetry::new(),
        }
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
        for container in pod_containers(&kubernetes_pod) {
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

            let external_data = ExternalData::new(pod_uid.clone(), container_name);
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

        for container in pod_containers(&kubernetes_pod) {
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
        "remote_agent_wmeta"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        let event_handler = &self.event_handler;
        self.health.mark_ready();

        let mut entity_stream = self.client.get_workloadmeta_stream().map_err(StatusError::from);
        debug!("Established workload metadata entity stream.");

        loop {
            select! {
                _ = self.health.live() => {},
                maybe_response = entity_stream.next() => match maybe_response {
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
                                event_handler.handle_kubernetes_pod_event(operations_tx, event_type, kubernetes_pod).await?;
                            }

                            if let Some(container) = event.container {
                                event_handler.handle_container_event(operations_tx, event_type, container).await?;
                            }
                        }

                        trace!("Processed workload metadata stream event.");
                    },
                    Some(Err(e)) => {
                        event_handler.telemetry.rpc_errors_total().increment(1);
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

/// Returns every container in the pod: init, regular, and ephemeral containers.
///
/// The Agent resolves External Data against all three lists.
fn pod_containers(kubernetes_pod: &KubernetesPod) -> impl Iterator<Item = &OrchestratorContainer> {
    kubernetes_pod
        .init_containers
        .iter()
        .chain(&kubernetes_pod.containers)
        .chain(&kubernetes_pod.ephemeral_containers)
}

async fn send_metadata_operation(operations_tx: &mut mpsc::Sender<MetadataOperation>, operation: MetadataOperation) {
    if let Err(e) = operations_tx.send(operation).await {
        debug!(error = %e, "Failed to send metadata operation.");
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use datadog_protos::agent::WorkloadmetaEntityId;
    use saluki_core::data_model::origin::RawExternalData;
    use saluki_env::workload::{aggregator::MetadataStore as _, stores::ExternalDataStore, MetadataAction};

    use super::*;

    const POD_UID: &str = "pod-uid";

    #[derive(Debug, PartialEq)]
    enum Op {
        Attach(EntityId, ExternalData),
        Delete(EntityId),
    }

    fn event_handler() -> EventHandler {
        EventHandler::new(GenericMapInterner::new(NonZeroUsize::new(64 * 1024).unwrap()))
    }

    fn orchestrator_container(id: &str, name: &str) -> OrchestratorContainer {
        OrchestratorContainer {
            id: id.to_string(),
            name: name.to_string(),
            image: None,
        }
    }

    fn pod_with_all_container_kinds() -> KubernetesPod {
        KubernetesPod {
            entity_id: Some(WorkloadmetaEntityId {
                id: POD_UID.to_string(),
                ..Default::default()
            }),
            init_containers: vec![orchestrator_container("init-id", "init")],
            containers: vec![orchestrator_container("app-id", "app")],
            ephemeral_containers: vec![orchestrator_container("debugger-id", "debugger")],
            ..Default::default()
        }
    }

    async fn pod_event_operations(
        handler: &EventHandler, event_type: EventType, pod: KubernetesPod,
    ) -> Vec<MetadataOperation> {
        let (mut operations_tx, mut operations_rx) = mpsc::channel(16);
        handler
            .handle_kubernetes_pod_event(&mut operations_tx, event_type, pod)
            .await
            .unwrap();
        drop(operations_tx);

        let mut operations = Vec::new();
        while let Some(operation) = operations_rx.recv().await {
            operations.push(operation);
        }
        operations
    }

    fn summarize(operations: &[MetadataOperation]) -> Vec<Op> {
        operations
            .iter()
            .flat_map(|operation| {
                operation.actions.clone().into_iter().map(|action| match action {
                    MetadataAction::AttachExternalData { external_data } => {
                        Op::Attach(operation.entity_id.clone(), external_data)
                    }
                    MetadataAction::Delete => Op::Delete(operation.entity_id.clone()),
                    other => panic!("unexpected action: {other:?}"),
                })
            })
            .collect()
    }

    fn container(id: &str) -> EntityId {
        EntityId::Container(id.into())
    }

    fn external_data(container_name: &str) -> ExternalData {
        ExternalData::new(POD_UID.into(), container_name.into())
    }

    #[tokio::test]
    async fn pod_update_attaches_external_data_for_every_container_kind() {
        let handler = event_handler();
        let operations =
            pod_event_operations(&handler, EventType::CreatedOrUpdated, pod_with_all_container_kinds()).await;

        assert_eq!(
            summarize(&operations),
            vec![
                Op::Attach(container("init-id"), external_data("init")),
                Op::Attach(container("app-id"), external_data("app")),
                Op::Attach(container("debugger-id"), external_data("debugger")),
            ]
        );
    }

    #[tokio::test]
    async fn pod_removal_deletes_the_pod_and_every_container_kind() {
        let handler = event_handler();
        let operations = pod_event_operations(&handler, EventType::Removed, pod_with_all_container_kinds()).await;

        assert_eq!(
            summarize(&operations),
            vec![
                Op::Delete(EntityId::PodUid(POD_UID.into())),
                Op::Delete(container("init-id")),
                Op::Delete(container("app-id")),
                Op::Delete(container("debugger-id")),
            ]
        );
    }

    #[tokio::test]
    async fn external_data_store_resolves_ephemeral_container_until_pod_removal() {
        let handler = event_handler();
        let mut store = ExternalDataStore::with_entity_limit(NonZeroUsize::new(16).unwrap());
        let resolver = store.resolver();
        let raw = format!("pu-{POD_UID},cn-debugger,it-false");
        let raw = RawExternalData::try_from_str(&raw).unwrap();

        for operation in
            pod_event_operations(&handler, EventType::CreatedOrUpdated, pod_with_all_container_kinds()).await
        {
            store.process_operation(operation);
        }
        let resolved = resolver.resolve(&raw).expect("ephemeral container should resolve");
        assert_eq!(resolved.pod_entity_id(), &EntityId::PodUid(POD_UID.into()));
        assert_eq!(resolved.container_entity_id(), &container("debugger-id"));

        for operation in pod_event_operations(&handler, EventType::Removed, pod_with_all_container_kinds()).await {
            store.process_operation(operation);
        }
        assert_eq!(resolver.resolve(&raw), None);
    }
}

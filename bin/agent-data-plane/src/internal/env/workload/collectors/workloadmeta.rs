use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_protos::agent::{Container, KubernetesPod, OrchestratorContainer, WorkloadmetaEventType};
use futures::StreamExt as _;
use saluki_context::origin::ExternalData;
use saluki_env::workload::{collectors::MetadataCollector, EntityId, MetadataOperation};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc;

use super::entity_id_from_workloadmeta;

/// Collects workload metadata relationships from the Datadog Agent stream.
#[derive(Clone)]
pub struct WorkloadmetaMetadataCollector {
    client: RemoteAgentClient,
}

impl WorkloadmetaMetadataCollector {
    /// Creates a workload metadata collector.
    pub fn new(client: RemoteAgentClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MetadataCollector for WorkloadmetaMetadataCollector {
    fn name(&self) -> &'static str {
        "workload/workloadmeta"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        let mut client = self.client.clone();
        let mut stream = client.get_workloadmeta_stream();

        while let Some(result) = stream.next().await {
            let response = result?;
            for event in response.events {
                let event_type =
                    WorkloadmetaEventType::try_from(event.r#type).unwrap_or(WorkloadmetaEventType::EventTypeSet);
                let is_delete = event_type == WorkloadmetaEventType::EventTypeUnset;

                if let Some(container) = event.container {
                    process_container(operations_tx, container, is_delete).await?;
                }
                if let Some(pod) = event.kubernetes_pod {
                    process_kubernetes_pod(operations_tx, pod, is_delete).await?;
                }
            }
        }

        Err(generic_error!("Datadog Agent workloadmeta stream ended."))
    }
}

async fn process_container(
    operations_tx: &mut mpsc::Sender<MetadataOperation>, container: Container, is_delete: bool,
) -> Result<(), GenericError> {
    let Some(container_entity_id) = entity_id_from_workloadmeta(container.entity_id) else {
        return Ok(());
    };
    if container.pid <= 0 {
        return Ok(());
    }

    let pid_entity_id = EntityId::ContainerPid(container.pid as u32);
    let operation = if is_delete {
        MetadataOperation::remove_alias(pid_entity_id, container_entity_id)
    } else {
        MetadataOperation::add_alias(pid_entity_id, container_entity_id)
    };
    operations_tx.send(operation).await?;
    Ok(())
}

async fn process_kubernetes_pod(
    operations_tx: &mut mpsc::Sender<MetadataOperation>, pod: KubernetesPod, is_delete: bool,
) -> Result<(), GenericError> {
    let Some(pod_uid) = pod
        .entity_id
        .as_ref()
        .map(|id| id.id.clone())
        .filter(|id| !id.is_empty())
    else {
        return Ok(());
    };

    for container in pod.containers {
        process_orchestrator_container(operations_tx, &pod_uid, container, false, is_delete).await?;
    }
    for container in pod.init_containers {
        process_orchestrator_container(operations_tx, &pod_uid, container, true, is_delete).await?;
    }
    for container in pod.ephemeral_containers {
        process_orchestrator_container(operations_tx, &pod_uid, container, false, is_delete).await?;
    }

    Ok(())
}

async fn process_orchestrator_container(
    operations_tx: &mut mpsc::Sender<MetadataOperation>, pod_uid: &str, container: OrchestratorContainer,
    init_container: bool, is_delete: bool,
) -> Result<(), GenericError> {
    if container.id.is_empty() || container.name.is_empty() {
        return Ok(());
    }

    let container_entity_id = EntityId::Container(container.id.into());
    let operation = if is_delete {
        MetadataOperation::delete(container_entity_id)
    } else {
        let external_data = ExternalData::new(pod_uid.to_string().into(), container.name.into(), init_container);
        MetadataOperation::attach_external_data(container_entity_id, external_data)
    };
    operations_tx.send(operation).await?;
    Ok(())
}

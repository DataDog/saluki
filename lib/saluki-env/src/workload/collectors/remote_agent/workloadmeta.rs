use async_trait::async_trait;
use datadog_protos::agent::{KubernetesPod, WorkloadmetaEventType};
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_health::Health;
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace};

use crate::{
    helpers::remote_agent::RemoteAgentClient,
    workload::{collectors::MetadataCollector, external_data::ExternalData, metadata::MetadataOperation, EntityId},
};

/// A workload provider that uses the remote workload metadata API from a Datadog Agent to provide workload information.
pub struct RemoteAgentWorkloadMetadataCollector {
    client: RemoteAgentClient,
    health: Health,
    interner: GenericMapInterner,
}

impl RemoteAgentWorkloadMetadataCollector {
    /// Creates a new `RemoteAgentWorkloadMetadataCollector` from the given configuration.
    ///
    /// ## Errors
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
        })
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
                                Ok(event_type) => event_type,
                                Err(e) => {
                                    trace!("Failed to parse workload metadata event type: {}", e);
                                    continue;
                                },
                            };

                            if event_type == WorkloadmetaEventType::EventTypeSet {
                                // If a Kubernetes Pod entity is being updated, generate External Data entries for it.
                                if let Some(kubernetes_pod) = event.kubernetes_pod {
                                    process_kubernetes_pod_external_data(kubernetes_pod, &self.interner, operations_tx).await?;
                                }
                            }
                        }

                        trace!("Processed workload metadata stream event.");
                    },
                    Some(Err(e)) => return Err(e.into()),
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

async fn process_kubernetes_pod_external_data(
    kubernetes_pod: KubernetesPod, interner: &GenericMapInterner, operations_tx: &mut mpsc::Sender<MetadataOperation>,
) -> Result<(), GenericError> {
    let raw_pod_uid = match kubernetes_pod.entity_id {
        Some(entity_id) => entity_id.id,
        None => {
            trace!("Received Kubernetes Pod event without UID; skipping.");
            return Ok(());
        }
    };

    let pod_uid = match interner.try_intern(&raw_pod_uid) {
        Some(pod_uid) => MetaString::from(pod_uid),
        None => {
            trace!("Failed to intern pod UID for pod; skipping.");
            return Ok(());
        }
    };

    for container in kubernetes_pod
        .containers
        .iter()
        .chain(kubernetes_pod.init_containers.iter())
    {
        let container_name = match interner.try_intern(&container.name) {
            Some(container_name) => MetaString::from(container_name),
            None => {
                trace!("Failed to intern name for container; skipping.");
                continue;
            }
        };

        let entity_id = match interner.try_intern(&container.id) {
            Some(entity_id) => EntityId::Container(entity_id.into()),
            None => {
                trace!("Failed to generate interned entity ID for container; skipping.");
                continue;
            }
        };

        let external_data = ExternalData::new(pod_uid.clone(), container_name);
        let operation = MetadataOperation::attach_external_data(entity_id, external_data);
        if let Err(e) = operations_tx.send(operation).await {
            debug!(error = %e, "Failed to send metadata operation.");
        }
    }

    Ok(())
}

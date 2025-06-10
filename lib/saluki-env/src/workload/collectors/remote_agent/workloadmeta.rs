use async_trait::async_trait;
use datadog_protos::agent::{KubernetesPod, WorkloadmetaEventType};
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::origin::ExternalData;
use saluki_error::GenericError;
use saluki_health::Health;
use saluki_metrics::static_metrics;
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace};

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
   ],
);

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

                            // If a Kubernetes Pod entity is being updated, generate External Data entries for it.
                            if let Some(kubernetes_pod) = event.kubernetes_pod {
                                match event_type {
                                    WorkloadmetaEventType::EventTypeSet => {
                                        self.telemetry.events_kubernetes_pod_updated_total().increment(1);
                                        process_kubernetes_pod_external_data(kubernetes_pod, &self.interner, &self.telemetry, operations_tx).await?;
                                    },
                                    WorkloadmetaEventType::EventTypeUnset => {
                                        self.telemetry.events_kubernetes_pod_removed_total().increment(1);
                                        // TODO: Potentially handle removal of external data here. For now, we just want
                                        // telemetry about removals.
                                    },
                                    _ => continue,
                                }
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

async fn process_kubernetes_pod_external_data(
    kubernetes_pod: KubernetesPod, interner: &GenericMapInterner, telemetry: &Telemetry,
    operations_tx: &mut mpsc::Sender<MetadataOperation>,
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
            telemetry.intern_failed_total().increment(1);
            trace!("Failed to intern pod UID for pod; skipping.");
            return Ok(());
        }
    };

    let init_containers = kubernetes_pod.init_containers.iter().map(|container| (container, true));
    let non_init_containers = kubernetes_pod.containers.iter().map(|container| (container, false));

    for (container, is_init) in init_containers.chain(non_init_containers) {
        let container_name = match interner.try_intern(&container.name) {
            Some(container_name) => MetaString::from(container_name),
            None => {
                telemetry.intern_failed_total().increment(1);
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

        let external_data = ExternalData::new(pod_uid.clone(), container_name, is_init);
        let operation = MetadataOperation::attach_external_data(entity_id, external_data);
        if let Err(e) = operations_tx.send(operation).await {
            debug!(error = %e, "Failed to send metadata operation.");
        }
    }

    Ok(())
}

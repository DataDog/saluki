use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use containerd_client::services::v1::Namespace;
use futures::{stream::select_all, Stream, StreamExt as _};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use stringtheory::interning::FixedSizeInterner;
use tokio::{sync::mpsc, time::sleep};
use tracing::{error, warn};

use crate::workload::{
    entity::EntityId,
    helpers::containerd::{
        events::{ContainerdEvent, ContainerdTopic},
        ContainerdClient,
    },
    metadata::MetadataOperation,
};

use super::MetadataCollector;

static CONTAINERD_WATCH_EVENTS: &[ContainerdTopic] = &[ContainerdTopic::TaskStarted, ContainerdTopic::TaskDeleted];

/// A metadata collector that watches for updates from containerd.
pub struct ContainerdMetadataCollector {
    client: ContainerdClient,
    watched_namespaces: Vec<Namespace>,
    tag_interner: FixedSizeInterner<1>,
}

impl ContainerdMetadataCollector {
    /// Creates a new `ContainerdMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the containerd gRPC client cannot be created, or listing the namespaces in the containerd runtime fails, an
    /// error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, tag_interner: FixedSizeInterner<1>,
    ) -> Result<Self, GenericError> {
        let client = ContainerdClient::from_configuration(config).await?;
        let watched_namespaces = client.list_namespaces().await?;

        Ok(Self {
            client,
            watched_namespaces,
            tag_interner,
        })
    }
}

#[async_trait]
impl MetadataCollector for ContainerdMetadataCollector {
    fn name(&self) -> &'static str {
        "containerd"
    }

    async fn watch(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Create a watcher for each namespace, and then join all of their watch streams, which then we'll just funnel
        // back to the operations channel.
        let watchers = self
            .watched_namespaces
            .iter()
            .map(|ns| NamespaceWatcher::new(self.client.clone(), ns.clone(), self.tag_interner.clone()).watch());

        let mut operations_stream = select_all(watchers);

        while let Some(operation) = operations_stream.next().await {
            operations_tx.send(operation).await?;
        }

        Ok(())
    }
}

impl MemoryBounds for ContainerdMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the gRPC client can really be bounded at the
        // moment, and we also don't have any way to know the number of namespaces we'll be monitoring a priori.
        builder.firm().with_fixed_amount(std::mem::size_of::<Self>());
    }
}

struct NamespaceWatcher {
    namespace: Namespace,
    client: ContainerdClient,
    tag_interner: FixedSizeInterner<1>,
}

impl NamespaceWatcher {
    fn new(client: ContainerdClient, namespace: Namespace, tag_interner: FixedSizeInterner<1>) -> Self {
        Self {
            client,
            namespace,
            tag_interner,
        }
    }

    async fn process_event(&self, event: ContainerdEvent) -> Option<MetadataOperation> {
        match event {
            ContainerdEvent::TaskStarted { id, pid } => {
                let pid_entity_id = EntityId::ContainerPid(pid);
                let container_entity_id = EntityId::Container(id);
                Some(MetadataOperation::link_ancestor(pid_entity_id, container_entity_id))
            }
            ContainerdEvent::TaskDeleted { pid, .. } => Some(MetadataOperation::delete(EntityId::ContainerPid(pid))),
        }
    }

    async fn build_initial_metadata_operations(&self) -> Option<Vec<MetadataOperation>> {
        let mut operations = Vec::new();

        // Get a list of all containers in the namespace.
        let containers = match self.client.list_containers(&self.namespace).await {
            Ok(containers) => containers,
            Err(e) => {
                error!(namespace = self.namespace.name, error = %e, "Error listing containers.");
                return None;
            }
        };

        for container in containers {
            let pids = match self
                .client
                .list_pids_for_container(&self.namespace, container.id.clone())
                .await
            {
                Ok(pids) => pids,
                Err(e) => {
                    if let Some(status) = e.as_response_error() {
                        if status.code() == tonic::Code::NotFound {
                            // The container may have been deleted before we could get the PIDs for it, so we'll just
                            // skip it without making a fuss.
                            continue;
                        }
                    }

                    error!(namespace = self.namespace.name, container_id = container.id, error = %e, "Error getting PIDs for container.");
                    continue;
                }
            };

            for pid in pids {
                let pid_entity_id = EntityId::ContainerPid(pid);

                match self.tag_interner.try_intern(container.id.as_str()) {
                    Some(container_id) => {
                        let container_entity_id = EntityId::Container(container_id.into());
                        operations.push(MetadataOperation::link_ancestor(pid_entity_id, container_entity_id));
                    }
                    None => {
                        warn!(
                            namespace = self.namespace.name,
                            container_id = container.id,
                            container_task_pid = pid,
                            "Failed to intern container ID. Container ID/task PID link will not be created."
                        );
                    }
                }
            }
        }

        Some(operations)
    }

    fn watch(self) -> impl Stream<Item = MetadataOperation> + Unpin {
        // We watch the given namespace for all of the relevant events, and convert those into metadata operations that
        // we pass back to be collected by the parent watcher task, which then forwards them to the metadata aggregator.
        Box::pin(stream! {
            // Do an initial scan of the namespace to get all of the existing containers, their tasks and images, and
            // so on, and generate metadata operations from that as a way to prime the store.
            if let Some(initial_operations) = self.build_initial_metadata_operations().await {
                for operation in initial_operations {
                    yield operation;
                }
            }

            // Now watch for events.
            loop {
                let mut event_stream = match self.client.watch_events(CONTAINERD_WATCH_EVENTS, &self.namespace).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!(namespace = self.namespace.name, error = %e, "Error watching container events.");

                        sleep(Duration::from_secs(1)).await;
                        continue;
                    },
                };

                while let Some(event_result) = event_stream.next().await {
                    let event = match event_result {
                        Ok(event) => event,
                        Err(e) => {
                            error!(namespace = self.namespace.name, error = %e, "Error watching container events.");
                            continue;
                        },
                    };

                    if let Some(operation) = self.process_event(event).await {
                        yield operation;
                    }
                }
            }
        })
    }
}

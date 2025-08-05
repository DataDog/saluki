use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use containerd_client::services::v1::Namespace;
use futures::{stream::select_all, Stream, StreamExt as _};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_health::Health;
use saluki_metrics::static_metrics;
use stringtheory::interning::{GenericMapInterner, Interner as _};
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};

use super::MetadataCollector;
use crate::workload::{
    entity::EntityId,
    helpers::containerd::{
        events::{ContainerdEvent, ContainerdTopic},
        ContainerdClient,
    },
    metadata::MetadataOperation,
};

static CONTAINERD_WATCH_EVENTS: &[ContainerdTopic] = &[ContainerdTopic::TaskStarted, ContainerdTopic::TaskDeleted];

static_metrics!(
   name => Telemetry,
   prefix => containerd_metadata_collector,
   labels => [namespace: String],
   metrics => [
       counter(rpc_errors_total),
       counter(intern_failed_total),
       counter(events_task_started_total),
       counter(events_task_deleted_total),
   ],
);

/// A metadata collector that watches for updates from containerd.
pub struct ContainerdMetadataCollector {
    client: ContainerdClient,
    watched_namespaces: Vec<Namespace>,
    tag_interner: GenericMapInterner,
    health: Health,
}

impl ContainerdMetadataCollector {
    /// Creates a new `ContainerdMetadataCollector` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the containerd gRPC client cannot be created, or listing the namespaces in the containerd runtime fails, an
    /// error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, health: Health, tag_interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = ContainerdClient::from_configuration(config).await?;
        let watched_namespaces = client.list_namespaces().await?;

        Ok(Self {
            client,
            watched_namespaces,
            tag_interner,
            health,
        })
    }
}

#[async_trait]
impl MetadataCollector for ContainerdMetadataCollector {
    fn name(&self) -> &'static str {
        "containerd"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        // Create a watcher for each namespace, and then join all of their watch streams, which then we'll just funnel
        // back to the operations channel.
        let watchers = self
            .watched_namespaces
            .iter()
            .map(|ns| NamespaceWatcher::new(self.client.clone(), ns.clone(), self.tag_interner.clone()).watch());

        let mut operations_stream = select_all(watchers);

        loop {
            select! {
                _ = self.health.live() => {},
                maybe_operation = operations_stream.next() => match maybe_operation {
                    Some(operation) => {
                        operations_tx.send(operation).await?;
                    },
                    None => break,
                },
            }
        }

        self.health.mark_not_ready();

        Ok(())
    }
}

impl MemoryBounds for ContainerdMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the gRPC client can really be bounded at the
        // moment, and we also don't have any way to know the number of namespaces we'll be monitoring a priori.
        builder
            .firm()
            .with_fixed_amount("self struct", std::mem::size_of::<Self>());
    }
}

struct NamespaceWatcher {
    namespace: Namespace,
    client: ContainerdClient,
    tag_interner: GenericMapInterner,
    telemetry: Telemetry,
}

impl NamespaceWatcher {
    fn new(client: ContainerdClient, namespace: Namespace, tag_interner: GenericMapInterner) -> Self {
        let telemetry = Telemetry::new(namespace.name.clone());
        Self {
            client,
            namespace,
            tag_interner,
            telemetry,
        }
    }

    async fn process_event(&self, event: ContainerdEvent) -> Option<MetadataOperation> {
        match event {
            ContainerdEvent::TaskStarted { id, pid } => {
                self.telemetry.events_task_started_total().increment(1);
                let pid_entity_id = EntityId::ContainerPid(pid);
                let container_entity_id = EntityId::Container(id);
                Some(MetadataOperation::add_alias(pid_entity_id, container_entity_id))
            }
            ContainerdEvent::TaskDeleted { pid, .. } => {
                self.telemetry.events_task_deleted_total().increment(1);
                Some(MetadataOperation::delete(EntityId::ContainerPid(pid)))
            }
        }
    }

    async fn build_initial_metadata_operations(&self) -> Option<Vec<MetadataOperation>> {
        let mut operations = Vec::new();

        // Get a list of all containers in the namespace.
        let containers = match self.client.list_containers(&self.namespace).await {
            Ok(containers) => containers,
            Err(e) => {
                self.telemetry.rpc_errors_total().increment(1);
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

                    self.telemetry.rpc_errors_total().increment(1);
                    error!(namespace = self.namespace.name, container_id = container.id, error = %e, "Error getting PIDs for container.");
                    continue;
                }
            };

            for pid in pids {
                let pid_entity_id = EntityId::ContainerPid(pid);

                match self.tag_interner.try_intern(container.id.as_str()) {
                    Some(container_id) => {
                        let container_entity_id = EntityId::Container(container_id.into());
                        operations.push(MetadataOperation::add_alias(pid_entity_id, container_entity_id));
                    }
                    None => {
                        self.telemetry.intern_failed_total().increment(1);
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
        debug!(
            namespace = self.namespace.name,
            "Starting containerd namespace watcher."
        );

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
                // TODO: We should be creating this stream -- and polling it! -- before we build our initial metadata
                // operations in order to ensure that we have an overlap between new events and the initial scan.
                let mut event_stream = match self.client.watch_events(CONTAINERD_WATCH_EVENTS, &self.namespace).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        self.telemetry.rpc_errors_total().increment(1);
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

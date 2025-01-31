use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use saluki_health::Health;
use stringtheory::interning::GenericMapInterner;
use tokio::{select, sync::mpsc, time::interval};
use tracing::{debug, error};

use super::MetadataCollector;
use crate::{
    features::FeatureDetector,
    workload::{
        entity::EntityId,
        helpers::cgroups::{CgroupsConfiguration, CgroupsReader},
        metadata::MetadataOperation,
    },
};

/// A metadata collector that observes Linux "Control Groups" (cgroups).
///
/// This collector specifically tracks cgroup controllers attached to container workloads using simple regex-based
/// matching against the controller path, and maintains a mapping of controller inodes to the container ID extracted
/// from the controller path.
///
/// This is specifically used to support client-based Origin Detection in DogStatsD, where clients will either send
/// their detected container ID _or_ the inode of their cgroup controller. A canonical container ID must always be used
/// for origin enrichment, so this mapping allows resolving controller inodes to their canonical container ID.
pub struct CgroupsMetadataCollector {
    reader: CgroupsReader,
    health: Health,
}

impl CgroupsMetadataCollector {
    /// Creates a new `CgroupsMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If a valid cgroups hierarchy can not be located at the configured path, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let cgroups_config = CgroupsConfiguration::from_configuration(config, feature_detector)?;
        let reader = match CgroupsReader::try_from_config(&cgroups_config, interner)? {
            Some(reader) => reader,
            None => {
                return Err(generic_error!("Failed to detect any cgroups v1/v2 hierarchy. "));
            }
        };

        Ok(Self { reader, health })
    }
}

#[async_trait]
impl MetadataCollector for CgroupsMetadataCollector {
    fn name(&self) -> &'static str {
        "cgroups"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        let mut traverse_interval = interval(Duration::from_secs(2));
        let mut reader = Some(self.reader.clone());
        let mut operations = Some(Vec::with_capacity(64));

        // Repeatedly traverse the cgroups v2 hierarchy in a loop, generating ancestry links for controller
        // inode/container ID pairs that we find. We do this using a simple heuristic to determine if the control group
        // name is actually a container ID or something unrelated.
        //
        // We batch these metadata operations and then send them all at the end of the loop.
        loop {
            select! {
                _ = self.health.live() => {},
                _ = traverse_interval.tick() => {
                    // This is a little clunky but necessary in order to reuse the reader/operations given that we have
                    // to transfer ownership when we spawn the blocking task.
                    let old_reader = reader.take().expect("reader should be present");
                    let old_operations = operations.take().expect("operations should be present");

                    let traverse_result = tokio::task::spawn_blocking(|| traverse_cgroups(old_reader, old_operations));
                    match traverse_result.await {
                        Ok(Ok((new_reader, mut new_operations))) => {
                            for operation in new_operations.drain(..) {
                                operations_tx.send(operation).await?;
                            }

                            reader = Some(new_reader);
                            operations = Some(new_operations);
                        },
                        Ok(Err(e)) => error!(error = %e, "Failed to read cgroups."),
                        Err(e) => error!(error = %e, "Failed to spawn cgroups traverse task."),
                    }
                },
            }
        }
    }
}

impl MemoryBounds for CgroupsMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Pre-allocated operation batch buffer. This is only the minimum, as it could grow larger.
            .with_array::<MetadataOperation>(64);
        // TODO: Kind of a throwaway calculation because nothing about the reader can really be bounded at the moment.
        builder.firm().with_fixed_amount(std::mem::size_of::<Self>());
    }
}

fn traverse_cgroups(
    reader: CgroupsReader, mut operations: Vec<MetadataOperation>,
) -> Result<(CgroupsReader, Vec<MetadataOperation>), GenericError> {
    let start = std::time::Instant::now();

    let child_cgroups = reader.get_child_cgroups();
    for child_cgroup in child_cgroups {
        let cgroup_name = child_cgroup.name;
        let container_id = child_cgroup.container_id;
        debug!(%container_id, %cgroup_name, "Found container control group.");

        // Create an ancestry link between the container inode and the container ID.
        let entity_id = EntityId::ContainerInode(child_cgroup.ino);
        let ancestor_entity_id = EntityId::Container(container_id);

        let operation = MetadataOperation::link_ancestor(entity_id, ancestor_entity_id);
        operations.push(operation);
    }

    let elapsed = start.elapsed();
    tracing::info!(elapsed = ?elapsed, "Traversed cgroups.");

    Ok((reader, operations))
}

use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use saluki_health::Health;
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::{select, sync::mpsc};
use tracing::{debug, error, warn};

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
    /// # Errors
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

        let mut poller_handle = None;

        // Drive a blocking background task that polls the cgroups hierarchy on a regular interval, and sends metadata
        // updates when cgroups are created or deleted. We do this in a blocking task since all of the I/O operations
        // are synchronous.
        //
        // Our loop here ensures that we always keep a single instance of the poller running.
        loop {
            // Ensure that we have a poller task running.
            if poller_handle.is_none() {
                let mut cgroups_manager = SynchronousCgroupsManager::from_reader(self.reader.clone());
                let operations_tx = operations_tx.clone();

                poller_handle = Some(tokio::task::spawn_blocking(move || {
                    cgroups_manager.poll(operations_tx)
                }));

                debug!("Spawned cgroups background poller task.");
            }

            select! {
                _ = self.health.live() => {},
                result = poller_handle.as_mut().unwrap() => {
                    // The poller task completed -- for some reason -- so drop our handle to signal that the next loop
                    // iteration needs to recreate it, and provide some information on _why_ the previous task stopped.
                    poller_handle = None;

                    match result {
                        Ok(Ok(())) => unreachable!("Cgroups background poller task should run indefinitely"),
                        Ok(Err(e)) => error!(error = ?e, "Cgroups background poller task encountered an error."),
                        Err(e) => error!(error = ?e, "Cgroups background poller task panicked."),
                    }
                }
            }
        }
    }
}

impl MemoryBounds for CgroupsMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Pre-allocated operation batch buffer. This is only the minimum, as it could grow larger.
            .with_array::<MetadataOperation>("metadata operations", 64);
        // TODO: Kind of a throwaway calculation because nothing about the reader can really be bounded at the moment.
        //
        // Specifically, we don't know the number of cgroups that will be present... and we have a map that both holds
        // the active cgroups _and_ a map to track the cgroups seen during a single traversal, which we need to
        // determine which cgroups have been removed. This means we might end up with like 3 copies of the same cgroup
        // times however many cgroups there are at peak.
        builder.firm().with_single_value::<Self>("component struct");
    }
}

struct SynchronousCgroupsManager {
    reader: CgroupsReader,
    active_cgroups: FastHashMap<u64, MetaString>,
    operations: Vec<MetadataOperation>,
}

impl SynchronousCgroupsManager {
    fn from_reader(reader: CgroupsReader) -> Self {
        Self {
            reader,
            active_cgroups: FastHashMap::default(),
            operations: Vec::with_capacity(64),
        }
    }

    fn poll(&mut self, operations_tx: mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        let mut traversed_cgroups = FastHashSet::default();
        let mut cgroups_to_delete = Vec::new();

        loop {
            // Make sure we should still be running.
            if operations_tx.is_closed() {
                return Ok(());
            }

            traversed_cgroups.clear();

            let start = std::time::Instant::now();

            // Traverse the cgroups hierarchy and collect all child cgroups that we can find that are attached to a
            // container and have a controller inode for us to attach an alias to.
            let child_cgroups = self.reader.get_child_cgroups();
            let child_cgroups_len = child_cgroups.len();
            for child_cgroup in child_cgroups {
                if let Some(cgroup_inode) = child_cgroup.inode() {
                    traversed_cgroups.insert(cgroup_inode);

                    // If we haven't seen this cgroup before, start tracking it.
                    if !self.active_cgroups.contains_key(&cgroup_inode) {
                        let container_id = child_cgroup.into_container_id();
                        debug!(%cgroup_inode, %container_id, "Found new container-based cgroup.");

                        self.active_cgroups.insert(cgroup_inode, container_id.clone());

                        // Emit an operation to add an alias between the cgroup inode and the container ID.
                        let entity_id = EntityId::ContainerInode(cgroup_inode);
                        let ancestor_entity_id = EntityId::Container(container_id);

                        let operation = MetadataOperation::add_alias(entity_id, ancestor_entity_id);
                        self.operations.push(operation);
                    }
                } else {
                    // If the cgroup has no inode, we can't track it.
                    let container_id = child_cgroup.into_container_id();
                    warn!(%container_id, "Encountered cgroup without controller inode during metadata traversal. This is unexpected.");
                    continue;
                }
            }

            // Figure out which cgroups are no longer active and mark them for deletion.
            for cgroup_inode in self.active_cgroups.keys() {
                if !traversed_cgroups.contains(cgroup_inode) {
                    // This cgroup is no longer present, so we need to delete it.
                    cgroups_to_delete.push(*cgroup_inode);
                }
            }

            // Process the deletions.
            for cgroup_inode in cgroups_to_delete.drain(..) {
                if let Some(container_id) = self.active_cgroups.remove(&cgroup_inode) {
                    debug!(%cgroup_inode, %container_id, "Removing old container-based cgroup.");

                    // Emit a metadata operation to remove the alias between the cgroup inode and the container ID.
                    let entity_id = EntityId::ContainerInode(cgroup_inode);
                    let ancestor_entity_id = EntityId::Container(container_id);

                    let operation = MetadataOperation::remove_alias(entity_id, ancestor_entity_id);
                    self.operations.push(operation);
                } else {
                    warn!(%cgroup_inode, "Tried to remove a cgroup that was not in the active set.");
                }
            }

            let elapsed = start.elapsed();
            debug!(elapsed = ?elapsed, child_cgroups_len, "Traversed cgroups.");

            // Send all collected operations to the channel.
            for operation in self.operations.drain(..) {
                if operations_tx.blocking_send(operation).is_err() {
                    return Err(GenericError::msg("Operations channel unexpectedly closed."));
                }
            }

            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

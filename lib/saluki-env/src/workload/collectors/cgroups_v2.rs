use std::{path::PathBuf, sync::LazyLock, time::Duration};

use async_trait::async_trait;
use cgroupfs::CgroupReader;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::{sync::mpsc, time::sleep};
use tracing::error;

use super::MetadataCollector;
use crate::workload::{entity::EntityId, metadata::MetadataOperation};

/// A metadata collector that observes Linux "Control Groups" (cgroups) v2.
///
/// This collector specifically tracks cgroup controllers attached to container workloads using simple regex-based
/// matching against the controller path, and maintains a mapping of controller inodes to the container ID extracted
/// from the controller path.
///
/// This is specifically used to support client-based Origin Detection in DogStatsD, where clients will either send
/// their detected container ID _or_ the inode of their cgroup controller. A canonical container ID must always be used
/// for origin enrichment, so this mapping allows resolving controller inodes to their canonical container ID.
///
/// ## Missing
///
/// - No support for specifying a custom path prefix when traversing the cgroup hierarchy.
pub struct CGroupsV2MetadataCollector {
    cgroup_reader: CgroupReader,
}

impl CGroupsV2MetadataCollector {
    /// Creates a new `CGroupsV2MetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the cgroups v2 root hierarchy can not be located at the configured path, an error will be returned.
    pub async fn from_configuration(_config: &GenericConfiguration) -> Result<Self, GenericError> {
        // TODO: Logic to handle a a `/host`-mapped variant of the cgroupsv2 hierarchy, etc.
        let cgroup_reader = CgroupReader::new(PathBuf::from("/sys/fs/cgroup"))?;

        Ok(Self { cgroup_reader })
    }
}

#[async_trait]
impl MetadataCollector for CGroupsV2MetadataCollector {
    fn name(&self) -> &'static str {
        "cgroupsv2"
    }

    async fn watch(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut operations = Vec::with_capacity(64);

        // Repeatedly traverse the cgroups v2 hierarchy in a loop, generating ancestry links for controller
        // inode/container ID pairs that we find. We do this using a simple heuristic to determine if the control group
        // name is actually a container ID or something unrelated.
        //
        // We batch these metadata operations and then send them all at the end of the loop.
        loop {
            match traverse_cgroups(&self.cgroup_reader, &mut operations) {
                Ok(()) => {
                    for operation in operations.drain(..) {
                        operations_tx.send(operation).await?;
                    }
                }
                Err(e) => error!(error = %e, "Failed to read cgroups."),
            }

            sleep(Duration::from_secs(2)).await;
        }
    }
}

impl MemoryBounds for CGroupsV2MetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Pre-allocated operation batch buffer. This is only the minimum, as it could grow larger.
            .with_array::<MetadataOperation>(64);
        // TODO: Kind of a throwaway calculation because nothing about the reader can really be bounded at the moment.
        builder.firm().with_fixed_amount(std::mem::size_of::<Self>());
    }
}

fn traverse_cgroups(root: &CgroupReader, operations: &mut Vec<MetadataOperation>) -> Result<(), GenericError> {
    for child_cgroup in root.child_cgroup_iter()? {
        let cgroup_name = child_cgroup.name().as_os_str().to_string_lossy();

        // Check if this control group is associated with a container.
        if let Some(container_id) = extract_container_id(&cgroup_name) {
            // Get the inode for this control group.
            let cgroup_inode = child_cgroup.read_inode_number()?;

            // Create an ancestry link between the container inode and the container ID.
            let entity_id = EntityId::ContainerInode(cgroup_inode);
            let ancestor_entity_id = EntityId::Container(container_id);

            let operation = MetadataOperation::link_ancestor(entity_id, ancestor_entity_id);
            operations.push(operation);
        }

        // After that, traverse the children of this control group.
        traverse_cgroups(&child_cgroup, operations)?;
    }

    Ok(())
}

fn extract_container_id(cgroup_name: &str) -> Option<MetaString> {
    static CONTAINER_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("([0-9a-f]{64})|([0-9a-f]{32}-\\d+)|([0-9a-f]{8}(-[0-9a-f]{4}){4}$)").unwrap());

    match CONTAINER_REGEX.find(cgroup_name) {
        Some(name) => {
            if name.as_str().ends_with(".mount") || name.as_str().starts_with("crio-conmon-") {
                None
            } else {
                Some(MetaString::from(name.as_str()))
            }
        }
        None => None,
    }
}

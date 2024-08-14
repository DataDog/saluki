use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use cgroupfs::CgroupReader;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::{sync::mpsc, time::sleep};
use tracing::error;

use super::MetadataCollector;
use crate::workload::{
    entity::EntityId,
    metadata::MetadataOperation,
};

/// A metadata collector that observes Linux "Control Groups" (cgroups) v2.
pub struct CGroupsV2MetadataCollector {
	cgroup_reader: CgroupReader,
}

impl CGroupsV2MetadataCollector {
    /// Creates a new `CGroupsV2MetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the cgroupsv2 root hierarchy can not be located at the configured path, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration
    ) -> Result<Self, GenericError> {
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

        // Repeatedly traverse the cgroupsv2 hierarchy in a loop, generating ancestry links for controller
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
				},
				Err(e) => error!(error = %e, "Failed to read cgroups."),
			}

			sleep(Duration::from_secs(2)).await;
		}
    }
}

impl MemoryBounds for CGroupsV2MetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the reader can really be bounded at the moment.
        builder.firm().with_fixed_amount(std::mem::size_of::<Self>());
    }
}

fn traverse_cgroups(root: &CgroupReader, operations: &mut Vec<MetadataOperation>) -> Result<(), GenericError> {
	for child_cgroup in root.child_cgroup_iter()? {
		// Check if this control group is associated with a container.
		if let Some(container_id) = extract_container_id_from_cgroup(&child_cgroup) {
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

fn extract_container_id_from_cgroup(child_cgroup: &CgroupReader) -> Option<MetaString> {
	// Get the control group's name and examine it to determine if it's a container ID.
	let cgroup_name = child_cgroup.name().as_os_str().to_string_lossy();

	todo!()
}

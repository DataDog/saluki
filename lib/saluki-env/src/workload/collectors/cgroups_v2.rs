use std::{sync::LazyLock, time::Duration};

use async_trait::async_trait;
use cgroupfs::CgroupReader;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use stringtheory::{interning::FixedSizeInterner, MetaString};
use tokio::{sync::mpsc, time::sleep};
use tracing::{debug, error};

use super::MetadataCollector;
use crate::{
    features::FeatureDetector,
    workload::{entity::EntityId, helpers::cgroups::CGroupsConfiguration, metadata::MetadataOperation},
};

/// A metadata collector that observes Linux "Control Groups" (cgroups) v2.
///
/// This collector specifically tracks cgroup controllers attached to container workloads using simple regex-based
/// matching against the controller path, and maintains a mapping of controller inodes to the container ID extracted
/// from the controller path.
///
/// This is specifically used to support client-based Origin Detection in DogStatsD, where clients will either send
/// their detected container ID _or_ the inode of their cgroup controller. A canonical container ID must always be used
/// for origin enrichment, so this mapping allows resolving controller inodes to their canonical container ID.
pub struct CGroupsV2MetadataCollector {
    reader: CgroupReader,
    interner: FixedSizeInterner<1>,
}

impl CGroupsV2MetadataCollector {
    /// Creates a new `CGroupsV2MetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the cgroups v2 root hierarchy can not be located at the configured path, an error will be returned.
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, interner: FixedSizeInterner<1>,
    ) -> Result<Self, GenericError> {
        let cgroups_config = CGroupsConfiguration::from_configuration(config, feature_detector)?;
        let cgroup_reader = CgroupReader::new(cgroups_config.cgroupfs_path().to_owned())?;

        Ok(Self {
            reader: cgroup_reader,
            interner,
        })
    }
}

#[async_trait]
impl MetadataCollector for CGroupsV2MetadataCollector {
    fn name(&self) -> &'static str {
        "cgroups-v2"
    }

    async fn watch(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Starting cgroups v2 metadata collector.");

        let mut operations = Vec::with_capacity(64);

        // Repeatedly traverse the cgroups v2 hierarchy in a loop, generating ancestry links for controller
        // inode/container ID pairs that we find. We do this using a simple heuristic to determine if the control group
        // name is actually a container ID or something unrelated.
        //
        // We batch these metadata operations and then send them all at the end of the loop.
        loop {
            match traverse_cgroups(&self.reader, &self.interner, &mut operations) {
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

fn traverse_cgroups(
    root: &CgroupReader, interner: &FixedSizeInterner<1>, operations: &mut Vec<MetadataOperation>,
) -> Result<(), GenericError> {
    for child_cgroup in root.child_cgroup_iter()? {
        let cgroup_name = child_cgroup.name().as_os_str().to_string_lossy();

        // Check if this control group is associated with a container.
        if let Some(container_id) = extract_container_id(&cgroup_name, interner) {
            debug!(container_id = %container_id, %cgroup_name, "Found container control group.");

            // Get the inode for this control group.
            let cgroup_inode = child_cgroup.read_inode_number()?;

            // Create an ancestry link between the container inode and the container ID.
            let entity_id = EntityId::ContainerInode(cgroup_inode);
            let ancestor_entity_id = EntityId::Container(container_id);

            let operation = MetadataOperation::link_ancestor(entity_id, ancestor_entity_id);
            operations.push(operation);
        }

        // After that, traverse the children of this control group.
        traverse_cgroups(&child_cgroup, interner, operations)?;
    }

    Ok(())
}

fn extract_container_id(cgroup_name: &str, interner: &FixedSizeInterner<1>) -> Option<MetaString> {
    // This regular expression is meant to capture:
    // - 64 character hexadecimal strings (standard format for container IDs almost everywhere)
    // - 32 character hexadecimal strings followed by a dash and a number (used by AWS ECS)
    // - 8 character hexadecimal strings followed by up to four groups of 4 character hexadecimal strings separated by
    //   dashes (essentially a UUID, used by Pivotal Cloud Foundry's Garden technology)
    static CONTAINER_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("([0-9a-f]{64})|([0-9a-f]{32}-\\d+)|([0-9a-f]{8}(-[0-9a-f]{4}){4}$)").unwrap());

    CONTAINER_REGEX
        .find(cgroup_name)
        .filter(|name| {
            // Filter out any systemd-managed cgroups, as well as CRI-O conmon cgroups, as they don't represent containers.
            !name.as_str().ends_with(".mount") && !name.as_str().starts_with("crio-conmon-")
        })
        .and_then(|name| match interner.try_intern(name.as_str()) {
            Some(interned) => Some(MetaString::from(interned)),
            None => {
                error!(container_id = %name.as_str(), "Failed to intern container ID.");
                None
            }
        })
}

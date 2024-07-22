use std::fmt;

use stringtheory::MetaString;

const ENTITY_PREFIX_POD_UID: &str = "kubernetes_pod_uid://";
const ENTITY_PREFIX_CONTAINER_ID: &str = "container_id://";
const ENTITY_PREFIX_CONTAINER_INODE: &str = "container_inode://";
const ENTITY_PREFIX_CONTAINER_PID: &str = "container_pid://";

/// An entity identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum EntityId {
    /// The global entity.
    ///
    /// Represents the root of the entity hierarchy, which is equivalent to a "global" scope. This is generally used
    /// to represent a collection of metadata entries that are not associated with any specific entity, but with
    /// anything within the workload, such as host or cluster tags.
    Global,

    /// A Kubernetes pod UID.
    ///
    /// Represents the UUID of a specific Kubernetes pod.
    PodUid(MetaString),

    /// A container ID.
    ///
    /// This is generally a long hexadecimal string, as generally used by container runtimes like `containerd`.
    Container(MetaString),

    /// A container inode.
    ///
    /// Represents the inode of the cgroups controller for a specific container.
    ContainerInode(u32),

    /// A container PID.
    ///
    /// Represents the PID of the process within a specific container.
    ContainerPid(u32),
}

impl EntityId {
    /// Creates an `EntityId` from a raw container ID.
    ///
    /// This handles the special case where the "container ID" is actually the inode of the cgroups controller for the
    /// container, and so should be used in scenarios where a raw container "ID" is received that can either be the true
    /// ID or the inode value.
    pub fn from_raw_container_id<S>(raw_container_id: S) -> Option<Self>
    where
        S: AsRef<str> + Into<MetaString>,
    {
        if raw_container_id.as_ref().starts_with("in-") {
            // We have a "container ID" that is actually the inode of the cgroups controller for the container where
            // the metric originated. We treat this separately from true container IDs, which are typically 64 character
            // hexadecimal strings.
            let raw_inode = raw_container_id.as_ref().trim_start_matches("in-");
            let inode = raw_inode.parse().ok()?;
            Some(Self::ContainerInode(inode))
        } else {
            Some(Self::Container(raw_container_id.into()))
        }
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => write!(f, "system://global"),
            Self::PodUid(pod_uid) => write!(f, "{}{}", ENTITY_PREFIX_POD_UID, pod_uid),
            Self::Container(container_id) => write!(f, "{}{}", ENTITY_PREFIX_CONTAINER_ID, container_id),
            Self::ContainerInode(inode) => write!(f, "{}{}", ENTITY_PREFIX_CONTAINER_INODE, inode),
            Self::ContainerPid(pid) => write!(f, "{}{}", ENTITY_PREFIX_CONTAINER_PID, pid),
        }
    }
}

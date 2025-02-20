use std::{cmp::Ordering, fmt};

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
    ContainerInode(u64),

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
    ///
    /// If the raw container ID value starts with "in-", but the remainder is not a valid integer, this will return `None`.
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

    /// Creates an `EntityId` from a Kubernetes pod UID.
    ///
    /// If the pod UID value is "none", this will return `None`.
    pub fn from_pod_uid<S>(pod_uid: S) -> Option<Self>
    where
        S: AsRef<str> + Into<MetaString>,
    {
        if pod_uid.as_ref() == "none" {
            return None;
        }
        Some(Self::PodUid(pod_uid.into()))
    }

    fn precedence_value(&self) -> usize {
        match self {
            Self::Global => 0,
            Self::PodUid(_) => 1,
            Self::Container(_) => 2,
            Self::ContainerInode(_) => 3,
            Self::ContainerPid(_) => 4,
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

impl serde::Serialize for EntityId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // We have this manual implementation of `Serialize` just to avoid needing to bring in `serde_with` to get the
        // helper that utilizes the `Display` implementation.
        serializer.collect_str(self)
    }
}

/// A wrapper for entity IDs that sorts them in a manner consistent with the expected precedence of entity IDs.
///
/// This type establishes a total ordering over entity IDs based on their logical precedence, which is as follows:
///
/// - global (highest precedence)
/// - pod
/// - container
/// - container inode
/// - container PID (lowest precedence)
///
/// Wrapped entity IDs are be sorted highest to lowest precedence. For entity IDs with the same precedence, they are
/// further ordered by their internal value. For entity IDs with a string identifier, lexicographical ordering is used.
/// For entity IDs with a numeric identifier, numerical ordering is used.
#[derive(Eq, PartialEq)]
pub struct HighestPrecedenceEntityIdRef<'a>(&'a EntityId);

impl<'a> From<&'a EntityId> for HighestPrecedenceEntityIdRef<'a> {
    fn from(entity_id: &'a EntityId) -> Self {
        Self(entity_id)
    }
}

impl PartialOrd for HighestPrecedenceEntityIdRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HighestPrecedenceEntityIdRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Do the initial comparison based on the implicit precedence of each entity ID.
        let self_precedence = self.0.precedence_value();
        let other_precedence = other.0.precedence_value();
        if self_precedence != other_precedence {
            return self_precedence.cmp(&other_precedence);
        }

        // We have two entities at the same level of precedence, so we need to compare their actual values.
        match (self.0, other.0) {
            // Global entities are always equal.
            (EntityId::Global, EntityId::Global) => Ordering::Equal,
            (EntityId::PodUid(self_pod_uid), EntityId::PodUid(other_pod_uid)) => self_pod_uid.cmp(other_pod_uid),
            (EntityId::Container(self_container_id), EntityId::Container(other_container_id)) => {
                self_container_id.cmp(other_container_id)
            }
            (EntityId::ContainerInode(self_inode), EntityId::ContainerInode(other_inode)) => {
                self_inode.cmp(other_inode)
            }
            (EntityId::ContainerPid(self_pid), EntityId::ContainerPid(other_pid)) => self_pid.cmp(other_pid),
            _ => unreachable!("entities with different precedence should not be compared"),
        }
    }
}

use std::{cmp::Ordering, fmt};

use stringtheory::MetaString;
use tracing::warn;

const ENTITY_PREFIX_POD_UID: &str = "kubernetes_pod_uid://";
const ENTITY_PREFIX_CONTAINER_ID: &str = "container_id://";
const ENTITY_PREFIX_CONTAINER_INODE: &str = "container_inode://";
const ENTITY_PREFIX_CONTAINER_PID: &str = "container_pid://";

const LOCAL_DATA_PREFIX_INODE: &str = "in-";
const LOCAL_DATA_PREFIX_CID: &str = "ci-";
const LOCAL_DATA_PREFIX_LEGACY_CID: &str = "cid-";

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
    /// Creates an `EntityId` from Local Data.
    ///
    /// This method follows the same logic/behavior as the Datadog Agent's origin detection logic:
    /// - If the input starts with `ci-`, we treat it as a container ID.
    /// - If the input starts with `in-`, we treat it as a container cgroup controller inode.
    /// - If the input contains a comma, we split the input and search for either a prefixed container ID or prefixed
    ///   inode. If both are present, we use the container ID.
    /// - If the input starts with `cid-`, we treat it as a container ID.
    /// - If none of the above conditions are met, we assume the entire input is a container ID.
    ///
    /// If the input fails to be parsed in a valid fashion (e.g., `in-` prefix but the remainder is not a valid
    /// integer), or is empty, `None` is returned.
    pub fn from_local_data<S>(raw_local_data: S) -> Option<Self>
    where
        S: AsRef<str> + Into<MetaString>,
    {
        let local_data_value = raw_local_data.as_ref();
        if local_data_value.is_empty() {
            return None;
        }

        if local_data_value.contains(',') {
            let mut maybe_container_inode = None;
            for local_data_subvalue in local_data_value.split(',') {
                match parse_local_data_value(local_data_subvalue) {
                    // We always prefer the container ID if we get it.
                    Ok(Some(Self::Container(cid))) => return Some(Self::Container(cid)),
                    Ok(Some(Self::ContainerInode(inode))) => maybe_container_inode = Some(inode),
                    Err(()) => {
                        warn!(
                            local_data = local_data_value,
                            local_data_subvalue,
                            "Failed parsing Local Data subvalue. Metric may be missing origin detection-based tags."
                        );
                    }
                    _ => {}
                }
            }

            // Return the container inode if we found one.
            if let Some(inode) = maybe_container_inode {
                return Some(Self::ContainerInode(inode));
            }
        }

        // Try to parse the local data value as a single entity ID value, falling back to treating the entire value as a
        // container ID otherwise.
        match parse_local_data_value(local_data_value) {
            // We always prefer the container ID if we get it.
            Ok(Some(eid)) => Some(eid),
            Ok(None) => Some(Self::Container(raw_local_data.into())),
            Err(()) => {
                warn!(
                    local_data = local_data_value,
                    "Failed parsing Local Data value. Metric may be missing origin detection-based tags."
                );
                None
            }
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

    /// Returns the inner container ID value, if this entity ID is a `Container`.
    ///
    /// Otherwise, `None` is returned and the original entity ID is consumed.
    pub fn try_into_container(self) -> Option<MetaString> {
        match self {
            Self::Container(container_id) => Some(container_id),
            _ => None,
        }
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

fn parse_local_data_value(raw_local_data_value: &str) -> Result<Option<EntityId>, ()> {
    if raw_local_data_value.starts_with(LOCAL_DATA_PREFIX_CID) {
        let cid = raw_local_data_value.trim_start_matches(LOCAL_DATA_PREFIX_CID);
        Ok(Some(EntityId::Container(cid.into())))
    } else if raw_local_data_value.starts_with(LOCAL_DATA_PREFIX_INODE) {
        let inode = raw_local_data_value
            .trim_start_matches(LOCAL_DATA_PREFIX_INODE)
            .parse()
            .map_err(|_| ())?;
        Ok(Some(EntityId::ContainerInode(inode)))
    } else if raw_local_data_value.starts_with(LOCAL_DATA_PREFIX_LEGACY_CID) {
        let cid = raw_local_data_value.trim_start_matches(LOCAL_DATA_PREFIX_LEGACY_CID);
        Ok(Some(EntityId::Container(cid.into())))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_data() {
        const PREFIX_CID: &str = "ci-singlecontainerid";
        const PREFIX_LEGACY_CID: &str = "cid-singlecontainerid";
        const CID: EntityId = EntityId::Container(MetaString::from_static("singlecontainerid"));
        const PREFIX_INODE: &str = "in-12345";
        const INODE: EntityId = EntityId::ContainerInode(12345);

        let cases = [
            // Empty inputs aren't valid.
            ("".into(), None),
            // Invalid container inode values.
            ("in-notanumber".into(), None),
            // Fallback to treat any unparsed value as container ID.
            ("random".into(), Some(EntityId::Container("random".into()))),
            // Single prefixed values.
            (PREFIX_CID.into(), Some(CID.clone())),
            (PREFIX_INODE.into(), Some(INODE.clone())),
            (PREFIX_LEGACY_CID.into(), Some(CID.clone())),
            // Multiple prefixed values, comma separated.
            //
            // We should always prefer container ID over inode. We also test invalid values here since we should
            // ignore them as we iterate over the split values.
            (format!("{},{}", PREFIX_CID, PREFIX_INODE), Some(CID.clone())),
            (format!("{},{}", PREFIX_INODE, PREFIX_CID), Some(CID.clone())),
            (format!("{},{}", PREFIX_LEGACY_CID, PREFIX_INODE), Some(CID.clone())),
            (format!("{},{}", PREFIX_INODE, PREFIX_LEGACY_CID), Some(CID.clone())),
            (format!("{},invalid", PREFIX_CID), Some(CID.clone())),
            (format!("{},invalid", PREFIX_LEGACY_CID), Some(CID.clone())),
            (format!("{},invalid", PREFIX_INODE), Some(INODE.clone())),
        ];

        for (input, expected) in cases {
            let actual = EntityId::from_local_data(input);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn pod_uid_valid() {
        let pod_uid = "abcdef1234567890";
        let entity_id = EntityId::from_pod_uid(pod_uid).unwrap();
        assert_eq!(entity_id, EntityId::PodUid(MetaString::from(pod_uid)));
    }

    #[test]
    fn pod_uid_none() {
        let pod_uid = "none";
        let entity_id = EntityId::from_pod_uid(pod_uid);
        assert!(entity_id.is_none());
    }
}

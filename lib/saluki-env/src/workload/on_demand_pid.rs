use std::sync::Arc;

#[cfg(target_os = "linux")]
use saluki_common::collections::FastConcurrentHashMap;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;
use tracing::{debug, trace};

#[cfg(target_os = "linux")]
use super::helpers::cgroups::{CgroupsConfiguration, CgroupsReader};
use crate::{features::FeatureDetector, workload::EntityId};

enum Inner {
    #[allow(dead_code)]
    Noop,

    #[cfg(target_os = "linux")]
    Linux {
        cgroups_reader: CgroupsReader,
        pid_mappings_cache: FastConcurrentHashMap<u32, EntityId>,
    },
}

impl Inner {
    fn resolve(&self, _process_id: u32) -> Option<EntityId> {
        match self {
            Inner::Noop => None,

            #[cfg(target_os = "linux")]
            Inner::Linux {
                pid_mappings_cache,
                cgroups_reader,
            } => {
                // First, check our PID mapping cache.
                //
                // TODO: This should be an actual cache, with expiration, because PIDs will eventually get recycled so we
                // shouldn't keep results forever, but perhaps most important: this is a slow memory leak generator otherwise.
                //
                // This is simply a stopgap to make sure this functionality, overall, works for the purposes of origin
                // detection.
                if let Some(container_id) = pid_mappings_cache.pin().get(&_process_id).cloned() {
                    trace!(
                        "Resolved PID {} to container ID {} from cache.",
                        _process_id,
                        container_id
                    );
                    return Some(container_id);
                }

                // If we don't have a mapping, query the host OS for it.
                match cgroups_reader.get_cgroup_by_pid(_process_id) {
                    Some(cgroup) => {
                        let container_eid = EntityId::Container(cgroup.into_container_id());

                        debug!("Resolved PID {} to container ID {}.", _process_id, container_eid);

                        pid_mappings_cache.pin().insert(_process_id, container_eid.clone());
                        Some(container_eid)
                    }
                    None => {
                        debug!(
                            "Failed to resolve container ID for PID {}. Process ID may not be part of a container.",
                            _process_id
                        );
                        None
                    }
                }
            }
        }
    }
}

/// A resolver for mapping process IDs to their container IDs based on querying the underlying host.
///
/// # Platform support
///
/// On Linux platforms, PIDs are resolved by querying procfs to find the cgroup of the process, if one exists, the cgroup
/// hierarchy is queried to discover the container ID that owns the process, if possible.
///
/// On all other platforms, `OnDemandPIDResolver` is a no-op and does not perform any resolution.
#[derive(Clone)]
pub struct OnDemandPIDResolver {
    inner: Arc<Inner>,
}

impl OnDemandPIDResolver {
    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            inner: Arc::new(Inner::Noop),
        }
    }

    /// Creates a new `OnDemandPIDResolver` from the given configuration.
    #[cfg(not(target_os = "linux"))]
    pub fn from_configuration(
        _config: &GenericConfiguration, _feature_detector: FeatureDetector, _interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        // On non-Linux platforms, we don't need to do anything special.
        Ok(Self {
            inner: Arc::new(Inner::Noop),
        })
    }

    #[cfg(target_os = "linux")]
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let cgroups_config = CgroupsConfiguration::from_configuration(config, feature_detector)?;
        let cgroups_reader = match CgroupsReader::try_from_config(&cgroups_config, interner)? {
            Some(reader) => reader,
            None => {
                return Err(generic_error!("Failed to detect any cgroups v1/v2 hierarchy. "));
            }
        };

        Ok(Self {
            inner: Arc::new(Inner::Linux {
                cgroups_reader,
                pid_mappings_cache: FastConcurrentHashMap::default(),
            }),
        })
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID is not part of a container, or cannot be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        self.inner.resolve(process_id)
    }
}

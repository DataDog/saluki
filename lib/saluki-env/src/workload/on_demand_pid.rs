use std::sync::Arc;

use saluki_common::collections::FastConcurrentHashMap;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;
use tracing::{debug, trace};

use super::helpers::cgroups::{CgroupsConfiguration, CgroupsReader};
use crate::{features::FeatureDetector, workload::EntityId};

struct Inner {
    cgroups_reader: CgroupsReader,
    pid_mappings_cache: FastConcurrentHashMap<u32, EntityId>,
}

/// A handle for resolving container IDs from process IDs on demand by querying the host OS.
#[derive(Clone)]
pub struct OnDemandPIDResolver {
    inner: Arc<Inner>,
}

impl OnDemandPIDResolver {
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
            inner: Arc::new(Inner {
                cgroups_reader,
                pid_mappings_cache: FastConcurrentHashMap::default(),
            }),
        })
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID is not part of a container, or cannot be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        // First, check our PID mapping map.
        //
        // TODO: This should really be a cache, because PIDs will eventually get recycled so we shouldn't keep results
        // forever, but perhaps most important: this is a slow memory leak generator otherwise.
        //
        // This is simply a stopgap to make sure this functionality, overall, works for the purposes of origin detection.
        if let Some(container_id) = self.inner.pid_mappings_cache.pin().get(&process_id) {
            trace!(
                "Resolved PID {} to container ID {} from cache.",
                process_id,
                container_id
            );
            return Some(container_id.clone());
        }

        // If we don't have a mapping, query the host OS for it.
        match self.inner.cgroups_reader.get_cgroup_by_pid(process_id) {
            Some(cgroup) => {
                let container_eid = EntityId::Container(cgroup.into_container_id());

                debug!("Resolved PID {} to container ID {}.", process_id, container_eid);

                self.inner
                    .pid_mappings_cache
                    .pin()
                    .insert(process_id, container_eid.clone());
                Some(container_eid)
            }
            None => {
                debug!(
                    "Failed to resolve container ID for PID {}. Process ID may not be part of a container.",
                    process_id
                );
                None
            }
        }
    }
}

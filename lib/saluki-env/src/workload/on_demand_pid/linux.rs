use std::{num::NonZeroUsize, time::Duration};

use saluki_common::cache::{Cache, CacheBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_metrics::{static_metrics, Gauge};
use stringtheory::interning::{GenericMapInterner, Interner as _};
use tokio::time::sleep;
use tracing::{debug, trace};

use crate::workload::helpers::cgroups::{get_self_container_id, CgroupsConfiguration, CgroupsReader};
use crate::{features::FeatureDetector, workload::EntityId};

#[static_metrics(prefix = pid_resolver)]
#[derive(Clone)]
struct Telemetry {
    interner_capacity_bytes: Gauge,
    interner_len_bytes: Gauge,
    interner_entries: Gauge,
}

type PIDCache = Cache<u32, EntityId>;
const DEFAULT_PID_CACHE_CACHED_PIDS_LIMIT: usize = 500_000;
const DEFAULT_PID_CACHE_IDLE_PID_EXPIRATION: Duration = Duration::from_secs(30);

pub struct ResolverImpl {
    cgroups_reader: CgroupsReader,
    interner: GenericMapInterner,
    pid_mappings_cache: PIDCache,
}

impl ResolverImpl {
    /// Creates a new `ResolverImpl` from the given configuration.
    ///
    /// # Errors
    ///
    /// If a cgroups hierarchy can't be found, or the internal cache can't be created, an error is returned.
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let telemetry = Telemetry::new();
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        let cgroups_config = CgroupsConfiguration::from_configuration(config, feature_detector)?;
        let cgroups_reader = match CgroupsReader::try_from_config(&cgroups_config, interner.clone())? {
            Some(reader) => reader,
            None => {
                return Err(GenericError::msg("Failed to detect any cgroups v1/v2 hierarchy."));
            }
        };

        let cache_builder = CacheBuilder::from_identifier("on_demand_pid_resolver")?
            .with_capacity(NonZeroUsize::new(DEFAULT_PID_CACHE_CACHED_PIDS_LIMIT).unwrap())
            .with_time_to_idle(Some(DEFAULT_PID_CACHE_IDLE_PID_EXPIRATION));

        tokio::spawn(drive_telemetry(interner.clone(), telemetry.clone()));

        Ok(Self {
            cgroups_reader,
            interner,
            pid_mappings_cache: cache_builder.build(),
        })
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID isn't part of a container, or can't be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        // First, check our PID mapping cache.
        if let Some(container_id) = self.pid_mappings_cache.get(&process_id) {
            trace!(
                "Resolved PID {} to container ID {} from cache.",
                process_id,
                container_id
            );
            return Some(container_id);
        }

        // If we don't have a mapping, query the host OS for it.
        match self.cgroups_reader.get_cgroup_by_pid(process_id) {
            Some(cgroup) => {
                let container_eid = EntityId::Container(cgroup.into_container_id());

                debug!("Resolved PID {} to container ID {}.", process_id, container_eid);

                self.pid_mappings_cache.insert(process_id, container_eid.clone());
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

    /// Resolves the current process's container entity from local cgroup membership.
    pub fn resolve_self_container(&self) -> Option<EntityId> {
        get_self_container_id(&self.interner).map(EntityId::Container)
    }
}

async fn drive_telemetry(interner: GenericMapInterner, telemetry: Telemetry) {
    loop {
        sleep(Duration::from_secs(1)).await;

        telemetry.interner_entries().set(interner.len() as f64);
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);
        telemetry.interner_len_bytes().set(interner.len_bytes() as f64);
    }
}

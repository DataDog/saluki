#[cfg(target_os = "linux")]
use std::num::NonZeroUsize;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use saluki_common::cache::{Cache, CacheBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_metrics::static_metrics;
use stringtheory::interning::GenericMapInterner;
#[cfg(target_os = "linux")]
use tokio::time::sleep;
#[cfg(target_os = "linux")]
use tracing::{debug, trace};

#[cfg(target_os = "linux")]
use super::helpers::cgroups::{CgroupsConfiguration, CgroupsReader};
use crate::{features::FeatureDetector, workload::EntityId};

static_metrics! {
    name => Telemetry,
    prefix => pid_resolver,
    metrics => [
        gauge(interner_capacity_bytes),
        gauge(interner_len_bytes),
        gauge(interner_entries),
    ],
}

#[cfg(target_os = "linux")]
type PIDCache = Cache<u32, EntityId>;
#[cfg(target_os = "linux")]
const DEFAULT_PID_CACHE_CACHED_PIDS_LIMIT: usize = 500_000;
#[cfg(target_os = "linux")]
const DEFAULT_PID_CACHE_IDLE_PID_EXPIRATION: Duration = Duration::from_secs(30);

#[allow(clippy::large_enum_variant)]
enum Inner {
    #[allow(dead_code)]
    Noop,

    #[cfg(target_os = "linux")]
    Linux {
        cgroups_reader: CgroupsReader,
        pid_mappings_cache: PIDCache,
    },
}

impl Inner {
    fn resolve(&self, process_id: u32) -> Option<EntityId> {
        match self {
            Inner::Noop => resolve_noop_pid(process_id),

            #[cfg(target_os = "linux")]
            Inner::Linux {
                pid_mappings_cache,
                cgroups_reader,
            } => resolve_linux_pid(process_id, pid_mappings_cache, cgroups_reader),
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

        let inner = Arc::new(Inner::Linux {
            cgroups_reader,
            pid_mappings_cache: cache_builder.build(),
        });

        tokio::spawn(drive_telemetry(interner.clone(), telemetry.clone()));

        Ok(Self { inner })
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID is not part of a container, or cannot be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        self.inner.resolve(process_id)
    }
}

#[cfg(target_os = "linux")]
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

fn resolve_noop_pid(_process_id: u32) -> Option<EntityId> {
    // No-op resolver, always returns None.
    None
}

#[cfg(target_os = "linux")]
fn resolve_linux_pid(
    process_id: u32, pid_mappings_cache: &PIDCache, cgroups_reader: &CgroupsReader,
) -> Option<EntityId> {
    // First, check our PID mapping cache.
    //
    // TODO: This should be an actual cache, with expiration, because PIDs will eventually get recycled so we
    // shouldn't keep results forever, but perhaps most important: this is a slow memory leak generator otherwise.
    //
    // This is simply a stopgap to make sure this functionality, overall, works for the purposes of origin
    // detection.
    if let Some(container_id) = pid_mappings_cache.get(&process_id) {
        trace!(
            "Resolved PID {} to container ID {} from cache.",
            process_id,
            container_id
        );
        return Some(container_id);
    }

    // If we don't have a mapping, query the host OS for it.
    match cgroups_reader.get_cgroup_by_pid(process_id) {
        Some(cgroup) => {
            let container_eid = EntityId::Container(cgroup.into_container_id());

            debug!("Resolved PID {} to container ID {}.", process_id, container_eid);

            pid_mappings_cache.insert(process_id, container_eid.clone());
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

use std::{num::NonZeroUsize, time::Duration};

use async_trait::async_trait;
use saluki_common::{
    cache::{Cache, CacheBuilder, CacheWorker},
    supervision::{InitializationError, Supervisable, SupervisorFuture},
    sync::shutdown::ShutdownHandle,
};
use saluki_error::GenericError;
use saluki_metrics::{static_metrics, Gauge};
use stringtheory::interning::{GenericMapInterner, Interner as _};
use tokio::{select, time::sleep};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, trace};

use crate::workload::helpers::cgroups::{get_self_container_id, CgroupsConfiguration, CgroupsReader};
use crate::workload::EntityId;

#[static_metrics(prefix = pid_resolver)]
#[derive(Clone)]
struct Telemetry {
    interner_capacity_bytes: Gauge,
    interner_len_bytes: Gauge,
    interner_entries: Gauge,
}

type PIDCache = Cache<u32, Option<EntityId>>;
const DEFAULT_PID_CACHE_CACHED_PIDS_LIMIT: usize = 500_000;
const DEFAULT_PID_CACHE_IDLE_PID_EXPIRATION: Duration = Duration::from_secs(30);

pub struct ResolverImpl {
    cgroups_reader: CgroupsReader,
    interner: GenericMapInterner,
    pid_mappings_cache: PIDCache,

    /// Drop guard for the interner telemetry loop.
    ///
    /// The loop holds clones of the interner and the telemetry handles, so it has to stop when this resolver is
    /// dropped rather than only when the process exits: the interner eagerly allocates its full configured capacity
    /// and only frees it once the last clone goes away, and live telemetry handles block idle eviction of the metrics
    /// they report. This is independent of supervisor shutdown, which the worker handles separately.
    _telemetry_shutdown: DropGuard,
}

impl ResolverImpl {
    /// Creates a new `ResolverImpl` from the given cgroups configuration.
    ///
    /// # Errors
    ///
    /// If a cgroups hierarchy can't be found, or the internal cache can't be created, an error is returned.
    pub fn new(
        cgroups_config: &CgroupsConfiguration, interner: GenericMapInterner,
    ) -> Result<(Self, ResolverWorker), GenericError> {
        let telemetry = Telemetry::new();
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        let cgroups_reader = match CgroupsReader::try_from_config(cgroups_config, interner.clone())? {
            Some(reader) => reader,
            None => {
                return Err(GenericError::msg("Failed to detect any cgroups v1/v2 hierarchy."));
            }
        };

        let (pid_mappings_cache, cache_worker) = CacheBuilder::from_identifier("on_demand_pid_resolver")?
            .with_capacity(NonZeroUsize::new(DEFAULT_PID_CACHE_CACHED_PIDS_LIMIT).unwrap())
            .with_time_to_idle(Some(DEFAULT_PID_CACHE_IDLE_PID_EXPIRATION))
            .build();

        // A token clone costs the worker nothing and keeps nothing alive; the guard held on the resolver is what
        // actually fires, when the resolver is dropped.
        let telemetry_shutdown = CancellationToken::new();
        let worker = ResolverWorker {
            cache: cache_worker,
            interner: interner.clone(),
            telemetry: telemetry.clone(),
            telemetry_shutdown: telemetry_shutdown.clone(),
        };

        let resolver = Self {
            cgroups_reader,
            interner,
            pid_mappings_cache,
            _telemetry_shutdown: telemetry_shutdown.drop_guard(),
        };

        Ok((resolver, worker))
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID isn't part of a container, or can't be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        // First, check our PID mapping cache.
        if let Some(container_id) = self.pid_mappings_cache.get(&process_id) {
            match &container_id {
                Some(container_id) => {
                    trace!(
                        "Resolved PID {} to container ID {} from cache.",
                        process_id,
                        container_id
                    );
                }
                None => trace!("Found cached negative container ID lookup for PID {}.", process_id),
            }
            return container_id;
        }

        // If we don't have a mapping, query the host OS for it.
        match self.cgroups_reader.get_cgroup_by_pid(process_id) {
            Some(cgroup) => {
                let container_eid = EntityId::Container(cgroup.into_container_id());

                debug!("Resolved PID {} to container ID {}.", process_id, container_eid);

                self.pid_mappings_cache.insert(process_id, Some(container_eid.clone()));
                Some(container_eid)
            }
            None => {
                debug!(
                    "Failed to resolve container ID for PID {}. Process ID may not be part of a container.",
                    process_id
                );
                self.pid_mappings_cache.insert(process_id, None);
                None
            }
        }
    }

    /// Resolves the current process's container entity from local cgroup membership.
    pub fn resolve_self_container(&self) -> Option<EntityId> {
        get_self_container_id(&self.interner).map(EntityId::Container)
    }
}

/// A worker that drives a [`ResolverImpl`]'s background work.
///
/// Expires idle PID mappings and reports interner utilization. Neither happens while this isn't running. Add it to a
/// supervisor as a transient child: both loops also stop when the resolver they serve is dropped, which is a clean
/// exit that a permanent child would restart into a loop.
pub struct ResolverWorker {
    cache: CacheWorker<u32, Option<EntityId>>,
    interner: GenericMapInterner,
    telemetry: Telemetry,
    telemetry_shutdown: CancellationToken,
}

#[async_trait]
impl Supervisable for ResolverWorker {
    fn name(&self) -> &str {
        "on_demand_pid_resolver"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // Rebuilt from cloned handles on every call, which is what makes this restartable.
        let cache = self.cache.initialize(ShutdownHandle::noop()).await?;
        let telemetry = drive_telemetry(
            self.interner.clone(),
            self.telemetry.clone(),
            self.telemetry_shutdown.clone(),
        );

        Ok(Box::pin(async move {
            let drive = async {
                let (cache, ()) = tokio::join!(cache, telemetry);
                cache
            };

            select! {
                _ = process_shutdown => Ok(()),
                result = drive => result,
            }
        }))
    }
}

async fn drive_telemetry(interner: GenericMapInterner, telemetry: Telemetry, shutdown: CancellationToken) {
    let report_telemetry = async {
        loop {
            sleep(Duration::from_secs(1)).await;

            telemetry.interner_entries().set(interner.len() as f64);
            telemetry
                .interner_capacity_bytes()
                .set(interner.capacity_bytes() as f64);
            telemetry.interner_len_bytes().set(interner.len_bytes() as f64);
        }
    };

    select! {
        _ = shutdown.cancelled() => {},
        _ = report_telemetry => {},
    }
}

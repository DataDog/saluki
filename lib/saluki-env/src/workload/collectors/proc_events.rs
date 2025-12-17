//! Process events metadata collector.
//!
//! This collector uses the Linux netlink process connector to receive process lifecycle events
//! and maps containerized processes to their container IDs via cgroup inspection.

use async_trait::async_trait;
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use neli::connector::ProcEvent;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_health::Health;
use saluki_metrics::static_metrics;
use stringtheory::interning::GenericMapInterner;
use tokio::{select, sync::mpsc};
use tracing::{debug, error, trace, warn};

use super::MetadataCollector;
use crate::{
    features::FeatureDetector,
    workload::{
        entity::EntityId,
        helpers::{
            cgroups::{CgroupsConfiguration, CgroupsReader},
            proc_events::ProcEventsClient,
        },
        metadata::MetadataOperation,
    },
};

static_metrics!(
    name => Telemetry,
    prefix => proc_events_metadata_collector,
    metrics => [
        counter(events_exec_total),
        counter(events_exit_total),
        counter(events_exec_containerized_total),
        counter(events_exec_non_containerized_total),
        counter(socket_errors_total),
    ],
);

/// Default limit for tracking container PIDs.
const DEFAULT_TRACKED_PIDS_LIMIT: usize = 100_000;

/// A metadata collector that watches for process events via netlink.
///
/// This collector receives process exec and exit events from the Linux kernel via the netlink
/// process connector interface. For each exec event, it queries the process's cgroup to determine
/// if it belongs to a container, and if so, emits an alias operation mapping the PID to the
/// container ID.
///
/// This is particularly useful for scenarios where container runtime-specific collectors
/// (like containerd) are not available, or as a fallback mechanism for detecting containerized
/// processes.
///
/// ## Requirements
///
/// - Linux kernel with CONFIG_PROC_EVENTS enabled
/// - CAP_NET_ADMIN capability or root privileges
/// - A valid cgroups v1 or v2 hierarchy
pub struct ProcEventsMetadataCollector {
    cgroups_reader: CgroupsReader,
    tracked_pids_limit: usize,
    health: Health,
    telemetry: Telemetry,
}

impl ProcEventsMetadataCollector {
    /// Creates a new `ProcEventsMetadataCollector` from the given configuration.
    ///
    /// # Errors
    ///
    /// If a valid cgroups hierarchy cannot be located, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let cgroups_config = CgroupsConfiguration::from_configuration(config, feature_detector)?;
        let cgroups_reader = match CgroupsReader::try_from_config(&cgroups_config, interner)? {
            Some(reader) => reader,
            None => {
                return Err(GenericError::msg(
                    "Failed to detect any cgroups v1/v2 hierarchy for proc events collector.",
                ));
            }
        };

        let tracked_pids_limit = config
            .try_get_typed::<usize>("proc_events_tracked_pids_limit")?
            .unwrap_or(DEFAULT_TRACKED_PIDS_LIMIT);

        Ok(Self {
            cgroups_reader,
            tracked_pids_limit,
            health,
            telemetry: Telemetry::new(),
        })
    }
}

#[async_trait]
impl MetadataCollector for ProcEventsMetadataCollector {
    fn name(&self) -> &'static str {
        "proc_events"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        // Create the process events client
        let client = match ProcEventsClient::new().await {
            Ok(client) => client,
            Err(e) => {
                return Err(GenericError::msg(format!(
                    "Failed to create process events client: {}",
                    e
                )));
            }
        };

        self.health.mark_ready();

        // Track which PIDs we've emitted aliases for, so we know which to clean up on exit
        let mut tracked_pids =
            saluki_common::collections::FastHashMap::with_capacity_and_hasher(1024, Default::default());

        let mut event_stream = client.into_event_stream();

        loop {
            select! {
                _ = self.health.live() => {},
                maybe_event = event_stream.next() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            if let Some(op) = self.process_event(
                                event,
                                &mut tracked_pids,
                            ) {
                                if operations_tx.send(op).await.is_err() {
                                    // Channel closed, exit gracefully
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            self.telemetry.socket_errors_total().increment(1);
                            error!(error = %e, "Error receiving process event");
                            // Continue trying to receive more events
                        }
                        None => {
                            // Stream ended
                            debug!("Process events stream ended.");
                            break;
                        }
                    }
                }
            }
        }

        self.health.mark_not_ready();
        Ok(())
    }
}

impl ProcEventsMetadataCollector {
    fn process_event(
        &self, event: ProcEvent, tracked_pids: &mut saluki_common::collections::FastHashMap<u32, ()>,
    ) -> Option<MetadataOperation> {
        match event {
            ProcEvent::Exec {
                process_pid,
                process_tgid,
            } => {
                self.telemetry.events_exec_total().increment(1);

                // Only track the thread group leader (main thread)
                if process_pid != process_tgid {
                    trace!(
                        pid = process_pid,
                        tgid = process_tgid,
                        "Skipping non-leader thread exec event"
                    );
                    return None;
                }

                // Check if we're at capacity
                if tracked_pids.len() >= self.tracked_pids_limit {
                    warn!(
                        limit = self.tracked_pids_limit,
                        "Tracked PIDs limit reached, skipping new process"
                    );
                    return None;
                }

                // Query cgroup to see if this process is in a container
                match self.cgroups_reader.get_cgroup_by_pid(process_pid as u32) {
                    Some(cgroup) => {
                        let container_id = cgroup.into_container_id();
                        debug!(pid = process_pid, %container_id, "Process is containerized, creating alias");

                        self.telemetry.events_exec_containerized_total().increment(1);
                        tracked_pids.insert(process_pid as u32, ());

                        let pid_entity = EntityId::ContainerPid(process_pid as u32);
                        let container_entity = EntityId::Container(container_id);
                        Some(MetadataOperation::add_alias(pid_entity, container_entity))
                    }
                    None => {
                        // Process is not in a container (or we couldn't determine)
                        trace!(
                            pid = process_pid,
                            "Process is not containerized or cgroup lookup failed"
                        );
                        self.telemetry.events_exec_non_containerized_total().increment(1);
                        None
                    }
                }
            }
            ProcEvent::Exit {
                process_pid,
                process_tgid,
                ..
            } => {
                self.telemetry.events_exit_total().increment(1);

                // Only handle the thread group leader
                if process_pid != process_tgid {
                    return None;
                }

                // Only emit delete if we were tracking this PID
                if tracked_pids.remove(&(process_pid as u32)).is_some() {
                    debug!(pid = process_pid, "Tracked container process exited, deleting entity");
                    Some(MetadataOperation::delete(EntityId::ContainerPid(process_pid as u32)))
                } else {
                    None
                }
            }
            // Ignore other event types (Fork, Uid, Gid, etc.)
            _ => None,
        }
    }
}

impl MemoryBounds for ProcEventsMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .firm()
            .with_fixed_amount("self struct", std::mem::size_of::<Self>())
            // Tracked PIDs map - worst case all slots filled
            // Each entry is u32 key + () value + HashMap overhead (~48 bytes per entry)
            .with_fixed_amount("tracked_pids", self.tracked_pids_limit * 48);
    }
}

//! Memory management.

use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::Duration,
};

use bytesize::ByteSize;
use memory_accounting::{
    allocator::{AllocationGroupRegistry, AllocationStats, AllocationStatsSnapshot},
    ComponentRegistry, MemoryGrant, MemoryLimiter, VerifiedBounds,
};
use metrics::{counter, gauge, Counter, Gauge};
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use tokio::time::sleep;
use tracing::{info, warn};

const fn default_memory_slop_factor() -> f64 {
    0.25
}

const fn default_enable_global_limiter() -> bool {
    true
}

/// Configuration for memory bounds.
#[derive(Deserialize)]
pub struct MemoryBoundsConfiguration {
    /// The memory limit to adhere to.
    ///
    /// This should be the overall memory limit for the entire process. The value can either be an integer for
    /// specifying the limit in bytes, or a string that uses SI byte prefixes (case-insensitive) such as `1mb` or `1GB`.
    ///
    /// If not specified, no memory bounds verification will be performed.
    #[serde(default)]
    memory_limit: Option<ByteSize>,

    /// The slop factor to apply to the given memory limit.
    ///
    /// Memory bounds are inherently fuzzy, as components are required to manually define their bounds, and as such, can
    /// only account for memory usage that they know about. The slop factor is applied as a reduction to the overall
    /// memory limit, such that we account for the "known unknowns" -- memory that hasn't yet been accounted for -- by
    /// simply ensuring that we can fit within a portion of the overall limit.
    ///
    /// Values between 0 to 1 are allowed, and represent the percentage of `memory_limit` that is held back. This means
    /// that a slop factor of 0.25, for example, will cause 25% of `memory_limit` to be withheld. If `memory_limit` was
    /// 100MB, we would then verify that the memory bounds can fit within 75MB (100MB * (1 - 0.25) => 75MB).
    #[serde(default = "default_memory_slop_factor")]
    memory_slop_factor: f64,

    /// Whether or not to enable the global memory limiter.
    ///
    /// When set to `false`, the global memory limiter will operate in a no-op mode. All calls to use it will never exert
    /// backpressure, and only the inherent memory bounds of the running components will influence memory usage.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_global_limiter")]
    enable_global_limiter: bool,
}

impl MemoryBoundsConfiguration {
    /// Attempts to read memory bounds configuration from the provided configuration.
    ///
    /// ## Errors
    ///
    /// If an error occurs during deserialization, an error will be returned.
    pub fn try_from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut config = config
            .as_typed::<Self>()
            .error_context("Failed to parse memory bounds configuration.")?;

        if config.memory_limit.is_none() {
            // Try to pull configured memory limit from Cgroup if running in a containerized environment.
            if let Ok(value) = env::var("DOCKER_DD_AGENT") {
                if !value.is_empty() {
                    let cgroup_memory_reader = CgroupMemoryParser;
                    if let Some(memory) = cgroup_memory_reader.parse() {
                        config.memory_limit = Some(memory);
                    }
                }
            }
        }

        // Try constructing the initial grant based on the configuration as a smoke test to validate the values.
        if let Some(limit) = config.memory_limit {
            let _ = MemoryGrant::with_slop_factor(limit.as_u64() as usize, config.memory_slop_factor)
                .error_context("Given memory limit and/or slop factor invalid.")?;
        }

        Ok(config)
    }

    /// Gets the initial memory grant based on the configuration.
    pub fn get_initial_grant(&self) -> Option<MemoryGrant> {
        self.memory_limit.map(|limit| {
            MemoryGrant::with_slop_factor(limit.as_u64() as usize, self.memory_slop_factor)
                .expect("memory limit should be valid")
        })
    }
}

/// Initializes the memory bounds system and verifies any configured bounds.
///
/// If no memory limit is configured, or if the populated memory bounds fit within the configured memory limit,
/// `Ok(MemoryLimiter)` is returned. The memory limiter can be used as a global limiter for the process, allowing
/// callers to cooperatively participate in staying within the configured memory bounds by blocking when used memory
/// exceeds the configured limit, until it returns below the limit. The limiter uses the effective memory limit, based
/// on the configured slop factor.
///
/// ## Errors
///
/// If the bounds could not be validated, an error is returned.
pub fn initialize_memory_bounds(
    configuration: MemoryBoundsConfiguration, mut component_registry: ComponentRegistry,
) -> Result<MemoryLimiter, GenericError> {
    let initial_grant = match configuration.memory_limit {
        Some(limit) => MemoryGrant::with_slop_factor(limit.as_u64() as usize, configuration.memory_slop_factor)?,
        None => {
            info!("No memory limit set for the process. Skipping memory bounds verification.");
            return Ok(MemoryLimiter::noop());
        }
    };

    let verified_bounds = component_registry.verify_bounds(initial_grant)?;

    let limiter = configuration
        .enable_global_limiter
        .then(|| {
            MemoryLimiter::new(initial_grant)
                .ok_or_else(|| generic_error!("Memory statistics cannot be gathered on this system."))
        })
        .unwrap_or_else(|| Ok(MemoryLimiter::noop()))?;

    info!(
		"Verified memory bounds. Minimum memory requirement of {}, with a calculated firm memory bound of {} out of {} available, from an initial {} grant.",
		bytesize::to_string(verified_bounds.total_minimum_required_bytes() as u64, true),
		bytesize::to_string(verified_bounds.total_firm_limit_bytes() as u64, true),
		bytesize::to_string(verified_bounds.total_available_bytes() as u64, true),
		bytesize::to_string(initial_grant.initial_limit_bytes() as u64, true),
	);

    print_verified_bounds(verified_bounds);

    Ok(limiter)
}

fn print_verified_bounds(bounds: VerifiedBounds) {
    info!("Breakdown of verified bounds:");
    info!(
        "- (root): {} minimum, {} firm",
        bytesize::to_string(bounds.bounds().total_minimum_required_bytes() as u64, true),
        bytesize::to_string(bounds.bounds().total_firm_limit_bytes() as u64, true),
    );

    let mut to_visit = VecDeque::new();
    to_visit.extend(
        bounds
            .bounds()
            .subcomponents()
            .into_iter()
            .map(|(name, bounds)| (1, name, bounds)),
    );

    while let Some((depth, component_name, component_bounds)) = to_visit.pop_front() {
        info!(
            "{:indent$}- {}: {} minimum, {} firm",
            "",
            component_name,
            bytesize::to_string(component_bounds.total_minimum_required_bytes() as u64, true),
            bytesize::to_string(component_bounds.total_firm_limit_bytes() as u64, true),
            indent = depth * 2
        );

        let mut subcomponents = component_bounds.subcomponents().into_iter().collect::<Vec<_>>();
        while let Some((subcomponent_name, subcomponent_bounds)) = subcomponents.pop() {
            to_visit.push_front((depth + 1, subcomponent_name, subcomponent_bounds));
        }
    }

    info!("");
}

struct AllocationGroupMetrics {
    totals: AllocationStatsSnapshot,
    allocated_bytes_total: Counter,
    allocated_bytes_live: Gauge,
    allocated_objects_total: Counter,
    allocated_objects_live: Gauge,
    deallocated_bytes_total: Counter,
    deallocated_objects_total: Counter,
}

impl AllocationGroupMetrics {
    fn new(group_name: &str) -> Self {
        Self {
            totals: AllocationStatsSnapshot::empty(),
            allocated_bytes_total: counter!("group_allocated_bytes_total", "group_id" => group_name.to_string()),
            allocated_bytes_live: gauge!("group_allocated_bytes_live", "group_id" => group_name.to_string()),
            allocated_objects_total: counter!("group_allocated_objects_total", "group_id" => group_name.to_string()),
            allocated_objects_live: gauge!("group_allocated_objects_live", "group_id" => group_name.to_string()),
            deallocated_bytes_total: counter!("group_deallocated_bytes_total", "group_id" => group_name.to_string()),
            deallocated_objects_total: counter!("group_deallocated_objects_total", "group_id" => group_name.to_string()),
        }
    }

    fn update(&mut self, stats: &AllocationStats) {
        let delta = stats.snapshot_delta(&self.totals);

        self.allocated_bytes_total.increment(delta.allocated_bytes as u64);
        self.allocated_objects_total.increment(delta.allocated_objects as u64);
        self.deallocated_bytes_total.increment(delta.deallocated_bytes as u64);
        self.deallocated_objects_total
            .increment(delta.deallocated_objects as u64);

        self.totals.merge(&delta);
        self.allocated_bytes_live
            .set((self.totals.allocated_bytes - self.totals.deallocated_bytes) as f64);
        self.allocated_objects_live
            .set((self.totals.allocated_objects - self.totals.deallocated_objects) as f64);
    }
}

/// Initializes the memory allocator telemetry subsystem.
///
/// This spawns a background task that will periodically collect memory usage statistics, such as which components are
/// responsible for which portion of the live heap, and report them as internal telemetry.
///
/// ## Errors
///
/// If the memory allocator subsystem has already been initialized, an error will be returned.
pub async fn initialize_allocator_telemetry() -> Result<(), GenericError> {
    // Simple initialization guard to prevent multiple calls to this function.
    static INIT: AtomicBool = AtomicBool::new(false);
    if INIT.swap(true, Relaxed) {
        return Err(generic_error!("Memory allocator subsystem already initialized."));
    }

    // We can't enforce, at compile-time, that the tracking allocator must be installed if a caller is trying to
    // initialize the allocator's reporting infrastructure... but we can at least warn them if we detect it's not
    // installed here at runtime.
    if !AllocationGroupRegistry::allocator_installed() {
        warn!("Tracking allocator not installed. Memory telemetry will not be available.");
    }

    // Spawn the background task that will periodically collect memory usage statistics.
    tokio::spawn(async {
        let mut metrics = HashMap::new();

        loop {
            sleep(Duration::from_secs(1)).await;

            AllocationGroupRegistry::global().visit_allocation_groups(|group_name, stats| {
                let group_metrics = match metrics.get_mut(group_name) {
                    Some(group_metrics) => group_metrics,
                    None => metrics
                        .entry(group_name.to_string())
                        .or_insert_with(|| AllocationGroupMetrics::new(group_name)),
                };

                group_metrics.update(stats);
            });
        }
    });

    Ok(())
}

struct CgroupMemoryParser;

impl CgroupMemoryParser {
    /// Parse memory limit from memory controller.
    ///
    /// `parse_controller_v2` is called if a unified controller is found in /proc/self/cgroup.
    /// `parse_controller_v1` is called on the controller with ":memory:" present.
    ///
    /// Returns `None` if memory limit is set to max or if an error is encountered while parsing.
    fn parse(self) -> Option<ByteSize> {
        let contents = fs::read_to_string("/proc/self/cgroup").ok()?;
        let parts: Vec<&str> = contents.trim().split("\n").collect();
        // CgroupV2 has unified controllers.
        if parts.len() == 1 {
            return self.parse_controller_v2(parts[0]);
        }
        for line in parts {
            if line.contains(":memory:") {
                return self.parse_controller_v1(line);
            }
        }
        None
    }

    fn parse_controller_v1(self, controller: &str) -> Option<ByteSize> {
        let path = controller.split(":").nth(2)?;
        let memory_path = format!("/sys/fs/cgroup/memory{}/memory.limit_in_bytes", path);
        let raw_memory_limit = fs::read_to_string(memory_path).ok()?;
        self.convert_to_bytesize(&raw_memory_limit)
    }

    fn parse_controller_v2(self, controller: &str) -> Option<ByteSize> {
        let path = controller.split(":").nth(2)?;
        let memory_path = format!("/sys/fs/cgroup{}/memory.max", path);
        let raw_memory_limit = fs::read_to_string(memory_path).ok()?;
        self.convert_to_bytesize(&raw_memory_limit)
    }

    fn convert_to_bytesize(self, s: &str) -> Option<ByteSize> {
        let memory = s.trim().to_string();
        if memory == "max" {
            return None;
        }
        memory.parse::<ByteSize>().ok()
    }
}

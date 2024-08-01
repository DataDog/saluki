//! Memory management.

use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::Duration,
};

use bytesize::ByteSize;
use memory_accounting::{
    allocator::{AllocationStats, AllocationStatsDelta, ComponentRegistry},
    BoundsVerifier, MemoryBoundsBuilder, MemoryGrant, MemoryLimiter, VerifiedBounds,
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
        config
            .as_typed::<Self>()
            .error_context("Failed to parse memory bounds configuration.")
    }
}

/// Initializes the memory bounds system and verifies any configured bounds.
///
/// This function takes a closure that is responsible for populating the memory bounds for all components within the
/// application that should be enforced. This allows the caller to build up the memory bounds in a structured way.
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
pub fn initialize_memory_bounds<F>(
    configuration: MemoryBoundsConfiguration, populate_bounds: F,
) -> Result<MemoryLimiter, GenericError>
where
    F: FnOnce(&mut MemoryBoundsBuilder),
{
    let initial_grant = match configuration.memory_limit {
        Some(limit) => MemoryGrant::with_slop_factor(limit.as_u64() as usize, configuration.memory_slop_factor)?,
        None => {
            info!("No memory limit set for the process. Skipping memory bounds verification.");
            return Ok(MemoryLimiter::noop());
        }
    };

    // Run the provided closure to populate the memory bounds that we want to verify.
    let mut bounds_builder = MemoryBoundsBuilder::new();
    populate_bounds(&mut bounds_builder);
    let component_bounds = bounds_builder.finalize();

    // Now verify the bounds.
    let bounds_verifier = BoundsVerifier::new(initial_grant, component_bounds);
    let verified_bounds = bounds_verifier.verify()?;

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

struct ComponentAllocationMetrics {
    totals: AllocationStatsDelta,
    allocated_bytes_total: Counter,
    allocated_bytes_live: Gauge,
    allocated_objects_total: Counter,
    allocated_objects_live: Gauge,
    deallocated_bytes_total: Counter,
    deallocated_objects_total: Counter,
}

impl ComponentAllocationMetrics {
    fn new(component_name: &str) -> Self {
        Self {
            totals: AllocationStatsDelta::empty(),
            allocated_bytes_total: counter!("component_allocated_bytes_total", "component_id" => component_name.to_string()),
            allocated_bytes_live: gauge!("component_allocated_bytes_live", "component_id" => component_name.to_string()),
            allocated_objects_total: counter!("component_allocated_objects_total", "component_id" => component_name.to_string()),
            allocated_objects_live: gauge!("component_allocated_objects_live", "component_id" => component_name.to_string()),
            deallocated_bytes_total: counter!("component_deallocated_bytes_total", "component_id" => component_name.to_string()),
            deallocated_objects_total: counter!("component_deallocated_objects_total", "component_id" => component_name.to_string()),
        }
    }

    fn update(&mut self, stats: &AllocationStats) {
        let delta = stats.consume();
        self.totals.merge(&delta);

        self.allocated_bytes_total.increment(delta.allocated_bytes as u64);
        self.allocated_objects_total.increment(delta.allocated_objects as u64);
        self.deallocated_bytes_total.increment(delta.deallocated_bytes as u64);
        self.deallocated_objects_total
            .increment(delta.deallocated_objects as u64);
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
    static INIT: AtomicBool = AtomicBool::new(false);
    if INIT.swap(true, Relaxed) {
        return Err(generic_error!("Memory allocator subsystem already initialized."));
    }

    // We can't enforce, at compile-time, that the tracking allocator must be installed if a caller is trying to
    // initialize the allocator's reporting infrastructure... but we can at least warn them if we detect it's not
    // installed.
    //
    // (Our logic here is that since a custom global allocator is used for _everything_ by default, the root component
    // _has_ to have handled some allocations by the point we get here if it's actually installed.)
    if !AllocationStats::root().has_allocated() {
        warn!("Tracking allocator not detected. Memory telemetry will not be available.");
    }

    // Spawn the background task that will periodically collect memory usage statistics.
    tokio::spawn(async {
        let mut metrics = HashMap::new();

        loop {
            sleep(Duration::from_secs(1)).await;

            ComponentRegistry::global().visit_components(|component_name, stats| {
                let component_metrics = match metrics.get_mut(component_name) {
                    Some(component_metrics) => component_metrics,
                    None => metrics
                        .entry(component_name.to_string())
                        .or_insert_with(|| ComponentAllocationMetrics::new(component_name)),
                };

                component_metrics.update(stats);
            })
        }
    });

    Ok(())
}

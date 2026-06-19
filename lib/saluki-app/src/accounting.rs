//! Resource accounting and telemetry.

use std::{collections::VecDeque, env, fs, time::Duration};

use bytesize::ByteSize;
use metrics::{counter, gauge, Counter, Gauge, Level};
use resource_accounting::{
    ComponentBounds, ComponentRegistry, ComponentRegistryHandle, MemoryGrant, MemoryLimiter, ResourceGroupRegistry,
    ResourceStats, ResourceStatsSnapshot,
};
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::{collections::FastHashMap, sync::shutdown::ShutdownHandle};
use saluki_config_tools::GenericConfiguration;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use tokio::{select, time::sleep};
use tonic::async_trait;
use tracing::{error, info, warn};

const fn default_memory_slop_factor() -> f64 {
    0.25
}

const fn default_enable_global_limiter() -> bool {
    true
}

/// Bounds validation and global memory limiter behavior.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    /// Bounds validation is skipped, and no memory limiting is applied.
    #[default]
    Disabled,

    /// Treat bounds validation failures as non-fatal.
    ///
    /// Global memory limiter will be enabled and active if a memory limit is configured.
    Permissive,

    /// Treat bounds validation failures as fatal.
    ///
    /// Global memory limiter will be enabled and active if a memory limit is configured.
    Strict,
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
    /// 100 MB, we would then verify that the memory bounds can fit within 75 MB (100 MB * (1 - 0.25) => 75 MB).
    #[serde(default = "default_memory_slop_factor")]
    memory_slop_factor: f64,

    /// Whether or not to enable the global memory limiter.
    ///
    /// When set to `false`, the global memory limiter will operate in a no-op mode. All calls to use it will never
    /// exert backpressure, and only the inherent memory bounds of the running components will influence memory usage.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_global_limiter")]
    enable_global_limiter: bool,

    /// The memory mode to use when reconciling the calculated memory bounds against the configured memory limit.
    ///
    /// See [`MemoryMode`] for the available modes and their behavior.
    ///
    /// Defaults to [`MemoryMode::Disabled`].
    #[serde(default)]
    memory_mode: MemoryMode,
}

impl MemoryBoundsConfiguration {
    /// Attempts to read memory bounds configuration from the provided configuration.
    ///
    /// # Errors
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
                        info!(
                            "Setting memory limit to {} based on detected cgroups limit.",
                            memory.display().si()
                        );
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

/// Initializes the memory bounds system and verifies any configured bounds based on the configured memory mode.
///
/// See [`MemoryMode`] for details on the behavior of each mode.
///
/// # Errors
///
/// If the bounds could not be validated under [`MemoryMode::Strict`], or if the configured grant is invalid, an error
/// is returned.
pub fn initialize_memory_bounds(
    configuration: MemoryBoundsConfiguration, component_registry: ComponentRegistryHandle,
) -> Result<MemoryLimiter, GenericError> {
    let configured_grant = configuration
        .memory_limit
        .map(|limit| MemoryGrant::with_slop_factor(limit.as_u64() as usize, configuration.memory_slop_factor))
        .transpose()?;

    let limiter_grant = match configuration.memory_mode {
        MemoryMode::Disabled => {
            info!("Memory limiting disabled.");
            None
        }
        mode @ (MemoryMode::Permissive | MemoryMode::Strict) => match configured_grant {
            Some(grant) => {
                verify_bounds_for_mode(mode, grant, &component_registry)?;
                Some(grant)
            }
            None => {
                info!("No memory limit set for the process. Skipping memory bounds verification.");
                None
            }
        },
    };

    let limiter = match limiter_grant {
        Some(grant) if configuration.enable_global_limiter => MemoryLimiter::new(grant)
            .ok_or_else(|| generic_error!("Memory statistics cannot be gathered on this system."))?,
        _ => MemoryLimiter::noop(),
    };

    Ok(limiter)
}

fn verify_bounds_for_mode(
    mode: MemoryMode, initial_grant: MemoryGrant, component_registry: &ComponentRegistryHandle,
) -> Result<(), GenericError> {
    match component_registry.verify_bounds(initial_grant) {
        Ok(verified_bounds) => {
            info!(
				"Verified memory bounds. Minimum memory requirement of {}, with a calculated firm memory bound of {} out of {} available, from an initial {} grant.",
				bytes_to_si_string(verified_bounds.total_minimum_required_bytes()),
				bytes_to_si_string(verified_bounds.total_firm_limit_bytes()),
				bytes_to_si_string(verified_bounds.total_available_bytes()),
				bytes_to_si_string(initial_grant.initial_limit_bytes()),
			);

            print_bounds(verified_bounds.bounds());
            Ok(())
        }
        Err(e) => {
            let bounds = component_registry.as_bounds();
            print_bounds(&bounds);

            match mode {
                MemoryMode::Strict => {
                    error!("Failed to verify memory bounds: {}.", e);
                    Err(generic_error!(
                        "Configured memory limit is insufficient for the current configuration."
                    ))
                }
                MemoryMode::Permissive => {
                    warn!(
                        "Configured memory limit ({}) may be insufficient for the current configuration. Memory limiting behavior will be best effort. Continuing.",
                        bytes_to_si_string(initial_grant.initial_limit_bytes()),
                    );
                    Ok(())
                }
                MemoryMode::Disabled => unreachable!("verify_bounds_for_mode is never called with Disabled mode"),
            }
        }
    }
}

fn print_bounds(bounds: &ComponentBounds) {
    info!("Breakdown of verified bounds:");
    info!(
        "- (root): {} minimum, {} firm",
        bytes_to_si_string(bounds.total_minimum_required_bytes()),
        bytes_to_si_string(bounds.total_firm_limit_bytes()),
    );

    let mut to_visit = VecDeque::new();
    to_visit.extend(
        bounds
            .subcomponents()
            .into_iter()
            .map(|(name, bounds)| (1, name, bounds)),
    );

    while let Some((depth, component_name, component_bounds)) = to_visit.pop_front() {
        info!(
            "{:indent$}- {}: {} minimum, {} firm",
            "",
            component_name,
            bytes_to_si_string(component_bounds.total_minimum_required_bytes()),
            bytes_to_si_string(component_bounds.total_firm_limit_bytes()),
            indent = depth * 2
        );

        let mut subcomponents = component_bounds.subcomponents().into_iter().collect::<Vec<_>>();
        while let Some((subcomponent_name, subcomponent_bounds)) = subcomponents.pop() {
            to_visit.push_front((depth + 1, subcomponent_name, subcomponent_bounds));
        }
    }

    info!("");
}

struct ResourceGroupMetrics {
    totals: ResourceStatsSnapshot,
    allocated_bytes_total: Counter,
    allocated_bytes_live: Gauge,
    allocated_objects_total: Counter,
    allocated_objects_live: Gauge,
    deallocated_bytes_total: Counter,
    deallocated_objects_total: Counter,
    cpu_time_nanos_total: Counter,
}

impl ResourceGroupMetrics {
    fn new(group_name: &str) -> Self {
        Self {
            totals: ResourceStatsSnapshot::empty(),
            allocated_bytes_total: counter!(level: Level::DEBUG, "group_allocated_bytes_total", "group_id" => group_name.to_string()),
            allocated_bytes_live: gauge!(level: Level::DEBUG, "group_allocated_bytes_live", "group_id" => group_name.to_string()),
            allocated_objects_total: counter!(level: Level::DEBUG, "group_allocated_objects_total", "group_id" => group_name.to_string()),
            allocated_objects_live: gauge!(level: Level::DEBUG, "group_allocated_objects_live", "group_id" => group_name.to_string()),
            deallocated_bytes_total: counter!(level: Level::DEBUG, "group_deallocated_bytes_total", "group_id" => group_name.to_string()),
            deallocated_objects_total: counter!(level: Level::DEBUG, "group_deallocated_objects_total", "group_id" => group_name.to_string()),
            cpu_time_nanos_total: counter!(level: Level::DEBUG, "group_cpu_time_nanos_total", "group_id" => group_name.to_string()),
        }
    }

    fn update(&mut self, stats: &ResourceStats) {
        let delta = stats.snapshot_delta(&self.totals);

        self.allocated_bytes_total.increment(delta.allocated_bytes as u64);
        self.allocated_objects_total.increment(delta.allocated_objects as u64);
        self.deallocated_bytes_total.increment(delta.deallocated_bytes as u64);
        self.deallocated_objects_total
            .increment(delta.deallocated_objects as u64);
        self.cpu_time_nanos_total.increment(delta.cpu_time_nanos);

        self.totals.merge(&delta);
        self.allocated_bytes_live
            .set((self.totals.allocated_bytes - self.totals.deallocated_bytes) as f64);
        self.allocated_objects_live
            .set((self.totals.allocated_objects - self.totals.deallocated_objects) as f64);
    }
}

/// A worker that periodically collects per-resource group memory usage statistics and emits the internal telemetry.
///
/// Additionally, asserts the memory API routes from the given [`ComponentRegistry`] as a [`DynamicRoute`] on the
/// unprivileged API endpoint.
pub struct ResourceTelemetryWorker {
    component_registry: ComponentRegistryHandle,
}

impl ResourceTelemetryWorker {
    /// Creates a new `ResourceTelemetryWorker` for the given component registry.
    pub fn new(component_registry: &ComponentRegistry) -> Self {
        Self {
            component_registry: component_registry.root(),
        }
    }
}

#[async_trait]
impl Supervisable for ResourceTelemetryWorker {
    fn name(&self) -> &str {
        "resource-telemetry"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // We can't enforce, at compile-time, that the tracking allocator must be installed if a caller is trying to
        // initialize the allocator's reporting infrastructure... but we can at least warn them if we detect it's not
        // installed here at runtime.
        if !ResourceGroupRegistry::allocator_installed() {
            warn!("Tracking allocator not installed. Memory telemetry will not be available.");
        }

        let memory_routes = DynamicRoute::http(EndpointType::Unprivileged, self.component_registry.api_handler());

        Ok(Box::pin(async move {
            // Register our API routes before we actually start running.
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(memory_routes, "resource-telemetry-api");

            select! {
                _ = process_shutdown => {},
                _ = run_resource_group_metrics_loop() => {},
            }

            Ok(())
        }))
    }
}

async fn run_resource_group_metrics_loop() {
    let mut metrics = FastHashMap::default();

    loop {
        ResourceGroupRegistry::global().visit_resource_groups(|group_name, stats| {
            let group_metrics = match metrics.get_mut(group_name) {
                Some(group_metrics) => group_metrics,
                None => metrics
                    .entry(group_name.to_string())
                    .or_insert_with(|| ResourceGroupMetrics::new(group_name)),
            };

            group_metrics.update(stats);
        });

        sleep(Duration::from_secs(1)).await;
    }
}

struct CgroupMemoryParser;

impl CgroupMemoryParser {
    /// Parse memory limit from memory controller.
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

fn bytes_to_si_string(bytes: usize) -> bytesize::Display {
    ByteSize::b(bytes as u64).display().si()
}

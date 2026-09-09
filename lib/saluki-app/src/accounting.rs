//! Resource accounting and telemetry.

use std::{collections::VecDeque, env, fs, time::Duration};

use bytesize::ByteSize;
use metrics::{counter, gauge, Counter, Gauge, Level};
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceStats, ResourceStatsSnapshot};
use saluki_common::{collections::FastHashMap, sync::shutdown::ShutdownHandle};
use saluki_core::accounting::{
    ComponentBounds, ComponentRegistry, ComponentRegistryHandle, MemoryGrant, MemoryLimiter,
};
use saluki_core::{
    diagnostic::DiagnosticsEmitter,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
    support::SubsystemIdentifier,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{select, time::sleep};
use tonic::async_trait;
use tracing::{error, info, warn};

/// Bounds validation and global memory limiter behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
pub struct MemoryBoundsConfiguration {
    /// Process memory limit. `None` enables cgroup detection.
    pub memory_limit: Option<ByteSize>,

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
    pub memory_slop_factor: f64,

    /// Whether or not to enable the global memory limiter.
    ///
    /// When set to `false`, the global memory limiter will operate in a no-op mode. All calls to use it will never
    /// exert backpressure, and only the inherent memory bounds of the running components will influence memory usage.
    pub enable_global_limiter: bool,

    /// The memory mode to use when reconciling the calculated memory bounds against the configured memory limit.
    ///
    /// See [`MemoryMode`] for the available modes and their behavior.
    pub memory_mode: MemoryMode,
}

/// Initializes the memory bounds system and verifies any configured bounds based on the configured memory mode.
///
/// When no memory limit is configured and `DOCKER_DD_AGENT` is set to a non-empty value, the limit is read from the
/// process cgroup instead.
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
    let memory_limit = configuration.memory_limit.or_else(detect_cgroup_memory_limit);

    let configured_grant = memory_limit
        .map(|limit| MemoryGrant::with_slop_factor(limit.as_u64() as usize, configuration.memory_slop_factor))
        .transpose()
        .error_context("Given memory limit and/or slop factor invalid.")?;

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

        let component_registry = self.component_registry.clone();

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            // Register our API routes before we actually start running.
            dataspace.assert(memory_routes, "resource-telemetry-api");

            // Expose our diagnostic artifact via the diagnostics control surface.
            let diagnostics = DiagnosticsEmitter::from_dataspace(
                SubsystemIdentifier::from_segments(["resource-telemetry"]),
                dataspace,
            );
            diagnostics.register_collector("memory_status.json", move || {
                component_registry.memory_snapshot_json().into_bytes()
            });

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

fn detect_cgroup_memory_limit() -> Option<ByteSize> {
    if !env::var("DOCKER_DD_AGENT").is_ok_and(|value| !value.is_empty()) {
        return None;
    }

    let memory = CgroupMemoryParser.parse()?;
    info!(
        "Setting memory limit to {} based on detected cgroups limit.",
        memory.display().si()
    );

    Some(memory)
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

#[cfg(test)]
mod tests {
    use saluki_config::test_env_lock;
    use saluki_core::support::SubsystemIdentifier;

    use super::*;

    fn config_with_limit(limit: ByteSize, memory_mode: MemoryMode) -> MemoryBoundsConfiguration {
        MemoryBoundsConfiguration {
            memory_limit: Some(limit),
            memory_slop_factor: 0.25,
            enable_global_limiter: false,
            memory_mode,
        }
    }

    fn registry_with_firm_bound(bytes: usize) -> ComponentRegistry {
        let registry = ComponentRegistry::default();
        registry
            .bounds_builder(&SubsystemIdentifier::from_dotted("test"))
            .firm()
            .with_fixed_amount("buffer", bytes);
        registry
    }

    #[test]
    fn cgroup_memory_parser_converts_raw_limits_to_bytes() {
        // The cgroup memory files hold a bare byte count, or the literal `max` when no limit is set. `max` and any
        // unparseable value yield `None`; a numeric value parses to that many bytes (after trimming whitespace).
        let cases: &[(&str, Option<u64>)] = &[
            ("max", None),
            ("1073741824", Some(1_073_741_824)),
            ("  1048576\n", Some(1_048_576)),
            ("not-a-number", None),
        ];

        for (raw, expected) in cases {
            let actual = CgroupMemoryParser.convert_to_bytesize(raw).map(|bytes| bytes.as_u64());
            assert_eq!(actual, *expected, "raw input: {raw:?}");
        }
    }

    #[test]
    fn out_of_range_slop_factor_is_rejected() {
        let mut config = config_with_limit(ByteSize::mib(1), MemoryMode::Strict);
        config.memory_slop_factor = 1.5;

        let error = initialize_memory_bounds(config, ComponentRegistry::default().root())
            .err()
            .expect("a slop factor of 1.5 should be rejected");
        assert!(error
            .to_string()
            .contains("Given memory limit and/or slop factor invalid."));
    }

    #[test]
    fn disabled_memory_mode_skips_bounds_verification() {
        let registry = registry_with_firm_bound(64 * 1024 * 1024);

        initialize_memory_bounds(
            config_with_limit(ByteSize::b(1024), MemoryMode::Disabled),
            registry.root(),
        )
        .expect("disabled mode should not verify bounds");
    }

    #[test]
    fn strict_memory_mode_rejects_bounds_exceeding_the_limit() {
        let registry = registry_with_firm_bound(64 * 1024 * 1024);

        let error = initialize_memory_bounds(
            config_with_limit(ByteSize::b(1024), MemoryMode::Strict),
            registry.root(),
        )
        .err()
        .expect("bounds larger than the limit should be fatal in strict mode");
        assert!(error
            .to_string()
            .contains("Configured memory limit is insufficient for the current configuration."));
    }

    #[test]
    fn permissive_memory_mode_tolerates_bounds_exceeding_the_limit() {
        let registry = registry_with_firm_bound(64 * 1024 * 1024);

        initialize_memory_bounds(
            config_with_limit(ByteSize::b(1024), MemoryMode::Permissive),
            registry.root(),
        )
        .expect("bounds larger than the limit should be non-fatal in permissive mode");
    }

    #[test]
    fn absent_memory_limit_skips_bounds_verification() {
        let registry = registry_with_firm_bound(64 * 1024 * 1024);
        let config = MemoryBoundsConfiguration {
            memory_limit: None,
            memory_slop_factor: 0.25,
            enable_global_limiter: false,
            memory_mode: MemoryMode::Strict,
        };

        let _env_guard = test_env_lock();
        std::env::remove_var("DOCKER_DD_AGENT");

        initialize_memory_bounds(config, registry.root()).expect("no limit means no bounds verification");
    }

    #[test]
    fn cgroup_memory_limit_is_not_detected_outside_a_container() {
        let _env_guard = test_env_lock();
        std::env::remove_var("DOCKER_DD_AGENT");
        assert_eq!(detect_cgroup_memory_limit(), None);

        std::env::set_var("DOCKER_DD_AGENT", "");
        assert_eq!(detect_cgroup_memory_limit(), None);
        std::env::remove_var("DOCKER_DD_AGENT");
    }
}

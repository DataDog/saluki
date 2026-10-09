//! The plan-driven metrics path: the part of the topology that the default program's physical frame describes.
//!
//! The frame, computed at boot from the default program, fixes the structure of the metrics path for the life of the
//! process (see the plan crate's `compile::frame`). [`PlanTopology`] builds it into the topology blueprint:
//!
//! - **Fixed components** are built exactly as natively, with the same component IDs and builders: the sources by
//!   their own pipelines (which register their metrics outputs here), DogStatsD aggregation (`dsd_agg`, with its
//!   context snapshot channel), and the policy gateways, which run as opaque hosts backed by their native components
//!   until stage 4. Each output of a fixed component is an admission point: sources' outputs admit, and the outputs of
//!   aggregation and the gateways readmit, all from the one schedule the installer commits to.
//! - **Slots** each run in one region host, named by [`slot_name`]: `dsd_pre_aggregation` (the DogStatsD mapper
//!   through the tag filterlist) and `metrics_enrich` (the post-aggregation filter through the fan-out to the outputs).
//!   A region host's outputs are its slot's exits, in order.
//! - **Outputs** are the native encoders and forwarders, built exactly as natively, and the DogStatsD client telemetry
//!   tap, `dsd_client_telemetry_out`, which the `metrics_enrich` slot feeds.
//! - **Edges** are the frame's: one blueprint edge per physical edge, from an admission point or a slot's exit to a
//!   fixed component, a slot, or an output's component. Exits the default program does not use are connected and stay
//!   idle, so that later generations can use them.
//!
//! The DogStatsD taps on `dsd_in.metrics` (`dsd_stats_out`, `dsd_debug_log_out`) and everything outside the metrics
//! path keep their native construction, in `run.rs`.

use std::{collections::BTreeSet, num::NonZeroUsize, sync::Arc};

use agent_data_plane_config::{shared::SharedConfiguration, SalukiConfiguration};
use agent_data_plane_config_system::ConfigurationSystem;
use saluki_components::{
    destinations::DogStatsDClientTelemetryConfiguration, transforms::AggregateContextSnapshotHandle,
};
use saluki_core::{
    execution::{AdmissionPointConfig, RegionHostBuilder},
    topology::TopologyBlueprint,
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_plan::{
    compile::{FixedComponent, Frame, FrameSource, FrameTarget, Slot},
    ir::{NativeComponentId, NodeId},
    limits,
    operators::names,
    runtime::{build_kernels, MetricRegionDriver, PanicBudget, PrepareRegion, RegionBounds, RegionConfig},
};
use serde_json::Value;

use super::{
    bindings::routing_groups,
    installer::{BootTopology, ProgrammableMetrics},
    lowering::ids,
    outputs::{NativeOutputs, OutputKind},
};
use crate::cli::run::{
    autoscaling_failover_branch, dogstatsd_aggregate_configuration, main_metrics_encoder, mrf_metrics_branch,
    routing_group_branch,
};

/// Names the region host of a slot of the default program's frame, after the native components it replaces.
///
/// The slot holding the DogStatsD mapper is `dsd_pre_aggregation`, and the slot holding host enrichment is
/// `metrics_enrich`, which keeps the native ID that the components feeding metrics enrichment have always been
/// connected to. A slot that R1's cost model cuts from one of them, so that a costly chain pipelines across regions,
/// is named after the native component it starts with: `<first node>_region`, without the `_in` of a merge that
/// starts it, such as `dsd_prefix_filter_region`.
///
/// # Errors
///
/// Returns an error for a slot without nodes, which no frame has.
pub fn slot_name(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    let has = |id: &str| nodes.iter().any(|node| node.to_string() == id);
    let name = if has(ids::DSD_ENRICH) {
        "dsd_pre_aggregation".to_string()
    } else if has(ids::METRICS_ENRICH) {
        ids::METRICS_ENRICH.to_string()
    } else {
        let first = nodes
            .first()
            .ok_or_else(|| "a region of the default program has no nodes; this is a bug".to_string())?
            .local()
            .as_str();
        format!("{}_region", first.strip_suffix("_in").unwrap_or(first))
    };
    NativeComponentId::new(name).map_err(|error| format!("{}; this is a lowering bug", error))
}

/// Builds the plan-driven metrics path into a topology blueprint, from what [`ProgrammableMetrics::boot`] prepared.
///
/// `run.rs` calls it in the order it builds the topology: [`add_outputs`][Self::add_outputs] before the pipelines that
/// feed the metrics path, [`add_source`][Self::add_source] for each of their metrics outputs,
/// [`add_dogstatsd`][Self::add_dogstatsd] with the DogStatsD pipeline, and [`finish`][Self::finish] last.
pub struct PlanTopology<'a> {
    boot: &'a mut BootTopology,
    sources: BTreeSet<String>,
    gateways: BTreeSet<String>,
    aggregate: bool,
}

impl<'a> PlanTopology<'a> {
    /// Creates the builder of the metrics path that `metrics` prepared.
    pub fn new(metrics: &'a mut ProgrammableMetrics) -> Self {
        Self {
            boot: metrics.boot_topology(),
            sources: BTreeSet::new(),
            gateways: BTreeSet::new(),
            aggregate: false,
        }
    }

    /// Adds the outputs of the frame, with their encoders and forwarders, and the gateways in front of them, built as
    /// natively.
    ///
    /// # Errors
    ///
    /// Returns an error if a component cannot be configured, or if the frame and the native outputs disagree, which is
    /// a lowering bug.
    pub fn add_outputs(
        &mut self, blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    ) -> Result<(), GenericError> {
        let startup = config_system.config();
        let routing = routing_groups(&startup)?;
        // The failover branches as native wiring builds them, which warns when one is requested but cannot be wired.
        // The Cluster Agent endpoint is a host fact, resolved once at boot.
        let mut mrf = mrf_metrics_branch(config_system, shared, &startup.domains.multi_region_failover);
        let cluster_agent = self
            .boot
            .facts
            .cluster_agent_endpoint
            .clone()
            .zip(shared.cluster_agent.auth_token.clone());
        let mut autoscaling = autoscaling_failover_branch(shared, cluster_agent)?;
        let outputs = &self.boot.outputs;
        for decl in self.boot.frame.outputs() {
            let output = outputs.get(decl.id.as_str()).ok_or_else(|| {
                generic_error!(
                    "The default program delivers to output `{}`, which is not a native metrics output; this is a \
                     lowering bug.",
                    decl.id
                )
            })?;
            let component = output.component.as_str();
            let missing = |what: &str| {
                generic_error!(
                    "The default program delivers to output `{}`, but the native {} is not wired; this is a lowering \
                     bug.",
                    decl.id,
                    what
                )
            };
            match &output.kind {
                OutputKind::ClientTelemetry => {
                    blueprint.add_destination(component, DogStatsDClientTelemetryConfiguration)?;
                }
                OutputKind::Main => {
                    blueprint.add_encoder(component, main_metrics_encoder(shared, &routing))?;
                }
                OutputKind::RoutingGroup(group) => {
                    let policy = routing
                        .policy_groups()
                        .get(group.index)
                        .ok_or_else(|| missing("endpoint routing group"))?;
                    let (filter, encoder) = routing_group_branch(shared, policy);
                    let gateway = ids::routing_filter(group.index);
                    blueprint
                        .add_transform(gateway.as_str(), filter)?
                        .add_encoder(component, encoder)?;
                    self.gateways.insert(gateway);
                }
                OutputKind::MultiRegionFailover => {
                    let branch = mrf.take().ok_or_else(|| missing("Multi-Region Failover branch"))?;
                    let forwarder = output.forwarder.ok_or_else(|| missing("forwarder"))?;
                    blueprint
                        .add_transform(ids::MRF_METRICS_GATEWAY, branch.gateway)?
                        .add_encoder(component, branch.encoder)?
                        .add_forwarder(forwarder, branch.forwarder)?;
                    self.gateways.insert(ids::MRF_METRICS_GATEWAY.to_string());
                }
                OutputKind::AutoscalingFailover => {
                    let branch = autoscaling
                        .take()
                        .ok_or_else(|| missing("autoscaling failover branch"))?;
                    let forwarder = output.forwarder.ok_or_else(|| missing("forwarder"))?;
                    blueprint
                        .add_transform(ids::AF_METRICS_GATEWAY, branch.gateway)?
                        .add_encoder(component, branch.encoder)?
                        .add_forwarder(forwarder, branch.forwarder)?;
                    self.gateways.insert(ids::AF_METRICS_GATEWAY.to_string());
                }
            }
            // Encoders feed their forwarder, as natively: `dd_out`, shared with the other pipelines, or their own.
            if let Some(forwarder) = output.forwarder {
                blueprint.connect_components(component, forwarder)?;
            }
        }
        if mrf.is_some() || autoscaling.is_some() {
            return Err(generic_error!(
                "Native wiring builds a failover branch that the default program does not deliver to; this is a \
                 lowering bug."
            ));
        }
        Ok(())
    }

    /// Registers a component output that carries metrics into the metrics path, which the frame must read.
    pub fn add_source(&mut self, output: &str) {
        self.sources.insert(output.to_string());
    }

    /// Adds DogStatsD aggregation, a fixed component of the frame, and registers the DogStatsD source's metrics
    /// output; returns the handle of aggregation's context snapshots.
    ///
    /// # Errors
    ///
    /// Returns an error if aggregation cannot be configured, or if the frame has no aggregation, which is a lowering
    /// bug.
    pub fn add_dogstatsd(
        &mut self, blueprint: &mut TopologyBlueprint, typed: &SalukiConfiguration,
    ) -> Result<AggregateContextSnapshotHandle, GenericError> {
        let aggregation = self
            .boot
            .frame
            .fixed()
            .iter()
            .find(|fixed| fixed.operator.as_str() == names::AGGREGATE)
            .ok_or_else(|| {
                generic_error!(
                    "The DogStatsD pipeline is enabled, but the default program has no aggregation; this is a \
                     lowering bug."
                )
            })?;
        let (aggregate, snapshots) = dogstatsd_aggregate_configuration(typed)?;
        blueprint.add_transform(component_id(aggregation)?, aggregate)?;
        self.aggregate = true;
        self.sources.insert(format!("{}.metrics", ids::DSD_IN));
        Ok(snapshots)
    }

    /// Adds a region host per slot, connects every edge of the frame, and marks every admission point.
    ///
    /// # Errors
    ///
    /// Returns an error if a region host cannot be created, if an edge or an admission point names a component or an
    /// output the topology does not have, or if the frame and the native pipelines disagree about the fixed components
    /// and sources, which is a lowering bug.
    pub fn finish(self, blueprint: &mut TopologyBlueprint) -> Result<(), GenericError> {
        let Self {
            boot,
            sources,
            gateways,
            aggregate,
        } = self;
        check_fixed_components(&boot.frame, &sources, &gateways, aggregate)?;
        add_regions(blueprint, boot)?;
        connect_frame(blueprint, boot)
    }
}

/// Adds a region host per slot of the frame, each starting with the boot generation's program for its slot.
pub(super) fn add_regions(blueprint: &mut TopologyBlueprint, boot: &mut BootTopology) -> Result<(), GenericError> {
    let frame = Arc::clone(&boot.frame);
    for slot in frame.slots() {
        let host = region_host(boot, slot)?;
        blueprint.add_transform(slot.name.as_str(), host)?;
    }
    Ok(())
}

/// Connects every physical edge of the frame, and marks every admission point, reading the boot schedule.
pub(super) fn connect_frame(blueprint: &mut TopologyBlueprint, boot: &BootTopology) -> Result<(), GenericError> {
    let frame = &boot.frame;
    for edge in frame.edges() {
        let from = source_output(frame, &edge.from)?;
        let to = target_component(frame, &boot.outputs, &edge.to)?;
        blueprint.connect_components(from.as_str(), to.as_str())?;
    }

    // No heartbeats until stage 6: through stage 5 every state owner is a host component.
    for point in frame.admission_points() {
        let output = admission_output(frame, &point.node)?;
        let config = AdmissionPointConfig::new(
            point.id,
            point.mode,
            Arc::clone(&boot.schedule),
            limits::values::CLOCK_RESET_THRESHOLD,
        );
        blueprint.with_admission_point(output, config)?;
    }
    Ok(())
}

/// Checks that the native pipelines built every fixed component of the frame, and that the frame reads every output
/// that carries metrics into the metrics path.
fn check_fixed_components(
    frame: &Frame, sources: &BTreeSet<String>, gateways: &BTreeSet<String>, aggregate: bool,
) -> Result<(), GenericError> {
    let mut read = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    for fixed in frame.fixed() {
        match fixed.operator.as_str() {
            names::SOURCE => {
                read.insert(admission_output(frame, &fixed.node)?);
            }
            names::AGGREGATE if aggregate => {}
            names::OPAQUE_HOST => {
                hosts.insert(component_id(fixed)?);
            }
            other => {
                return Err(generic_error!(
                    "The default program has a `{}` node, `{}`, that no native component runs; this is a lowering bug.",
                    other,
                    fixed.node
                ))
            }
        }
    }
    if let Some(unread) = sources.difference(&read).next() {
        return Err(generic_error!(
            "The `{}` output carries metrics, but the default program has no source for it; this is a lowering bug.",
            unread
        ));
    }
    if let Some(missing) = read.difference(sources).next() {
        return Err(generic_error!(
            "The default program reads metrics from `{}`, which no native pipeline provides; this is a lowering bug.",
            missing
        ));
    }
    if hosts != *gateways {
        return Err(generic_error!(
            "The default program's opaque hosts ({}) are not the native gateways ({}); this is a lowering bug.",
            hosts.into_iter().collect::<Vec<_>>().join(", "),
            gateways.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

/// Returns the region host of a slot, whose driver runs the boot generation's program for the slot.
///
/// The driver and the boot program's kernels are built when the topology builds the region, inside its runtime.
fn region_host(boot: &mut BootTopology, slot: &Slot) -> Result<RegionHostBuilder<MetricRegionDriver>, GenericError> {
    let program = boot.programs.get(slot.id.get()).cloned().ok_or_else(|| {
        generic_error!(
            "The boot generation has no program for region `{}`; this is a bug.",
            slot.name
        )
    })?;
    let mailbox = boot
        .mailboxes
        .get_mut(slot.id.get())
        .and_then(Option::take)
        .ok_or_else(|| {
            generic_error!(
                "The control mailbox of region `{}` was already taken; this is a bug.",
                slot.name
            )
        })?;
    let bounds = RegionBounds::new(&boot.plan, &boot.registry, &program)?;
    let work_per_turn = usize::try_from(limits::values::WORK_PER_TURN)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| generic_error!("The work per turn of a region must be a positive number of batches."))?;
    let exits = slot.exits.iter().map(|exit| exit.name.to_string()).collect::<Vec<_>>();

    let generation = boot.generation.clone();
    let telemetry = boot.telemetry.clone();
    let failures = Arc::clone(&boot.failures);
    let slot = slot.clone();
    let host = RegionHostBuilder::new(exits, work_per_turn, mailbox, move |_| {
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(&program, keys, |node| telemetry.kernel_context(&program, node))?;
        let prepare = PrepareRegion::new(
            generation.clone(),
            Arc::clone(&program),
            kernels,
            PanicBudget::DEFAULT_PROGRAM,
            &telemetry,
        );
        let config = RegionConfig::for_slot(&slot, &telemetry).with_failure_reporter(Arc::clone(&failures));
        Ok(MetricRegionDriver::new(config, prepare)?)
    })?;
    Ok(host.with_bounds(bounds))
}

/// Returns the component ID a fixed component runs as: an opaque host's `component` parameter, or otherwise its node's
/// ID, as sources and aggregation have their native component's ID.
pub(super) fn component_id(fixed: &FixedComponent) -> Result<String, GenericError> {
    if fixed.operator.as_str() == names::OPAQUE_HOST {
        return fixed
            .params
            .get("component")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                generic_error!(
                    "Opaque host `{}` names no component; this is a lowering bug.",
                    fixed.node
                )
            });
    }
    Ok(fixed.node.local().to_string())
}

/// Returns the component output an admission point is: a source's port is its output of the same name, and the one
/// port of aggregation or an opaque host is its default output.
pub(super) fn admission_output(frame: &Frame, node: &NodeId) -> Result<String, GenericError> {
    let fixed = frame
        .fixed_component(node)
        .ok_or_else(|| generic_error!("The frame has no fixed component `{}`; this is a bug.", node))?;
    let component = component_id(fixed)?;
    if fixed.operator.as_str() == names::SOURCE {
        let point = fixed
            .admission_points
            .first()
            .and_then(|id| frame.admission_point(*id))
            .ok_or_else(|| generic_error!("Source `{}` has no output; this is a lowering bug.", node))?;
        return Ok(format!("{}.{}", component, point.port));
    }
    if fixed.admission_points.len() != 1 {
        return Err(generic_error!(
            "Host component `{}` has {} outputs in the default program, but runs a native component with one; this is \
             a lowering bug.",
            node,
            fixed.admission_points.len()
        ));
    }
    Ok(component)
}

/// Returns the component output a physical edge starts at.
fn source_output(frame: &Frame, source: &FrameSource) -> Result<String, GenericError> {
    match source {
        FrameSource::Admission(id) => {
            let point = frame
                .admission_point(*id)
                .ok_or_else(|| generic_error!("The frame has no admission point {}; this is a bug.", id.get()))?;
            admission_output(frame, &point.node)
        }
        FrameSource::Exit { slot, exit } => {
            let slot = frame
                .slot(*slot)
                .ok_or_else(|| generic_error!("The frame has no slot {}; this is a bug.", slot))?;
            let exit = slot
                .exits
                .get(exit.get())
                .ok_or_else(|| generic_error!("Region `{}` has no exit {}; this is a bug.", slot.name, exit))?;
            Ok(format!("{}.{}", slot.name, exit.name))
        }
    }
}

/// Returns the component a physical edge leads to.
fn target_component(frame: &Frame, outputs: &NativeOutputs, target: &FrameTarget) -> Result<String, GenericError> {
    match target {
        FrameTarget::Fixed(node) => {
            let fixed = frame
                .fixed_component(node)
                .ok_or_else(|| generic_error!("The frame has no fixed component `{}`; this is a bug.", node))?;
            component_id(fixed)
        }
        FrameTarget::Slot(slot) => frame
            .slot(*slot)
            .map(|slot| slot.name.to_string())
            .ok_or_else(|| generic_error!("The frame has no slot {}; this is a bug.", slot)),
        FrameTarget::Output(output) => outputs
            .get(output.as_str())
            .map(|output| output.component.clone())
            .ok_or_else(|| {
                generic_error!(
                    "Output `{}` is not a native metrics output; this is a lowering bug.",
                    output
                )
            }),
    }
}

#[cfg(test)]
mod differential_tests;
#[cfg(test)]
mod measurement_tests;
#[cfg(test)]
mod tests;

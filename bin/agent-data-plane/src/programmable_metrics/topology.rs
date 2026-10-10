//! The plan-driven metrics path: the part of the topology that the default program's physical frame describes.
//!
//! The frame, computed at boot from the default program, fixes the structure of the metrics path for the life of the
//! process (see the plan crate's `compile::frame`). [`PlanTopology`] builds it into the topology blueprint:
//!
//! - **Fixed components** are the sources, built exactly as natively, with the same component IDs and builders, by
//!   their own pipelines (which register their metrics outputs here); adding the DogStatsD pipeline also takes its
//!   context snapshot channel, for the bridge below. Each source output is an admission point, which admits from the
//!   one schedule the installer commits to. Every admission point registers in the installer's admission registry,
//!   through which cuts lock and poll it.
//! - **Slots** each run in one region host, named by [`slot_name`]: `dsd_pre_aggregation` (the DogStatsD mapper
//!   through the tag filterlist) and `metrics_enrich` (the post-aggregation filter through the policy region, whose
//!   gateways select what reaches each output, as one decision R1 never cuts), and the regions R1's cost model cuts
//!   from them so that a costly chain pipelines, such as the post-aggregation filter when it does not fit beside
//!   enrichment (`dsd_post_agg_filter_region`). A region host's outputs are its slot's exits, in order. Its input
//!   holds what the input queues of the native components it replaces held together (see
//!   [`region_input_capacity`]), so that a burst backs up to its source no sooner than natively.
//! - **Isolation slots** run a state owner in a region host of its own: DogStatsD aggregation as a stateful kernel, in
//!   `dsd_agg`, which keeps the native aggregation component's ID, health, resource group, and telemetry. Its region
//!   reads the schedule, a wall clock, and the host's budgets of state, and works through flushes in bounded steps.
//!   The admission points upstream of it send heartbeats toward it, and only toward it, so that its watermark advances
//!   while they are idle; the bridge of [`snapshot_bridge`](super::snapshot_bridge) answers aggregation's context
//!   snapshot requests from it.
//! - **Outputs** are the native encoders and forwarders, built exactly as natively, and the DogStatsD client telemetry
//!   tap, `dsd_client_telemetry_out`, which the `metrics_enrich` slot feeds. No gateway component stands in front of an
//!   encoder: the endpoint routing filters and the failover gateways are `split`s of the policy region, so a region
//!   exit feeds each encoder directly.
//! - **Edges** are the frame's: one blueprint edge per physical edge, from an admission point or a slot's exit to a
//!   fixed component, a slot, or an output's component. Exits the default program does not use are connected and stay
//!   idle, so that later generations can use them.
//!
//! The DogStatsD taps on `dsd_in.metrics` (`dsd_stats_out`, `dsd_debug_log_out`) and everything outside the metrics
//! path keep their native construction, in `run.rs`.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::shared::SharedConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use saluki_components::{
    destinations::DogStatsDClientTelemetryConfiguration,
    transforms::{aggregate_context_snapshot_channel, AggregateContextSnapshotHandle},
};
use saluki_core::{
    execution::{AdmissionPointConfig, AdmissionPointId, Budget, RegionHostBuilder, WorkKind, MIN_HEARTBEAT_PERIOD},
    topology::{ComponentId, TopologyBlueprint},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_plan::{
    compile::{FixedComponent, Frame, FrameSource, FrameTarget, RegionProgram, Slot, SlotKind},
    ir::{NativeComponentId, NodeId, Plan, Provenance},
    limits,
    operators::names,
    runtime::{build_kernels, MetricRegionDriver, PanicBudget, PrepareRegion, RegionBounds, RegionConfig},
};

use super::{
    bindings::routing_groups,
    installer::{BootTopology, ProgrammableMetrics},
    lowering::ids,
    outputs::{NativeOutputs, OutputKind},
    snapshot_bridge::SnapshotBridgeState,
};
use crate::cli::run::{autoscaling_failover_branch, main_metrics_encoder, mrf_metrics_branch, routing_group_encoder};

/// Names the region host of a slot of the default program's frame, after the native components it replaces.
///
/// The slot holding the DogStatsD mapper is `dsd_pre_aggregation`, and the slot holding host enrichment is
/// `metrics_enrich`, which keeps the native ID that the components feeding metrics enrichment have always been
/// connected to. The isolation slot of DogStatsD aggregation is `dsd_agg`, the native aggregation transform's ID. A
/// slot that R1's cost model cuts from one of them, so that a costly chain pipelines across regions, is named after the
/// native component it starts with: `<first node>_region`, without the `_in` of a merge that starts it, such as
/// `dsd_prefix_filter_region`.
///
/// # Errors
///
/// Returns an error for a slot without nodes, which no frame has.
pub fn slot_name(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    let has = |id: &str| nodes.iter().any(|node| node.to_string() == id);
    let name = if has(ids::DSD_AGG) {
        ids::DSD_AGG.to_string()
    } else if has(ids::DSD_ENRICH) {
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
    dogstatsd: bool,
}

impl<'a> PlanTopology<'a> {
    /// Creates the builder of the metrics path that `metrics` prepared.
    pub fn new(metrics: &'a mut ProgrammableMetrics) -> Self {
        Self {
            boot: metrics.boot_topology(),
            sources: BTreeSet::new(),
            dogstatsd: false,
        }
    }

    /// Adds the outputs of the frame, with their encoders and forwarders, built as natively.
    ///
    /// The gateways in front of them are part of the default program, which the regions run.
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
                    blueprint.add_encoder(component, routing_group_encoder(shared, policy))?;
                }
                OutputKind::MultiRegionFailover => {
                    let branch = mrf.take().ok_or_else(|| missing("Multi-Region Failover branch"))?;
                    let forwarder = output.forwarder.ok_or_else(|| missing("forwarder"))?;
                    blueprint
                        .add_encoder(component, branch.encoder)?
                        .add_forwarder(forwarder, branch.forwarder)?;
                }
                OutputKind::AutoscalingFailover => {
                    let branch = autoscaling
                        .take()
                        .ok_or_else(|| missing("autoscaling failover branch"))?;
                    let forwarder = output.forwarder.ok_or_else(|| missing("forwarder"))?;
                    blueprint
                        .add_encoder(component, branch.encoder)?
                        .add_forwarder(forwarder, branch.forwarder)?;
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

    /// Registers the DogStatsD source's metrics output and keeps aggregation's context snapshot requests; returns the
    /// handle of aggregation's context snapshots.
    ///
    /// Aggregation runs as a stateful kernel in the frame's `dsd_agg` isolation slot, whose region is added with the
    /// others (see [`finish`][Self::finish]): the snapshot requests are kept for the
    /// [snapshot bridge](ProgrammableMetrics::snapshot_bridge), which answers them from the region.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame has no aggregation slot, which is a lowering bug.
    pub fn add_dogstatsd(&mut self) -> Result<AggregateContextSnapshotHandle, GenericError> {
        self.dogstatsd = true;
        self.sources.insert(format!("{}.metrics", ids::DSD_IN));
        if aggregation_slot(&self.boot.frame).is_none() {
            return Err(generic_error!(
                "The DogStatsD pipeline is enabled, but the default program runs no aggregation in an isolation slot; \
                 this is a lowering bug."
            ));
        }
        let (snapshots, receiver) = aggregate_context_snapshot_channel();
        self.boot.snapshots = Some(SnapshotBridgeState::new(receiver)?);
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
            dogstatsd,
        } = self;
        check_fixed_components(&boot.frame, &sources, dogstatsd)?;
        add_regions(blueprint, boot)?;
        connect_frame(blueprint, boot)
    }
}

/// Adds a region host per slot of the frame, each starting with the boot generation's program for its slot, and with
/// the input capacity of the native components it replaces.
pub(super) fn add_regions(blueprint: &mut TopologyBlueprint, boot: &mut BootTopology) -> Result<(), GenericError> {
    let frame = Arc::clone(&boot.frame);
    let interconnect = blueprint.interconnect_capacity();
    for slot in frame.slots() {
        let capacity = region_input_capacity(&boot.plan, &frame, &boot.outputs, slot, interconnect)?;
        let host = region_host(boot, slot)?;
        blueprint
            .add_transform(slot.name.as_str(), host)?
            .with_component_input_capacity(slot.name.as_str(), capacity)?;
    }
    Ok(())
}

/// Returns the input capacity of a slot's region host: as many buffers as the native components the region replaces
/// held on the way through it, so that a burst backs up to its source no sooner than it did natively.
///
/// Natively, each of those components was a transform of its own, with an input interconnect of `interconnect`
/// buffers, and held one more buffer while it waited to send downstream. The region does their work in one transform,
/// with one input interconnect, and holds one buffer while it waits. On a path through the region's boot nodes that
/// runs through `n` of those components, `n * (interconnect + 1)` buffers waited natively, so the region's input holds
/// `n * (interconnect + 1) - 1`, for the path that runs through the most: no path holds fewer buffers than natively,
/// and a region that runs one chain of components holds exactly as many. A path runs through a component where it
/// enters the component's nodes, by their native provenance, from outside the region or from another component's
/// nodes. The components the plan-driven topology still builds are not replaced, and their interconnects remain: the
/// sources (the frame's fixed components), and the encoders and destinations of the outputs, which the `emit`s and the
/// client telemetry tap's fan-out are lowered from. Aggregation, which runs in its isolation slot `dsd_agg`, is one of
/// the components a region replaces.
///
/// The capacity follows the boot program: later generations run in the same regions.
///
/// # Errors
///
/// Returns an error if a slot's nodes form a cycle, or if the capacity overflows, both of which are bugs.
fn region_input_capacity(
    plan: &Plan, frame: &Frame, outputs: &NativeOutputs, slot: &Slot, interconnect: NonZeroUsize,
) -> Result<NonZeroUsize, GenericError> {
    let kept = frame
        .fixed()
        .iter()
        .map(component_id)
        .chain(outputs.iter().map(|output| output.component.clone()))
        .collect::<BTreeSet<_>>();
    // The native component that a node of the region does the work of, if the region replaces it.
    let replaced = |node: &NodeId| match plan.node(node).and_then(|node| node.provenance.as_ref()) {
        Some(Provenance::Native(component)) if !kept.contains(component.as_str()) => Some(component.as_str()),
        _ => None,
    };
    // Whether a path enters a replaced component at `to`, from `from` or, for `None`, from outside the region.
    let enters = |from: Option<&NodeId>, to: &NodeId| {
        let component = replaced(to);
        usize::from(component.is_some() && from.and_then(replaced) != component)
    };

    let in_slot = slot.nodes.iter().collect::<BTreeSet<_>>();
    let mut successors = BTreeMap::<&NodeId, Vec<&NodeId>>::new();
    let mut unvisited_predecessors = in_slot.iter().map(|node| (*node, 0usize)).collect::<BTreeMap<_, _>>();
    // The most replaced components a path runs through up to each node, starting with the paths from outside.
    let mut through = in_slot.iter().map(|node| (*node, 0usize)).collect::<BTreeMap<_, _>>();
    for edge in &plan.edges {
        let (from, to) = (&edge.from.node, &edge.to.node);
        if !in_slot.contains(to) {
            continue;
        }
        if in_slot.contains(from) {
            successors.entry(from).or_default().push(to);
            *unvisited_predecessors.entry(to).or_default() += 1;
        } else {
            through.insert(to, enters(None, to));
        }
    }

    let mut ready = unvisited_predecessors
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| *node)
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(node) = ready.pop_front() {
        visited += 1;
        let here = through[node];
        for next in successors.get(node).into_iter().flatten() {
            let candidate = here + enters(Some(node), next);
            let best = through.entry(next).or_default();
            *best = (*best).max(candidate);
            let count = unvisited_predecessors.entry(next).or_default();
            *count -= 1;
            if *count == 0 {
                ready.push_back(next);
            }
        }
    }
    if visited != in_slot.len() {
        return Err(generic_error!(
            "The nodes of region `{}` form a cycle; this is a bug.",
            slot.name
        ));
    }

    let components = through.values().copied().max().unwrap_or_default();
    if components == 0 {
        return Ok(interconnect);
    }
    interconnect
        .get()
        .checked_add(1)
        .and_then(|per_component| components.checked_mul(per_component))
        .and_then(|held| NonZeroUsize::new(held - 1))
        .ok_or_else(|| {
            generic_error!(
                "The input capacity of region `{}` overflows: it replaces {} native components of {} buffers each; \
                 this is a bug.",
                slot.name,
                components,
                interconnect
            )
        })
}

/// Connects every physical edge of the frame, and marks every admission point, reading the boot schedule.
///
/// Every admission point registers in the installer's admission registry. A point upstream of a state owner sends
/// heartbeats while it is idle, every [`heartbeat_period`], to the component its route leads to and to no other, so
/// that the state owner's watermark advances, while the DogStatsD taps on the same output keep their native traffic.
pub(super) fn connect_frame(blueprint: &mut TopologyBlueprint, boot: &BootTopology) -> Result<(), GenericError> {
    let frame = &boot.frame;
    for edge in frame.edges() {
        let from = source_output(frame, &edge.from)?;
        let to = target_component(frame, &boot.outputs, &edge.to)?;
        blueprint.connect_components(from.as_str(), to.as_str())?;
    }

    for point in frame.admission_points() {
        let output = admission_output(frame, &point.node)?;
        let mut config = AdmissionPointConfig::new(
            point.id,
            point.mode,
            Arc::clone(&boot.schedule),
            limits::values::CLOCK_RESET_THRESHOLD,
        )
        .with_registry(boot.admission.clone());
        if point.feeds_state {
            let route = point.route.as_ref().ok_or_else(|| {
                generic_error!(
                    "Admission point `{}` feeds a state owner, but the frame routes it nowhere; this is a bug.",
                    output
                )
            })?;
            let recipient = target_component(frame, &boot.outputs, route)?;
            let recipient = ComponentId::try_from(recipient.as_str()).map_err(|error| {
                generic_error!(
                    "The heartbeats of admission point `{}` go to `{}`, which is not a valid component ID: {}",
                    output,
                    recipient,
                    error
                )
            })?;
            config = config
                .with_heartbeat(heartbeat_period(frame, &boot.programs, point.id))
                .with_heartbeat_recipients([recipient]);
            #[cfg(test)]
            if let Some(clock) = &boot.state_admission_clock {
                config = config.with_wall_clock(clock.clone());
            }
        }
        blueprint.with_admission_point(output, config)?;
    }
    Ok(())
}

/// Returns the heartbeat period of the admission point `point`: [`HEARTBEAT_PERIOD`][limits::values::HEARTBEAT_PERIOD],
/// or the shortest flush interval of the boot program's state owners downstream of the point, if shorter, so that an
/// idle point never delays a flush; at least [`MIN_HEARTBEAT_PERIOD`].
///
/// The period follows the boot program, as the frame does: a later generation whose state owners flush more often than
/// that may close an idle bucket up to one period later than its flush interval, never wrongly.
fn heartbeat_period(frame: &Frame, programs: &[Arc<RegionProgram>], point: AdmissionPointId) -> Duration {
    frame
        .slots()
        .iter()
        .filter(|slot| matches!(&slot.kind, SlotKind::Isolation { upstream, .. } if upstream.contains(&point)))
        .filter_map(|slot| programs.get(slot.id.get()))
        .flat_map(|program| program.kernels().iter())
        .filter_map(|kernel| kernel.lineage.as_ref().map(|lineage| lineage.flush_interval))
        .fold(limits::values::HEARTBEAT_PERIOD, Duration::min)
        .max(MIN_HEARTBEAT_PERIOD)
}

/// Checks that the native pipelines built every fixed component of the frame, that the frame runs DogStatsD
/// aggregation in its isolation slot exactly when the DogStatsD pipeline is enabled, and that the frame reads every
/// output that carries metrics into the metrics path.
fn check_fixed_components(frame: &Frame, sources: &BTreeSet<String>, dogstatsd: bool) -> Result<(), GenericError> {
    let mut read = BTreeSet::new();
    for fixed in frame.fixed() {
        match fixed.operator.as_str() {
            names::SOURCE => {
                read.insert(admission_output(frame, &fixed.node)?);
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
    let aggregation = aggregation_slot(frame).map(|slot| slot.id);
    if dogstatsd && aggregation.is_none() {
        return Err(generic_error!(
            "The DogStatsD pipeline is enabled, but the default program runs no aggregation in an isolation slot; this \
             is a lowering bug."
        ));
    }
    for slot in frame.slots().iter().filter(|slot| slot.is_isolation()) {
        if !dogstatsd || aggregation != Some(slot.id) {
            return Err(generic_error!(
                "The default program runs `{}` in an isolation slot, which only DogStatsD aggregation runs in, with \
                 the DogStatsD pipeline enabled; this is a lowering bug.",
                slot.name
            ));
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
    // An isolation slot accounts the state owner it is named after in its own group, with its bounds at its top level,
    // as the native component it stands for did, and bounds one drain step.
    let bounds = RegionBounds::for_slot(&boot.plan, &boot.registry, &program, slot)?;
    let work_per_turn = usize::try_from(limits::values::WORK_PER_TURN)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| generic_error!("The work per turn of a region must be a positive number of batches."))?;
    let exits = slot.exits.iter().map(|exit| exit.name.to_string()).collect::<Vec<_>>();

    let generation = boot.generation.clone();
    let telemetry = boot.telemetry.clone();
    let failures = Arc::clone(&boot.failures);
    // A state owner's region also reads the schedule, to tell which generation its flushes belong to, a wall clock, and
    // the budgets of state its lineages draw on.
    let state = slot.is_isolation().then(|| {
        (
            Arc::clone(&boot.schedule),
            Arc::clone(&boot.wall_clock),
            boot.state_budgets.clone(),
        )
    });
    let isolation = state.is_some();
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
        let mut config = RegionConfig::for_slot(&slot, &telemetry).with_failure_reporter(Arc::clone(&failures));
        if let Some((schedule, wall_clock, budgets)) = &state {
            config = config
                .with_schedule(Arc::clone(schedule))
                .with_wall_clock(Arc::clone(wall_clock))
                .with_state_budgets(budgets.clone());
        }
        Ok(MetricRegionDriver::new(config, prepare)?)
    })?;
    // A flush emits what a state owner holds: an isolation slot works through it in bounded steps, between which it
    // answers control messages and liveness probes.
    let host = if isolation {
        host.with_turn_budget(drain_step_budget())
    } else {
        host
    };
    Ok(host.with_bounds(bounds))
}

/// Returns the work budget of each call into an isolation slot's driver: one step of a flush.
fn drain_step_budget() -> Budget {
    Budget::unlimited()
        .with_limit(WorkKind::Events, limits::values::DRAIN_STEP_EVENTS)
        .with_limit(WorkKind::StateWork, limits::values::DRAIN_STEP_CONTEXTS)
}

/// Returns the isolation slot that runs DogStatsD aggregation, if the frame has one: it does exactly when DogStatsD is
/// enabled.
pub(super) fn aggregation_slot(frame: &Frame) -> Option<&Slot> {
    frame
        .slots()
        .iter()
        .find(|slot| slot.is_isolation() && slot.nodes.iter().any(|node| node.to_string() == ids::DSD_AGG))
}

/// Returns the component ID a fixed component runs as: its node's ID, as each source has its native component's ID.
fn component_id(fixed: &FixedComponent) -> String {
    fixed.node.local().to_string()
}

/// Returns the component output an admission point is: a source's port is its output of the same name, and the one
/// port of a host component, which no default program has since aggregation runs in its isolation slot, its default
/// output.
pub(super) fn admission_output(frame: &Frame, node: &NodeId) -> Result<String, GenericError> {
    let fixed = frame
        .fixed_component(node)
        .ok_or_else(|| generic_error!("The frame has no fixed component `{}`; this is a bug.", node))?;
    let component = component_id(fixed);
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
            Ok(component_id(fixed))
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
mod burst_tests;
#[cfg(test)]
mod differential_tests;
#[cfg(test)]
mod isolation_tests;
#[cfg(test)]
mod measurement_tests;
#[cfg(test)]
mod tests;

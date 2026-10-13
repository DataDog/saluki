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
//! - **Hosts** are fixed at boot, one encode host and one delivery host per fixed delivery host of the process
//!   ([`FixedHosts`](super::resources::FixedHosts)): `dd_metrics_encode` and `dd_out`, `mrf_metrics_encode` and
//!   `mrf_dd_out`, and, when a Cluster Agent endpoint resolves at boot, `af_metrics_encode` and `cluster_agent_out`.
//!   Every output bound to a delivery host runs as a lease in its encode host, and every recipient as an I/O loop in
//!   the delivery host, which the installer prepares, publishes, drains, and retires while they run; the hosts start
//!   with what the boot generation needs, so that `dd_out` routes every pipeline's payloads from the first one. A host
//!   with nothing at boot is idle until the installer prepares something in it. The DogStatsD client telemetry tap,
//!   `dsd_client_telemetry_out`, which the `metrics_enrich` slot feeds, is the one fixed destination. No gateway
//!   component stands in front of an encode host: the endpoint routing filters and the failover gateways are `split`s
//!   of the policy region, whose exits name the lease of each output they emit to in their batches' headers.
//! - **Edges** are the frame's: one blueprint edge per physical edge, from an admission point or a slot's exit to a
//!   fixed component, a slot, or an output's component. Exits the default program does not use are connected and stay
//!   idle, so that later generations can use them.
//!
//! - **Lanes.** On lanes ([`LaneCounts::lanes_on`](super::lanes::LaneCounts::lanes_on)), every slot runs the lanes the
//!   frame gives it ([`Slot::lanes`]): one task per lane inside the slot's one component, each with its own driver,
//!   kernels (accounted under `<region>.lane_<i>`), input of its share of the slot's capacity, and statistics of its
//!   work, which the `plan_lane_*` series publish. The DogStatsD source's metrics split into one output per producer
//!   lane, `dsd_in.metrics_<d>` (`metrics` with one), each feeding its lane of `dsd_pre_aggregation`; the lane, not the
//!   source's dispatcher, stamps its own admission point, every lane's point sharing one source clock. Every edge into
//!   a slot on several lanes routes as the frame says: by key into aggregation's state lanes, by the sender's lane
//!   from the producer lanes, and spread over the lanes from aggregation and the other sources. Without lanes, the
//!   metrics path is laid out as before lanes existed.
//!
//! The DogStatsD taps on the source's metrics outputs (`dsd_stats_out`, `dsd_debug_log_out`) and everything outside the
//! metrics path keep their native construction, in `run.rs`.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::{NonZeroU16, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::shared::SharedConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use saluki_components::{
    destinations::DogStatsDClientTelemetryConfiguration,
    encoders::encode_host::MetricsEncodeHostConfiguration,
    forwarders::{
        delivery::{BootPublication, DeliveryHostConfiguration},
        ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration,
    },
    sources::dogstatsd_metric_lane_output,
    transforms::{aggregate_context_snapshot_channel, AggregateContextSnapshotHandle},
};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder},
    execution::{
        AdmissionClock, AdmissionPointConfig, AdmissionPointId, Budget, LaneContext, RegionHostBuilder, SourceClock,
        SystemWallClock, WallClock, WorkKind, MIN_HEARTBEAT_PERIOD,
    },
    support::SubsystemIdentifier,
    topology::{ComponentId, LaneEdge, TopologyBlueprint},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    compile::{
        AdmissionPointInfo, ExitRouting, FixedComponent, Frame, FrameSource, FrameTarget, PointRouting, RegionProgram,
        Slot, SlotId, SlotKind,
    },
    ir::{NativeComponentId, NodeId, Plan, Provenance, Resources},
    limits,
    operators::{names, ConfiguredOperator, LaneBinding, LaneCell, OperatorRegistry},
    runtime::{
        build_kernels, kernel_accounting_in, state::lanes::SlotLanes, MetricRegionDriver, PanicBudget, PrepareRegion,
        RegionBounds, RegionConfig, RoutingTelemetry,
    },
};
use saluki_metrics::{Counter, Gauge};

use super::{
    installer::{BootTopology, HostWiring, ProgrammableMetrics},
    lowering::ids,
    resources::{
        specs::{host_settings, lease_spec, publication, recipient_settings},
        FixedHost, NativeOutputs, OutputKind, CLUSTER_AGENT_OUT, DD_OUT, MRF_DD_OUT,
    },
    snapshot_bridge::SnapshotBridgeState,
    telemetry::{lane_group_name, SlotLaneStats},
};
use crate::cli::run::{autoscaling_failover_wired, mrf_metrics_endpoint};

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
/// `run.rs` calls it in the order it builds the topology: [`dd_forwarder`][Self::dd_forwarder] for `dd_out`, which
/// every pipeline sends through, [`add_outputs`][Self::add_outputs] before the pipelines that feed the metrics path,
/// [`add_source`][Self::add_source] for each of their metrics outputs, [`add_dogstatsd`][Self::add_dogstatsd] with the
/// DogStatsD pipeline, and [`finish`][Self::finish] last.
pub struct PlanTopology<'a> {
    boot: &'a mut BootTopology,
    sources: BTreeSet<String>,
    dogstatsd: bool,

    /// The encode host of each delivery host configured so far, which [`add_outputs`][Self::add_outputs] adds.
    encode_hosts: Vec<(FixedHost, MetricsEncodeHostConfiguration)>,
}

impl<'a> PlanTopology<'a> {
    /// Creates the builder of the metrics path that `metrics` prepared.
    pub fn new(metrics: &'a mut ProgrammableMetrics) -> Self {
        Self {
            boot: metrics.boot_topology(),
            sources: BTreeSet::new(),
            dogstatsd: false,
            encode_hosts: Vec::new(),
        }
    }

    /// Returns the configuration of `dd_out`: the delivery host that every pipeline's payloads go through, which the
    /// installer controls, and which routes from its first payload with what the boot generation needs.
    ///
    /// Its encode host, `dd_metrics_encode`, is added with the other outputs.
    ///
    /// # Errors
    ///
    /// Returns an error if `dd_out` was configured already, or if what the boot generation needs in it cannot be read,
    /// which is a lowering bug.
    pub fn dd_forwarder(
        &mut self, shared: &SharedConfiguration, config_system: &ConfigurationSystem,
    ) -> Result<DatadogForwarderConfiguration, GenericError> {
        let host = self.configure_host(DD_OUT)?;
        let secrets = config_system.live(|config| &config.shared.secrets);
        Ok(DatadogForwarderConfiguration::controlled(shared, secrets, host))
    }

    /// Adds the encode hosts of the frame, the failover delivery hosts, and the DogStatsD client telemetry
    /// destination.
    ///
    /// The gateways in front of the encode hosts are part of the default program, which the regions run.
    ///
    /// # Errors
    ///
    /// Returns an error if a component cannot be configured, if `dd_out` was not configured first, or if the frame and
    /// the native outputs disagree, which is a lowering bug.
    pub fn add_outputs(
        &mut self, blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    ) -> Result<(), GenericError> {
        let startup = config_system.config();
        // Native wiring warns when a failover branch is requested but cannot be wired. Their delivery hosts are built
        // whatever the branches resolve to: a branch that appears later is a recipient and a lease the installer
        // prepares in them.
        mrf_metrics_endpoint(&startup.domains.multi_region_failover);
        let endpoint = self.boot.facts.cluster_agent_endpoint.clone();
        let endpoint_at_startup = endpoint.is_some();
        let cluster_agent = endpoint
            .filter(|_| shared.cluster_agent.enabled)
            .zip(shared.cluster_agent.auth_token.clone());
        autoscaling_failover_wired(shared, cluster_agent.as_ref(), endpoint_at_startup);

        if !self.encode_hosts.iter().any(|(host, _)| host.delivery == DD_OUT) {
            return Err(generic_error!(
                "The metrics outputs are added before `dd_out` was configured; this is a bug."
            ));
        }
        let failover = self
            .boot
            .hosts_at_boot
            .iter()
            .filter(|host| host.delivery != DD_OUT)
            .collect::<Vec<_>>();
        for host in failover {
            let configured = self.configure_host(host.delivery)?;
            match host.delivery {
                MRF_DD_OUT => {
                    let secrets = config_system.live(|config| &config.shared.secrets);
                    blueprint.add_forwarder(
                        host.delivery,
                        DatadogForwarderConfiguration::controlled(shared, secrets, configured),
                    )?;
                }
                CLUSTER_AGENT_OUT => {
                    blueprint.add_forwarder(
                        host.delivery,
                        ClusterAgentForwarderConfiguration::controlled(shared, configured),
                    )?;
                }
                other => {
                    return Err(generic_error!(
                        "The process has a delivery host `{}` the plan-driven path cannot build; this is a bug.",
                        other
                    ))
                }
            }
        }
        // Each encode host feeds its delivery host: `dd_out`, shared with the other pipelines, or its own.
        for (host, encoder) in std::mem::take(&mut self.encode_hosts) {
            blueprint
                .add_encoder(host.encoder, encoder)?
                .connect_components(host.encoder, host.delivery)?;
        }

        let outputs = &self.boot.outputs;
        for decl in self.boot.frame.outputs().iter().filter(|decl| decl.binding.is_none()) {
            let output = outputs.get(decl.id.as_str()).ok_or_else(|| {
                generic_error!(
                    "The default program delivers to output `{}`, which is not a native metrics output; this is a \
                     lowering bug.",
                    decl.id
                )
            })?;
            match &output.kind {
                OutputKind::ClientTelemetry => {
                    blueprint.add_destination(output.component.as_str(), DogStatsDClientTelemetryConfiguration)?;
                }
                _ => {
                    return Err(generic_error!(
                        "The default program delivers to output `{}` as a fixed destination, but it is an encoder \
                         stream, which runs as a lease of an encode host; this is a lowering bug.",
                        decl.id
                    ))
                }
            }
        }
        Ok(())
    }

    /// Configures the delivery host `delivery` and its encode host from their wiring: each starts with what the boot
    /// generation needs in it (the encode host its leases, the delivery host its recipients, native set, and settings,
    /// as published at the boot revision), and exposes its factory and its status to the installer.
    ///
    /// Returns the delivery host's configuration; the encode host's waits for [`add_outputs`][Self::add_outputs].
    fn configure_host(&mut self, delivery: &str) -> Result<DeliveryHostConfiguration, GenericError> {
        let wiring = self.boot.hosts.take(delivery).ok_or_else(|| {
            generic_error!(
                "Delivery host `{}` was configured already, or the process has none; this is a bug.",
                delivery
            )
        })?;
        let (encoder, delivery) = configure_wired_host(wiring, &self.boot.plan.resources)?;
        #[cfg(test)]
        let (encoder, delivery) = match &self.boot.host_doubles {
            Some(doubles) => doubles.apply(encoder, delivery),
            None => (encoder, delivery),
        };
        self.encode_hosts.push(encoder);
        Ok(delivery)
    }

    /// Returns the number of lanes the DogStatsD source's metrics split into, which its configuration takes
    /// ([`with_metric_lanes`]): one per producer lane of the frame, each a component output that its producer lane
    /// stamps.
    ///
    /// [`with_metric_lanes`]: saluki_components::sources::DogStatsDConfiguration::with_metric_lanes
    pub fn dogstatsd_metric_lanes(&self) -> NonZeroU16 {
        dogstatsd_metric_lanes(&self.boot.frame)
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
            encode_hosts,
        } = self;
        if let Some((host, _)) = encode_hosts.first() {
            return Err(generic_error!(
                "The encode host of `{}` was configured but never added; this is a bug.",
                host.delivery
            ));
        }
        check_fixed_components(&boot.frame, &sources, dogstatsd)?;
        add_regions(blueprint, boot)?;
        connect_frame(blueprint, boot)?;
        boot.hosts.close_unbuilt();
        Ok(())
    }
}

/// What stands in for the edges of the encode and delivery hosts in tests: each encode host shows every buffer it
/// routes to an observer, by output, and may wait for a gate before it reads its input; each delivery host's
/// recipients send nothing, and count the events they would have sent, unless they send as in production. The hosts
/// themselves, with their control, run as in production.
#[cfg(test)]
pub struct HostDoubles {
    /// What sees every buffer an encode host routes to a lease.
    pub observer: Arc<dyn saluki_components::encoders::encode_host::LeaseObserver>,

    /// Whether the leases encode what the observer saw, or the encode hosts drop it.
    pub encode: bool,

    /// The events the delivery hosts' recipients took, in all, if they send nothing; recipients that send as in
    /// production otherwise.
    pub delivered: Option<Arc<std::sync::atomic::AtomicU64>>,

    /// The gate each encode host waits on before it reads its input, by encode host component ID.
    pub gates: BTreeMap<String, Arc<tokio::sync::Semaphore>>,
}

#[cfg(test)]
impl HostDoubles {
    /// Makes a host's encode host and delivery host run with these doubles.
    fn apply(
        &self, (host, encoder): (FixedHost, MetricsEncodeHostConfiguration), delivery: DeliveryHostConfiguration,
    ) -> ((FixedHost, MetricsEncodeHostConfiguration), DeliveryHostConfiguration) {
        let mut encoder = encoder.with_observer(Arc::clone(&self.observer), self.encode);
        if let Some(gate) = self.gates.get(host.encoder) {
            encoder = encoder.with_input_gate(Arc::clone(gate));
        }
        let delivery = match &self.delivered {
            Some(delivered) => delivery.with_discarding_recipients(Arc::clone(delivered)),
            None => delivery,
        };
        ((host, encoder), delivery)
    }
}

/// Adds every fixed host of the boot topology to `blueprint`, each encode host feeding its delivery host, as the
/// plan-driven path adds them, with the doubles of the boot topology: for tests that build the rest of the metrics path
/// themselves.
///
/// # Errors
///
/// Returns an error if a host was configured already, or cannot be configured.
#[cfg(test)]
pub(super) fn add_hosts(
    blueprint: &mut TopologyBlueprint, boot: &mut BootTopology, shared: &SharedConfiguration,
    secrets: agent_data_plane_config::Live<agent_data_plane_config::shared::Secrets>,
) -> Result<(), GenericError> {
    super::test_support::init_test_tls();
    for host in boot.hosts_at_boot.iter() {
        let wiring = boot
            .hosts
            .take(host.delivery)
            .ok_or_else(|| generic_error!("Delivery host `{}` was configured already.", host.delivery))?;
        let configured = configure_wired_host(wiring, &boot.plan.resources)?;
        let ((host, encoder), delivery) = match &boot.host_doubles {
            Some(doubles) => doubles.apply(configured.0, configured.1),
            None => configured,
        };
        match host.delivery {
            CLUSTER_AGENT_OUT => {
                blueprint.add_forwarder(
                    host.delivery,
                    ClusterAgentForwarderConfiguration::controlled(shared, delivery),
                )?;
            }
            _ => {
                blueprint.add_forwarder(
                    host.delivery,
                    DatadogForwarderConfiguration::controlled(shared, secrets.clone(), delivery),
                )?;
            }
        }
        blueprint
            .add_encoder(host.encoder, encoder)?
            .connect_components(host.encoder, host.delivery)?;
    }
    Ok(())
}

/// Configures a delivery host and its encode host from their wiring, each starting with what the boot generation
/// needs in it, as `resources`, the boot plan's resources, declares it: the encode host the boot leases, and the
/// delivery host the boot publication's recipients (with those the boot generation creates), native set, settings,
/// and withdrawn set. The installer gets each host's factory, and the delivery host's status.
///
/// # Errors
///
/// Returns an error if a boot lease, recipient, or the host's settings cannot be read, which is a lowering bug.
pub(super) fn configure_wired_host(
    wiring: HostWiring, resources: &Resources,
) -> Result<((FixedHost, MetricsEncodeHostConfiguration), DeliveryHostConfiguration), GenericError> {
    let HostWiring {
        host,
        delivery,
        encode,
        events,
        boot_prepare,
        boot_publish,
        boot_revision,
        factories,
        credentials,
    } = wiring;
    let mut encoder = MetricsEncodeHostConfiguration::new().with_control_mailbox(encode);
    for lease in boot_prepare.iter().flat_map(|prepare| &prepare.leases) {
        let spec = lease_spec(lease, resources).error_context(format!(
            "The boot lease of output `{}` cannot be read.",
            lease.output.id
        ))?;
        encoder = encoder.with_lease(spec);
    }

    let binding = resources.host(&host.id()).ok_or_else(|| {
        generic_error!(
            "The boot plan declares no settings for delivery host `{}`; this is a lowering bug.",
            host.delivery
        )
    })?;
    let settings = Arc::new(host_settings(binding)?);
    let mut boot = match &boot_publish {
        Some(publish) => {
            // No resources artifact is delivered before boot, so every boot recipient is native and has no name.
            let published = publication(boot_revision, publish, |_| None)?;
            BootPublication {
                revision: published.revision,
                settings: published.settings,
                recipients: published.recipients,
                native_set: published.native_set,
                withdrawn: published.withdrawn,
            }
        }
        None => BootPublication::empty(settings),
    };
    // A recipient the boot generation creates that the boot publication does not set runs from the start too.
    for create in boot_prepare.iter().flat_map(|prepare| &prepare.create) {
        let created = recipient_settings(create, None)?;
        if !boot.recipients.iter().any(|known| known.recipient == created.recipient) {
            boot.recipients.push(created);
        }
    }

    let delivery = DeliveryHostConfiguration::new(delivery, credentials, boot)
        .with_events(events)
        .with_drain_deadline(limits::values::DRAIN_DEADLINE);
    // Each slot is set once per host, and this host is configured once.
    let _ = factories.leases.set(encoder.lease_factory());
    let _ = factories.recipients.set(delivery.factory());
    let _ = factories.status.set(delivery.status());
    Ok(((host, encoder), delivery))
}

/// Adds a region host per slot of the frame, each starting with the boot generation's program for its slot, and with
/// the input capacity of the native components it replaces.
///
/// With lanes ([`LaneCounts::lanes_on`](super::lanes::LaneCounts::lanes_on)), every slot runs its lanes
/// ([`Slot::lanes`]), each lane with its own driver and kernels and with statistics of its own work, which
/// [`BootTopology::lane_stats`] keeps; and the DogStatsD source's producer lanes stamp its admission points themselves
/// (see [`connect_frame`]).
pub(super) fn add_regions(blueprint: &mut TopologyBlueprint, boot: &mut BootTopology) -> Result<(), GenericError> {
    let frame = Arc::clone(&boot.frame);
    let interconnect = blueprint.interconnect_capacity();
    let (owner, mut owned) = owned_lane_points(boot).unzip();
    let mut lane_stats = Vec::new();
    for slot in frame.slots() {
        let native = region_input_capacity(&boot.plan, &frame, &boot.outputs, slot, interconnect)?;
        let capacity = lane_input_capacity(slot, native);
        let admission = if owner == Some(slot.id) { owned.take() } else { None };
        let stats = boot.lanes.lanes_on().then(|| {
            let routes = slot
                .exits
                .iter()
                .any(|exit| matches!(exit.routing, ExitRouting::Keyed(_)));
            let holds_state = boot
                .programs
                .get(slot.id.get())
                .is_some_and(|program| program.kernels().iter().any(|kernel| kernel.lineage.is_some()));
            SlotLaneStats::new(slot.name.as_str(), slot.lanes, routes, holds_state)
        });
        let mut host = region_host(boot, slot, admission, stats.as_ref())?;
        if let Some(events) = routed_buffer_events(&frame, slot) {
            host = host.with_input_buffer_events(events);
        }
        blueprint
            .add_transform(slot.name.as_str(), host)?
            .with_component_input_capacity(slot.name.as_str(), capacity)?;
        lane_stats.extend(stats);
    }
    if let (Some(slot), Some(_)) = (owner, owned) {
        return Err(generic_error!(
            "The DogStatsD source's admission points lead to slot {}, which the frame does not have; this is a bug.",
            slot
        ));
    }
    boot.lane_stats = lane_stats;
    Ok(())
}

/// Returns the capacity of each lane's input of a slot whose region's input holds `native` buffers on one lane.
///
/// A slot on several lanes holds what its region holds on one lane, in all: a stateless slot `ceil(native / lanes)`
/// buffers per lane; the lanes of a state owner's slot, fed buffers routed per key of [`routed_buffer_events`] events
/// each, `ceil(native × 1,024 / (lanes × events))` buffers per lane, so that the events allocated in its inputs, and
/// the firm bound of its interconnects, keep their native totals, rounding aside.
fn lane_input_capacity(slot: &Slot, native: NonZeroUsize) -> NonZeroUsize {
    let lanes = usize::from(slot.lanes.max(1));
    if lanes == 1 {
        return native;
    }
    let events = if slot.is_isolation() {
        limits::routed_buffer_events(slot.lanes)
    } else {
        limits::values::BATCH_SIZE
    };
    let held = native.get().saturating_mul(limits::values::BATCH_SIZE);
    let per_lane = held.div_ceil(lanes.saturating_mul(events));
    NonZeroUsize::new(per_lane).unwrap_or(NonZeroUsize::MIN)
}

/// Returns the events each buffer sent to `slot` holds at most, if fewer than an events buffer holds: those of buffers
/// routed per key ([`limits::routed_buffer_events`], which the keyed exits fill to), for the lanes of a state owner's
/// slot that keyed exits feed.
fn routed_buffer_events(frame: &Frame, slot: &Slot) -> Option<usize> {
    let keyed = frame
        .slots()
        .iter()
        .flat_map(|sender| &sender.exits)
        .any(|exit| exit.target == FrameTarget::Slot(slot.id) && matches!(exit.routing, ExitRouting::Keyed(_)));
    let events = limits::routed_buffer_events(slot.lanes);
    (keyed && slot.lanes > 1 && events < limits::values::BATCH_SIZE).then_some(events)
}

/// Returns the admission point of the DogStatsD source's metrics, if the frame has one: the point the source's metrics
/// lanes split, when the metrics path runs on lanes.
fn dogstatsd_point(frame: &Frame) -> Option<&AdmissionPointInfo> {
    frame
        .admission_points()
        .iter()
        .find(|point| point.node.local().as_str() == ids::DSD_IN && point.port.as_str() == "metrics")
}

/// Returns `true` if the producer lanes stamp `point` themselves, each lane its own point, rather than the source's
/// dispatcher: the DogStatsD source's metrics, whenever the metrics path runs on lanes and they lead to a slot, whose
/// lanes they are.
fn lanes_own(boot: &BootTopology, point: &AdmissionPointInfo) -> bool {
    boot.lanes.lanes_on()
        && matches!(point.route, Some(FrameTarget::Slot(_)))
        && dogstatsd_point(&boot.frame).is_some_and(|dogstatsd| dogstatsd.id == point.id)
}

/// Returns the slot whose lanes stamp the DogStatsD source's points, and the configuration of each lane's point, in
/// lane order, when the metrics path runs on lanes.
///
/// Every lane's point shares one clock, `dsd_in`'s, so that the source has one latest admission time, one epoch, and
/// one reset decision. A point that feeds a state owner stamps heartbeats while it is idle, every
/// [`heartbeat_period`], and its lane forwards them; with several lanes, also
/// [`PROGRESS_PERIOD`][limits::values::PROGRESS_PERIOD] into each second it stamps nothing in, since a state lane's
/// watermark is the minimum over the lanes' points, and a tick waits for it to reach the tick's second: an idle lane
/// would otherwise hold every flush of the busy ones back by up to its period.
fn owned_lane_points(boot: &BootTopology) -> Option<(SlotId, Vec<AdmissionPointConfig>)> {
    let point = dogstatsd_point(&boot.frame).filter(|point| lanes_own(boot, point))?;
    let Some(FrameTarget::Slot(slot)) = point.route else {
        return None;
    };
    let wall: Arc<dyn WallClock> = Arc::new(SystemWallClock);
    #[cfg(test)]
    let wall: Arc<dyn WallClock> = match &boot.state_admission_clock {
        Some(clock) if point.feeds_state => Arc::new(clock.clone()),
        _ => wall,
    };
    let clock = SourceClock::named(
        AdmissionClock::with_wall_clock(limits::values::CLOCK_RESET_THRESHOLD, wall),
        ids::DSD_IN,
    );
    let several = point.lane_ids().nth(1).is_some();
    let points = point
        .lane_ids()
        .map(|id| {
            let mut config = AdmissionPointConfig::new(
                id,
                point.mode,
                Arc::clone(&boot.schedule),
                limits::values::CLOCK_RESET_THRESHOLD,
            )
            .with_registry(boot.admission.clone())
            .owned()
            .with_source_clock(Arc::clone(&clock));
            if point.feeds_state {
                config = config.with_heartbeat(heartbeat_period(&boot.frame, &boot.programs, id));
                if several {
                    config = config.with_heartbeat_each_second(limits::values::PROGRESS_PERIOD);
                }
            }
            config
        })
        .collect();
    Some((slot, points))
}

/// The memory bounds of the drivers of a slot's lanes: one lane's bounds per lane, each under the lane's own
/// subcomponent, `lane_<i>`, as the lane's kernels are accounted under its resource group; except the state of the
/// slot's state owners, which their lanes partition and the host's state budget bounds in all.
///
/// That state is declared once, where the slot on one lane declares it: under the subcomponent its kernel is accounted
/// under, or at the slot's own level for the state owner the slot is named after, as aggregation's is at
/// `dsd_agg.aggregate_context_limit`, which the sizing guide sets. Every lane, the first included, then declares the
/// rest of one lane's bounds.
struct LaneRegionBounds {
    bounds: RegionBounds,
    lanes: u16,
    /// For a state owner's slot on several lanes, its state owners' state, declared once, and the bytes of one lane's
    /// bounds without it: the minimum, and the firm beyond the minimum.
    partitioned: Option<(StateBounds, usize, usize)>,
}

impl LaneRegionBounds {
    /// Returns the bounds of the lanes of `slot`, one lane of which has `bounds`, and whose state owners keep
    /// `state`.
    fn new(bounds: RegionBounds, slot: &Slot, state: StateBounds) -> Self {
        let partitioned = (slot.lanes > 1 && !state.0.is_empty()).then(|| {
            let (minimum, firm) = bytes_of(&bounds);
            let (state_minimum, state_firm) = bytes_of(&state);
            let stateless_minimum = minimum.saturating_sub(state_minimum);
            let stateless_firm = firm
                .saturating_sub(minimum)
                .saturating_sub(state_firm.saturating_sub(state_minimum));
            (state, stateless_minimum, stateless_firm)
        });
        Self {
            bounds,
            lanes: slot.lanes,
            partitioned,
        }
    }
}

impl MemoryBounds for LaneRegionBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        if self.lanes <= 1 {
            self.bounds.specify_bounds(builder);
            return;
        }
        if let Some((state, _, _)) = &self.partitioned {
            state.specify_bounds(builder);
        }
        for lane in 0..self.lanes {
            let mut lane_builder = builder.subcomponent(lane_group_name(LaneContext {
                index: lane,
                count: self.lanes,
            }));
            match &self.partitioned {
                Some((_, minimum, firm)) => {
                    lane_builder
                        .minimum()
                        .with_fixed_amount("lane driver without partitioned state", *minimum);
                    lane_builder
                        .firm()
                        .with_fixed_amount("lane driver without partitioned state", *firm);
                }
                None => self.bounds.specify_bounds(&mut lane_builder),
            }
        }
    }
}

/// The bounds of the state that state owners keep in entries drawn from the host's state budget, each under the
/// subcomponent its kernel is accounted under, or at the region's own level.
struct StateBounds(Vec<(ConfiguredOperator, Option<String>)>);

impl MemoryBounds for StateBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for (operator, accounted) in &self.0 {
            match accounted {
                Some(name) => operator.specify_state_bounds(&mut builder.subcomponent(name)),
                None => operator.specify_state_bounds(builder),
            }
        }
    }
}

/// Returns the minimum and firm bytes `bounds` declare, the firm bytes including the minimum.
fn bytes_of(bounds: &dyn MemoryBounds) -> (usize, usize) {
    let registry = ComponentRegistry::default();
    let id = SubsystemIdentifier::from_dotted("bounds");
    bounds.specify_bounds(&mut registry.bounds_builder(&id));
    let bounds = registry.as_bounds();
    (bounds.total_minimum_required_bytes(), bounds.total_firm_limit_bytes())
}

/// Returns the state of the state owners of `program`, a program of `plan` that `slot` runs, configured with their
/// parameters, each where `slot`'s region accounts its kernel ([`kernel_accounting_in`]).
fn state_owners(
    plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram, slot: &Slot,
) -> Result<StateBounds, GenericError> {
    program
        .kernels()
        .iter()
        .filter(|kernel| kernel.lineage.is_some())
        .map(|kernel| {
            let node = plan.node(&kernel.key.node).ok_or_else(|| {
                generic_error!(
                    "The boot program runs state owner `{}`, which its plan does not have; this is a bug.",
                    kernel.key.node
                )
            })?;
            let operator = registry.configure(node.op.as_str(), &node.params).map_err(|error| {
                generic_error!(
                    "State owner `{}` of the boot program cannot be configured: {}; this is a lowering bug.",
                    node.id,
                    error
                )
            })?;
            Ok((operator, kernel_accounting_in(slot, &node.id)))
        })
        .collect::<Result<_, GenericError>>()
        .map(StateBounds)
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
/// An edge into a slot on several lanes routes each buffer to a lane as the frame says: a point with lanes feeds the
/// slot lane by lane, each of the DogStatsD source's metrics lanes its own (`dsd_in.metrics_<d>` to lane `d`); an exit
/// into a state owner's lanes routes each event by its partition key; one that carries a producer lane's batches keeps
/// to the sender's lane; and the other edges spread whole buffers over the lanes. Every edge into a slot on one lane
/// leads to that lane.
///
/// Every admission point registers in the installer's admission registry. A point upstream of a state owner sends
/// heartbeats while it is idle, every [`heartbeat_period`], to the component its route leads to and to no other, so
/// that the state owner's watermark advances, while the DogStatsD taps on the same output keep their native traffic.
/// On lanes, the DogStatsD source's points are its producer lanes', which stamp them and drive their heartbeats (see
/// [`add_regions`]), so no dispatcher stamps them.
pub(super) fn connect_frame(blueprint: &mut TopologyBlueprint, boot: &BootTopology) -> Result<(), GenericError> {
    let frame = &boot.frame;
    for edge in frame.edges() {
        let to = target_component(frame, &boot.outputs, &edge.to)?;
        match &edge.from {
            FrameSource::Admission(id) => {
                let point = frame
                    .admission_point(*id)
                    .ok_or_else(|| generic_error!("The frame has no admission point {}; this is a bug.", id.get()))?;
                for (lane, from) in point_outputs(frame, point)?.iter().enumerate() {
                    match point.routing {
                        PointRouting::One => blueprint.connect_components(from.as_str(), to.as_str())?,
                        PointRouting::Spread => {
                            blueprint.connect_components_with(from.as_str(), to.as_str(), LaneEdge::Spread)?
                        }
                        PointRouting::Fixed => {
                            // At most `MAX_LANES` lanes, which fit.
                            let lane = u16::try_from(lane).map_err(|_| {
                                generic_error!("Admission point {} has too many lanes; this is a bug.", id.get())
                            })?;
                            blueprint.connect_components_with(from.as_str(), to.as_str(), LaneEdge::Fixed(lane))?
                        }
                    };
                }
            }
            FrameSource::Exit { .. } => {
                let from = source_output(frame, &edge.from)?;
                match exit_routing(frame, &edge.from)? {
                    ExitRouting::One => blueprint.connect_components(from.as_str(), to.as_str())?,
                    ExitRouting::Keyed(_) => {
                        blueprint.connect_components_with(from.as_str(), to.as_str(), LaneEdge::Keyed)?
                    }
                    ExitRouting::Affinity => {
                        blueprint.connect_components_with(from.as_str(), to.as_str(), LaneEdge::Affinity)?
                    }
                    ExitRouting::Spread => {
                        blueprint.connect_components_with(from.as_str(), to.as_str(), LaneEdge::Spread)?
                    }
                };
            }
        }
    }

    for point in frame.admission_points() {
        if lanes_own(boot, point) {
            continue;
        }
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

/// Returns the component outputs an admission point's lanes are, in lane order: the source's metrics lanes,
/// `<component>.metrics_<lane>`, for a point with lanes, and the point's one output otherwise.
pub(super) fn point_outputs(frame: &Frame, point: &AdmissionPointInfo) -> Result<Vec<String>, GenericError> {
    let output = admission_output(frame, &point.node)?;
    let Some(lanes) = NonZeroU16::new(point.lanes).filter(|lanes| lanes.get() > 1) else {
        return Ok(vec![output]);
    };
    let component = component_id(
        frame
            .fixed_component(&point.node)
            .ok_or_else(|| generic_error!("The frame has no fixed component `{}`; this is a bug.", point.node))?,
    );
    Ok((0..lanes.get())
        .map(|lane| format!("{}.{}", component, dogstatsd_metric_lane_output(lane, lanes)))
        .collect())
}

/// Returns the number of lanes the DogStatsD source's metrics split into, which its producer lanes stamp: one without
/// lanes, or without DogStatsD.
pub(super) fn dogstatsd_metric_lanes(frame: &Frame) -> NonZeroU16 {
    dogstatsd_point(frame)
        .and_then(|point| NonZeroU16::new(point.lanes))
        .unwrap_or(NonZeroU16::MIN)
}

/// Returns how the slot exit an edge starts at sends to the lanes of its target.
fn exit_routing<'f>(frame: &'f Frame, source: &FrameSource) -> Result<&'f ExitRouting, GenericError> {
    let FrameSource::Exit { slot, exit } = source else {
        return Err(generic_error!("An admission point has no exit routing; this is a bug."));
    };
    let slot = frame
        .slot(*slot)
        .ok_or_else(|| generic_error!("The frame has no slot {}; this is a bug.", slot))?;
    slot.exits
        .get(exit.get())
        .map(|exit| &exit.routing)
        .ok_or_else(|| generic_error!("Region `{}` has no exit {}; this is a bug.", slot.name, exit))
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
/// The driver and the boot program's kernels are built when the topology builds the region, inside its runtime. On
/// lanes, the host builds one driver per lane, each with kernels of its own accounted under the lane's group; the lanes
/// of a slot on several lanes share one [`SlotLanes`], whose panic ledger counts their panics against each generation's
/// one budget, and one [`LaneCell`] per lineage of the boot program, which every lane's instance of a state owner is
/// built against. `admission` gives each lane the admission point it stamps, and `stats` what each lane measures of
/// its work.
fn region_host(
    boot: &mut BootTopology, slot: &Slot, admission: Option<Vec<AdmissionPointConfig>>, stats: Option<&SlotLaneStats>,
) -> Result<RegionHostBuilder<MetricRegionDriver>, GenericError> {
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
    // as the native component it stands for did, and bounds one drain step. Each lane holds a driver of its own.
    let bounds = RegionBounds::for_slot(&boot.plan, &boot.registry, &program, slot)?;
    let bounds = LaneRegionBounds::new(bounds, slot, state_owners(&boot.plan, &boot.registry, &program, slot)?);
    let work_per_turn = usize::try_from(limits::values::WORK_PER_TURN)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| generic_error!("The work per turn of a region must be a positive number of batches."))?;
    let exits = slot.exits.iter().map(|exit| exit.name.to_string()).collect::<Vec<_>>();

    let generation = boot.generation.clone();
    let telemetry = boot.telemetry.clone();
    let failures = Arc::clone(&boot.failures);
    let leases = Arc::clone(&boot.leases);
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
    // What a slot's lanes share: one record of the slot's lanes, and one cell per lineage of the boot program, which
    // every lane's instance of the lineage is built against.
    let shared = (slot.lanes > 1).then(|| {
        let cells = program
            .kernels()
            .iter()
            .filter(|kernel| kernel.lineage.is_some())
            .map(|kernel| (kernel.key.node.clone(), LaneCell::new()))
            .collect::<BTreeMap<_, _>>();
        (SlotLanes::new(slot.lanes), cells)
    });
    let slot_config = slot.clone();
    // On lanes, each lane's driver publishes what its keyed exits sent, and a state lane what it holds and the rules
    // of its lineages decided, into the lane's own series.
    let lane_stats = stats.cloned();
    let build = move |lane: LaneContext| -> Result<MetricRegionDriver, GenericError> {
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(&program, keys, |node| {
            let context = telemetry.kernel_context_in(&program, node, lane);
            match shared.as_ref().and_then(|(_, cells)| cells.get(node)) {
                Some(cell) => context.with_lanes(LaneBinding {
                    cell: Arc::clone(cell),
                    lane: lane.index,
                    lanes: lane.count,
                }),
                None => context,
            }
        })?;
        let prepare = PrepareRegion::new(
            generation.clone(),
            Arc::clone(&program),
            kernels,
            PanicBudget::DEFAULT_PROGRAM,
            &telemetry,
        )
        .with_leases(Arc::clone(&leases));
        let mut config = RegionConfig::for_slot(&slot_config, &telemetry).with_failure_reporter(Arc::clone(&failures));
        if let Some((lanes, _)) = &shared {
            config = config.with_lane(lane, Arc::clone(lanes));
        }
        if let Some((schedule, wall_clock, budgets)) = &state {
            config = config
                .with_schedule(Arc::clone(schedule))
                .with_wall_clock(Arc::clone(wall_clock))
                .with_state_budgets(budgets.clone());
        }
        if let Some(driver) = lane_stats
            .as_ref()
            .and_then(|stats| stats.drivers.get(usize::from(lane.index)))
        {
            config = config.with_routing_telemetry(RoutingTelemetry {
                routed_buffers: Counter::from_arc(Arc::clone(&driver.routed_buffers)),
                routed_events: Counter::from_arc(Arc::clone(&driver.routed_events)),
                progress: Counter::from_arc(Arc::clone(&driver.progress)),
            });
            if state.is_some() {
                let mut lane_telemetry = telemetry.state_lane(slot_config.name.as_str(), lane.index);
                lane_telemetry.contexts = Gauge::from_arc(Arc::clone(&driver.contexts));
                config = config.with_state_lane_telemetry(lane_telemetry);
            }
        }
        Ok(MetricRegionDriver::new(config, prepare)?)
    };
    let build = Arc::new(build);
    let one = Arc::clone(&build);
    let mut host = RegionHostBuilder::new(exits, work_per_turn, mailbox, move |_| one(LaneContext::ONE))?;
    if boot.lanes.lanes_on() {
        let lanes = NonZeroU16::new(slot.lanes)
            .ok_or_else(|| generic_error!("The frame lays region `{}` out on no lanes; this is a bug.", slot.name))?;
        host = host.with_lanes(lanes, move |_, lane| build(lane));
        if let Some(points) = admission {
            host = host.with_lane_admission(points);
        }
        if let Some(stats) = stats {
            host = host.with_lane_stats(stats.lanes.clone());
        }
    }
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
        FrameTarget::Destination(output) => outputs
            .get(output.as_str())
            .map(|output| output.component.clone())
            .ok_or_else(|| {
                generic_error!(
                    "Output `{}` is not a native metrics output; this is a lowering bug.",
                    output
                )
            }),
        // Stage 7 part P7.5 binds outputs to delivery hosts and builds their encode hosts; until then the frame has
        // none, and every output is a fixed destination.
        FrameTarget::EncodeHost(host) => frame
            .encode_host(host)
            .map(|host| host.encoder.to_string())
            .ok_or_else(|| generic_error!("The frame has no encode host for `{}`; this is a bug.", host)),
    }
}

#[cfg(test)]
mod burst_tests;
#[cfg(test)]
mod differential_tests;
#[cfg(test)]
mod isolation_tests;
#[cfg(test)]
mod lane_tests;
#[cfg(test)]
mod measurement_tests;
#[cfg(test)]
mod tests;

//! The installer: boots the plan-driven metrics path, and keeps it running the default program of the current
//! configuration, or the policy artifact the artifact channel delivers.
//!
//! # Boot
//!
//! [`ProgrammableMetrics::boot`] captures the [host facts](HostFacts), prepares [lowering](Lowering) once from the boot
//! configuration, lowers and validates the boot default program, and computes its physical frame, which fixes the
//! structure of the metrics path for the life of the process. It then creates the installer's state: the plan crate's
//! [`InstallerCore`], which makes the boot generation, G0, active at once, the admission schedule `Steady(G0)` that
//! every admission point reads, and one control mailbox per region. The topology is built from what it prepared (see
//! [`PlanTopology`](super::topology::PlanTopology)), and every region host starts with G0's program.
//!
//! A default program that fails validation still runs, as long as it is structurally sound and fits its frame: it is
//! what native configuration says, and nothing else could run in its place. It is reported degraded, in the logs and
//! in [`PlanStatus::degraded`].
//!
//! # Installation
//!
//! The [installer worker](InstallerWorker) runs on its own task, supervised. It treats every configuration change as a
//! wake signal: it lowers the new configuration and proposes the result to the core, which compares it with the plan
//! that is active or being prepared, places it into the frame, diffs it, and decides what to do. The worker performs
//! what the core decides, in order:
//!
//! 1. **Prepare.** It builds the kernels each slot's program needs and the region lacks, off the data path, in the
//!    slot's resource group, and sends every region a preparation through its control mailbox, without waiting on it.
//!    A kernel that fails to build fails the preparation at once.
//! 2. **Barrier.** It reports every region's acknowledgment to the core. When every region has acknowledged, the core
//!    commits; when one fails, or not all acknowledge within the prepare timeout, the core aborts in every region, and
//!    a default-program candidate is retried with backoff (only the latest candidate is kept).
//! 3. **Commit.** One store of `Steady(generation)` into the schedule: every admission point stamps new batches with
//!    the new generation from then on, at a batch boundary.
//! 4. **Retire.** Batches stamped earlier finish under their own generation. When the last reference to a generation
//!    drops, its token notifies the worker, which reports the retirement to the core; regions release its program.
//!
//! The installer's state lives in an [`Arc`] outside the worker's task, so a restarted worker continues where the
//! previous one stopped: with the same core, schedule, acknowledgments still awaited, deadlines, and actions not yet
//! performed. Standalone mode never changes configuration, so there the worker only waits for artifacts.
//!
//! # Cuts
//!
//! A candidate whose state owners cannot carry their state over at a batch boundary, such as one that changes
//! aggregation's bucket width, is installed with a cut: the old and new generations switch at a cut time that every
//! bucket width involved shares. Once every region acknowledged the preparation, the worker performs the core's cut
//! actions against the [`AdmissionRegistry`] of the topology (see the `cut` module):
//!
//! 1. **Arm.** In one transaction over the locks of the admission points upstream of the changing state owners, it
//!    computes the cut time from their latest stamps and wall clocks, records the cut on the new generation's token,
//!    and publishes `Cut { old, new, at }`. A lock that is not free within the lock timeout, or a clock running ahead
//!    of its wall clock, puts arming off, and the core tries again after a delay. A cut that can never be armed, which
//!    only a bug leaves, fails its generation, and the active one keeps running.
//! 2. **Watch.** While the cut is in flight, it polls every admission point every `CUT_POLL`, without waiting on a busy
//!    one, and reports whether any point stamped the new generation (the cut is then irrevocable) and how many affected
//!    points passed the cut.
//! 3. **Cancel.** A newer proposal before any point stamped the new generation cancels the cut: under every point's
//!    lock, those feeding no state owner first, it publishes `Steady(old)`, or refuses if a point stamped the new
//!    generation or a lock is not free in time.
//! 4. **Finalize.** Once every affected point passed the cut, one store of `Steady(new)`. The replaced generation's
//!    drain deadline starts then; before, a cut whose affected points have not passed its time plus the drain deadline
//!    is reported overdue.
//!
//! Each step is idempotent by the schedule's value, and a restarted worker performs again the cut step the core has in
//! flight ([`InstallerCore::resume_actions`]).
//!
//! When aggregation runs in an isolation slot, a second worker, the [snapshot bridge](super::snapshot_bridge), answers
//! the DogStatsD context dump API from that region.
//!
//! # Artifacts
//!
//! With an artifact channel [connected](ProgrammableMetrics::connect_artifact_channel), the worker also wakes on every
//! snapshot the channel delivers, and hands it to the core ([`InstallerCore::on_snapshot`]), which revokes, rejects,
//! acknowledges, or proposes the artifact it holds, composed into the default program ADP last lowered, with its
//! catalog. While an artifact is in effect, each configuration change composes it again against the new default
//! program ([`InstallerCore::on_default_program`]), so native configuration still reaches it through its bindings.
//! The core's actions are performed as for any candidate. A preparation's panic budget is zero for the default program,
//! and, for an artifact, [`PanicBudget::ARTIFACT`] in the regions where the composed plan places a node with the
//! artifact's provenance, and zero in the others, which run native nodes only. Even in those regions the budget covers
//! only the nodes with the artifact's provenance: a panic in a native node, such as host enrichment beside the policy
//! region, fails the region as the native component does. When a region fails an artifact generation, the core rolls
//! back to the generation kept as last-known-good, or asks for the default program.
//!
//! After every event, the worker reports the core's acknowledgments and its status summary back through the channel
//! ([`InstallerCore::apply_states`]), with the base program hash of the default program's export and whether ADP is
//! degraded.
//!
//! # Telemetry and status
//!
//! The worker publishes `plan_active_generation`, `plan_active_revision`, `plan_live_generations`, and
//! `plan_installs_total{outcome}`, and the native metrics that telemetry profiles say the installer records from
//! bindings (see [`InstallerTelemetry`](super::telemetry::InstallerTelemetry)). It keeps a [`PlanStatus`] current in an
//! [`ArcSwap`], with everything the [control surface](super::api) serves: the acknowledgments and status summary, the
//! export of every scope of the default program, and the active plan with its explain document.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    pin::pin,
    sync::Arc,
    time::SystemTime,
};

use agent_data_plane_config::{Live, SalukiConfiguration};
use agent_data_plane_config_system::ConfigurationSystem;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt as _};
use saluki_common::sync::shutdown::ShutdownHandle;
#[cfg(test)]
use saluki_core::execution::ManualWallClock;
use saluki_core::{
    accounting::ComponentRegistry,
    components::ComponentContext,
    execution::{
        control_mailbox, AdmissionPointId, AdmissionRegistry, AdmissionSchedule, AdmissionTime, ControlMailbox,
        ControlSender, GenerationId, GenerationRef, RetireNotifier, ScheduleHandle, SystemWallClock, WallClock,
    },
    runtime::{InitializationError, Supervisable, SupervisorFuture},
    topology::{topology_identifier, ComponentId},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    canonical::PlanHash,
    channel::{artifact_channel, ApplyStates, ArtifactSnapshot, InstallerEnd, TransportEnd},
    compile::{compile, compile_unvalidated, place_unvalidated, Frame, RegionProgram, SlotId},
    compose::{compose_all, Approval, ComposeError, Composed, Composer},
    explain::Explain,
    export::{base, export, ExportBundle},
    install::{
        Action, ArmCut, Candidate, CandidateId, CutDeferReason, CutPhase, GenerationState, InstallerConfig,
        InstallerCore, InstallerStatus, Origin, Outcome, PendingReason, Prepare, Report, SlotPrepare,
    },
    ir::{NodeId, OutputId, Plan, Provenance, ReceivedArtifact, ScopeName},
    limits,
    operators::OperatorRegistry,
    runtime::{
        build_kernels, GenerationFailureReporter, PanicBudget, PrepareRegion, RegionControl, RegionError, StateBudgets,
    },
    validate::{ValidationReport, Validator},
};
use tokio::{
    select,
    sync::{mpsc, Mutex},
    time::{sleep_until, Instant},
};
use tracing::{debug, error, info, warn};

use self::cut::{ArmOutcome, CancelOutcome, Polled};
use super::{
    catalog::AdpCatalog,
    host_facts::HostFacts,
    lowering::Lowering,
    outputs::{NativeOutputs, OutputKind},
    snapshot_bridge::{SnapshotBridge, SnapshotBridgeState},
    telemetry::{CutOutcome, InstallOutcome, InstallerTelemetry, RegionTelemetry},
    topology::{aggregation_slot, slot_name},
};
use crate::{cli::run::PRIMARY_TOPOLOGY, config::DataPlaneConfiguration, internal::env::ADPEnvironmentProvider};

/// What the plan-driven topology is built from: the boot generation in its frame.
pub struct BootTopology {
    /// The physical frame, computed from the boot default program.
    pub frame: Arc<Frame>,

    /// The boot default program.
    pub plan: Arc<Plan>,

    /// The operator registry the program is compiled with.
    pub registry: OperatorRegistry,

    /// The native metrics outputs.
    pub outputs: NativeOutputs,

    /// The host facts, captured at boot.
    pub facts: HostFacts,

    /// The boot generation, G0.
    pub generation: GenerationRef,

    /// The boot generation's program of every slot, in slot order.
    pub programs: Vec<Arc<RegionProgram>>,

    /// The admission schedule, `Steady(G0)` at boot, which every admission point reads.
    pub schedule: ScheduleHandle,

    /// The receiving side of every region's control mailbox, in slot order, taken when the region host is created.
    pub mailboxes: Vec<Option<ControlMailbox<RegionControl>>>,

    /// What regions publish to, the identities their kernels report as, and where their kernels are accounted.
    pub telemetry: RegionTelemetry,

    /// Where regions report the generations they fail, for the installer.
    pub failures: GenerationFailureReporter,

    /// The registry every admission point of the frame registers in when the topology starts, through which the
    /// installer arms, watches, and cancels cuts.
    pub admission: AdmissionRegistry,

    /// The budgets of state every lineage of every state owner draws on: one global budget, the boot program's context
    /// limit of DogStatsD aggregation, and one per state owner.
    pub state_budgets: StateBudgets,

    /// The wall clock isolation slots read for their tick gates, the time they close at when their input ends, and
    /// their watermark lag: the operating system's.
    pub wall_clock: Arc<dyn WallClock>,

    /// DogStatsD aggregation's context snapshot requests, when aggregation runs in an isolation slot: taken when the
    /// DogStatsD pipeline is added, and answered by the [snapshot bridge](ProgrammableMetrics::snapshot_bridge).
    pub snapshots: Option<SnapshotBridgeState>,

    /// The wall clock the admission points that feed state owners read in place of the operating system's, for tests
    /// that clock aggregation.
    #[cfg(test)]
    pub state_admission_clock: Option<ManualWallClock>,
}

/// The programmable metrics path of one process: what the metrics path of the topology is built from, and the
/// installer that keeps it current.
pub struct ProgrammableMetrics {
    topology: BootTopology,
    installer: Installer,
    status: Arc<ArcSwap<PlanStatus>>,
}

impl ProgrammableMetrics {
    /// Lowers the boot configuration into the default program and prepares the metrics path and its installer.
    ///
    /// `process_start` is the wall-clock time at which the process started. Returns `None` when the configuration
    /// requires no metrics pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error where the native metrics path fails to build, with the native error: host tags that cannot be
    /// fetched, or a configuration the native components reject. Returns an error also if the default program cannot
    /// be laid out or compiled, which only a plan with structural problems causes.
    pub async fn boot(
        process_start: SystemTime, config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider,
        client_config: Option<&RemoteAgentClientConfiguration>, component_registry: &ComponentRegistry,
    ) -> Result<Option<Self>, GenericError> {
        // The installer wakes on every change after the configuration it boots with.
        let config = config_system.live(|config| config);
        let startup = Arc::new(SalukiConfiguration::clone(&config));
        if !DataPlaneConfiguration::from_configuration(&startup).metrics_pipeline_required() {
            return Ok(None);
        }

        let facts = HostFacts::capture(process_start, &startup, env_provider, client_config).await?;
        let Some(lowering) = Lowering::new(Arc::clone(&startup), facts.clone())? else {
            return Ok(None);
        };
        let operators = OperatorRegistry::native();
        Self::from_lowering(lowering, facts, &startup, config, component_registry, operators).map(Some)
    }

    /// Prepares the metrics path and its installer from lowering prepared at boot, with the configuration the process
    /// started with and the view the installer wakes on.
    ///
    /// `registry` is the registry of the operators that plans are validated, laid out, composed, and compiled with: the
    /// native operators, which tests extend with operators of their own.
    pub(super) fn from_lowering(
        lowering: Lowering, facts: HostFacts, startup: &SalukiConfiguration, config: Live<SalukiConfiguration>,
        component_registry: &ComponentRegistry, registry: OperatorRegistry,
    ) -> Result<Self, GenericError> {
        let lowered = lowering
            .lower(startup)
            .error_context("Failed to lower the metrics configuration into the default program.")?;
        for note in &lowered.notes {
            info!(node = %note.node, "{}", note.message);
        }
        let report = Validator::new(&registry, &lowered.catalog).validate(&lowered.plan);
        let degraded = errors(&report);
        if !degraded.is_empty() {
            warn!(
                errors = %degraded.join("; "),
                "The default program lowered from the configuration does not validate. It runs as the configuration \
                 says, degraded: artifacts derived from it are rejected."
            );
        }

        let frame = Frame::compute(&lowered.plan, &registry, &slot_name).map_err(|error| {
            generic_error!(
                "The metrics path cannot be laid out from the default program lowered from the configuration: {} If \
                 a node's parameter is out of range, change the configuration setting it is bound to.",
                error
            )
        })?;
        let frame = Arc::new(frame);
        let plan = Arc::new(lowered.plan);

        let (retired_tx, retired_rx) = mpsc::unbounded_channel();
        let notifier = move |_: GenerationId| {
            let retired_tx = retired_tx.clone();
            // Runs wherever the last reference to the generation drops, possibly on the data path: it only queues.
            RetireNotifier::new(move |generation| {
                let _ = retired_tx.send(generation);
            })
        };
        let boot_candidate = Candidate {
            plan: Arc::clone(&plan),
            report,
            origin: Origin::DefaultProgram { config_change: 0 },
        };
        let (core, boot) = InstallerCore::new(
            Arc::clone(&frame),
            registry,
            InstallerConfig::default(),
            notifier,
            boot_candidate,
        )
        .map_err(|rejection| generic_error!("The default program cannot run in its own frame: {}", rejection))?;
        // The boot generation's validation errors are kept with its candidate, as every default program's are, so that
        // they are reported again when it is active again after a rollback.
        let degraded_candidates = core
            .status()
            .active
            .map(|active| (active.candidate, degraded.clone()))
            .into_iter()
            .collect();
        let schedule = AdmissionSchedule::Steady(boot.generation.clone()).into_handle();

        let (regions, mailboxes): (Vec<_>, Vec<_>) = frame
            .slots()
            .iter()
            .map(|_| {
                let (sender, mailbox) = control_mailbox();
                (sender, Some(mailbox))
            })
            .unzip();
        let topology_root = topology_identifier(PRIMARY_TOPOLOGY);
        let region_identities = frame
            .slots()
            .iter()
            .map(|slot| {
                let component_id = ComponentId::try_from(slot.name.as_str())
                    .map_err(|error| generic_error!("Region `{}` has an invalid name: {}", slot.name, error))?;
                Ok(ComponentContext::transform(&topology_root, component_id).identity())
            })
            .collect::<Result<Vec<_>, GenericError>>()?;
        // Every lineage of every state owner, in every generation, draws on the same budgets, which native
        // aggregation's context limit sizes.
        let state_budgets = match lowered.catalog.state_budget() {
            Some(capacity) => StateBudgets::new(capacity),
            None => StateBudgets::unlimited(),
        };
        let region_telemetry = RegionTelemetry::new(topology_root.clone())
            .with_accounting(component_registry.clone(), region_identities)
            .with_state(state_budgets.clone(), &frame);
        // Runs on a region's task, possibly mid-turn: it only queues.
        let (failures_tx, failures_rx) = mpsc::unbounded_channel();
        let failures: GenerationFailureReporter = Arc::new(move |generation, reason: &str| {
            let _ = failures_tx.send((generation, reason.to_string()));
        });
        let mut telemetry = InstallerTelemetry::new(topology_root);
        telemetry.set_binding_gauges(&plan);
        telemetry.register_change_counters(&plan);
        telemetry.rewrites(core.active_rewrites());

        let labels = output_labels(lowering.outputs());
        let default_program = DefaultProgramView::of(&plan, lowered.catalog, &labels);
        let outputs = lowering.outputs().clone();
        let admission = AdmissionRegistry::new();
        let cut_points = CutPoints::of(&frame);
        let mut installer = Installer {
            core,
            lowering,
            registry,
            schedule: Arc::clone(&schedule),
            admission: admission.clone(),
            cut_points,
            arming: None,
            cut: None,
            regions,
            region_telemetry: region_telemetry.clone(),
            telemetry,
            gauges_describe: Some(boot.generation.id()),
            retired: retired_rx,
            failures: failures_rx,
            config,
            config_changes: 0,
            last_lowered: Arc::clone(&plan),
            default_program,
            labels,
            degraded: degraded_candidates,
            active_degraded: degraded,
            last_error: None,
            last_error_is_policy: false,
            last_config_candidate: None,
            actions: VecDeque::new(),
            acks: FuturesUnordered::new(),
            prepare_deadline: None,
            retry_at: None,
            draining: BTreeMap::new(),
            channel: None,
            artifact_paths: Box::new(CoreArtifactPaths),
            active_plan: None,
        };
        let status = Arc::new(ArcSwap::from_pointee(installer.current_status()));

        Ok(Self {
            topology: BootTopology {
                frame,
                plan,
                registry,
                outputs,
                facts,
                generation: boot.generation,
                programs: boot.programs,
                schedule,
                mailboxes,
                telemetry: region_telemetry,
                failures,
                admission,
                state_budgets,
                wall_clock: Arc::new(SystemWallClock),
                snapshots: None,
                #[cfg(test)]
                state_admission_clock: None,
            },
            installer,
            status,
        })
    }

    /// Returns what the metrics path of the topology is built from.
    pub(super) fn boot_topology(&mut self) -> &mut BootTopology {
        &mut self.topology
    }

    /// Connects the installer to an artifact channel, and returns the channel's other end, for its transport.
    ///
    /// From then on, the installer installs the policy artifacts the transport publishes, and reports their
    /// acknowledgments and its status summary back to it. Without a channel, the default program runs.
    pub fn connect_artifact_channel(&mut self) -> TransportEnd {
        let (transport, end) = artifact_channel();
        self.installer.channel = Some(ChannelEnd {
            end,
            open: true,
            handled: None,
        });
        // The transport sends what the installer reports from its first poll on, before the worker runs.
        self.installer.publish(&self.status);
        transport
    }

    /// Replaces the core's artifact paths with a test double.
    #[cfg(test)]
    pub(super) fn with_artifact_paths(&mut self, paths: Box<dyn ArtifactPaths>) {
        self.installer.artifact_paths = paths;
    }

    /// Returns the installer's status, kept current as it changes, for the control surface.
    pub fn status(&self) -> Arc<ArcSwap<PlanStatus>> {
        Arc::clone(&self.status)
    }

    /// Returns the worker that answers DogStatsD aggregation's context snapshot requests from the isolation slot that
    /// runs aggregation, `dsd_agg`, or `None` if the topology has no DogStatsD pipeline.
    ///
    /// Call it once the topology is built: the topology takes the requests when it adds the DogStatsD pipeline.
    pub fn snapshot_bridge(&self) -> Option<SnapshotBridge> {
        let state = self.topology.snapshots.clone()?;
        let slot = aggregation_slot(&self.topology.frame)?.id;
        let region = self.installer.regions.get(slot.get())?.clone();
        Some(SnapshotBridge::new(state, region))
    }

    /// Returns the installer worker, to run once the topology is built.
    pub fn into_installer(self) -> InstallerWorker {
        InstallerWorker {
            state: Arc::new(InstallerState {
                installer: Mutex::new(self.installer),
                status: self.status,
            }),
        }
    }
}

/// The installer's view of the metrics plan, for status reporting and the control surface.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanStatus {
    /// The installer core's state: the active, kept, preparing, and pending generations, those alive, and the most
    /// recent others.
    pub installer: InstallerStatus,

    /// The validation errors of the active default program, which runs degraded while there are any.
    pub degraded: Vec<String>,

    /// The number of configuration changes observed since boot.
    pub config_changes: u64,

    /// Why the last configuration change was not installed, if it was not and nothing was installed since.
    pub last_error: Option<String>,

    /// The generations still draining past the drain deadline: batches stamped with them are still in flight.
    pub overdue: Vec<GenerationId>,

    /// The status summary and every config's acknowledgment: what the installer reports through the artifact channel,
    /// when one is connected.
    pub apply_states: ApplyStates,

    /// The export of every scope of the default program last lowered, by scope, or why the scope cannot be exported.
    pub exports: BTreeMap<ScopeName, Result<Arc<ExportBundle>, String>>,

    /// The default program last lowered, with every configuration binding resolved: the plan artifacts are composed
    /// into, and the exports are taken from.
    pub default_program: Arc<Plan>,

    /// The active generation's plan and explain document.
    pub active: Option<Arc<ActivePlan>>,
}

/// The plan of the active generation, as the control surface serves it.
#[derive(Debug)]
pub struct ActivePlan {
    /// The active generation.
    pub generation: GenerationId,

    /// Its plan, composed, with every configuration binding resolved.
    pub plan: Arc<Plan>,

    /// The plan's explain document, with the rewrites compiling it considered, and the physical sections of how it
    /// runs, rewritten, in the frame built at boot.
    pub explain: Explain,
}

impl PartialEq for ActivePlan {
    fn eq(&self, other: &Self) -> bool {
        // The installer describes each generation once, and publishes that description until another is active, so
        // comparing a published status with the current one is usually comparing one description with itself.
        std::ptr::eq(self, other)
            || (self.generation == other.generation && self.plan == other.plan && self.explain == other.explain)
    }
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.installer.active {
            Some(active) => write!(f, "generation {} active", active.id)?,
            None => f.write_str("no generation active")?,
        }
        if let Some(artifact) = &self.apply_states.status.active_artifact {
            write!(f, " (artifact `{}` revision {})", artifact.id, artifact.revision)?;
        }
        write!(
            f,
            ", {} live, {} configuration changes observed",
            self.installer.live.len(),
            self.config_changes
        )?;
        if let Some(preparing) = &self.installer.preparing {
            write!(f, ", generation {} preparing", preparing.id)?;
        }
        if let Some(cut) = &self.installer.cut {
            write!(f, ", cut to generation {}", cut.generation)?;
            match cut.phase {
                CutPhase::Barrier => f.write_str(" not armed yet")?,
                CutPhase::Armed { at } => write!(f, " armed at {}", at)?,
                CutPhase::Irrevocable { at } => write!(
                    f,
                    " irrevocable at {}, {} of {} admission points passed",
                    at, cut.passed, cut.total
                )?,
            }
            if let Some((reason, times)) = cut.deferred {
                let reason = match reason {
                    CutDeferReason::Timeout => "admission points busy".to_string(),
                    CutDeferReason::ClockAhead { by } => format!("a clock {:?} ahead of its wall clock", by),
                };
                match times {
                    1 => write!(f, " (put off once: {})", reason)?,
                    times => write!(f, " (put off {} times: {})", times, reason)?,
                }
            }
        }
        if self.installer.pending.is_some() {
            f.write_str(", a candidate pending")?;
        }
        if self.apply_states.status.stale_base {
            f.write_str(", stale base")?;
        }
        if self.apply_states.status.stale_review {
            f.write_str(", stale review")?;
        }
        if !self.degraded.is_empty() {
            write!(f, ", degraded: {}", self.degraded.join("; "))?;
        }
        if let Some(error) = &self.last_error {
            write!(f, ", last change not installed: {}", error)?;
        }
        if !self.overdue.is_empty() {
            let overdue = self.overdue.iter().map(ToString::to_string).collect::<Vec<_>>();
            write!(f, ", draining past the deadline: {}", overdue.join(", "))?;
        }
        Ok(())
    }
}

/// The installer's state, shared by every run of the worker.
struct InstallerState {
    installer: Mutex<Installer>,
    status: Arc<ArcSwap<PlanStatus>>,
}

/// The supervised worker that installs new generations of the metrics plan as the configuration changes and artifacts
/// arrive.
pub struct InstallerWorker {
    state: Arc<InstallerState>,
}

impl InstallerWorker {
    /// Returns the installer's status, kept current as it changes.
    #[cfg(test)]
    pub fn status(&self) -> Arc<ArcSwap<PlanStatus>> {
        Arc::clone(&self.state.status)
    }
}

#[async_trait]
impl Supervisable for InstallerWorker {
    fn name(&self) -> &str {
        "programmable-metrics-installer"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let state = Arc::clone(&self.state);
        Ok(Box::pin(async move {
            let mut shutdown = pin!(process_shutdown);
            let mut installer = state.installer.lock().await;
            // A snapshot whose handling a panic of the previous run interrupted is handled again.
            if let Some(channel) = &mut installer.channel {
                channel.restart();
            }
            // The cut step the previous run was performing when it stopped is performed again, first: every cut step
            // is idempotent by the schedule's value.
            installer.resume_cut();
            // A topology builds its region hosts long before it runs them: it waits for its environment to be ready
            // first, which can take a while. A preparation sent before a region reads its mailbox would only time
            // out, so the installer starts once every region runs, and installs the configuration current then.
            select! {
                biased;
                _ = &mut shutdown => return Ok(()),
                () = wait_for_regions(installer.regions.clone()) => {}
            }
            loop {
                installer.perform_actions().await;
                installer.publish(&state.status);
                let event = select! {
                    biased;
                    _ = &mut shutdown => return Ok(()),
                    event = installer.next_event() => event,
                };
                installer.handle(event);
            }
        }))
    }
}

/// The core's artifact paths, which the worker hands every snapshot from the artifact channel and every default
/// program it lowers.
///
/// The worker calls the core's own ([`CoreArtifactPaths`]); tests substitute doubles, to drive the worker through what
/// the core decides.
pub(super) trait ArtifactPaths: Send {
    /// Hands the core a snapshot from the artifact channel, composing through `composer`, and returns what to do.
    fn on_snapshot(
        &mut self, core: &mut InstallerCore, snapshot: &ArtifactSnapshot, composer: &dyn Composer,
    ) -> Vec<Action>;

    /// Proposes a default program lowered after a configuration change, composing an artifact in effect through
    /// `composer`, and returns the candidate's identifier and what to do.
    fn on_default_program(
        &mut self, core: &mut InstallerCore, candidate: Candidate, composer: &dyn Composer,
    ) -> (CandidateId, Vec<Action>);

    /// Returns every config's acknowledgment and the status summary, with the base program hash of each scope of the
    /// default program and why the host is degraded, if it is: what the installer reports, and the artifacts in effect
    /// it reports.
    fn apply_states(
        &self, core: &InstallerCore, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates;
}

/// The core's own artifact paths: [`InstallerCore::on_snapshot`], [`InstallerCore::on_default_program`], and
/// [`InstallerCore::apply_states_with_bases`].
struct CoreArtifactPaths;

impl ArtifactPaths for CoreArtifactPaths {
    fn on_snapshot(
        &mut self, core: &mut InstallerCore, snapshot: &ArtifactSnapshot, composer: &dyn Composer,
    ) -> Vec<Action> {
        core.on_snapshot(snapshot, composer)
    }

    fn on_default_program(
        &mut self, core: &mut InstallerCore, candidate: Candidate, composer: &dyn Composer,
    ) -> (CandidateId, Vec<Action>) {
        core.on_default_program(candidate, composer)
    }

    fn apply_states(
        &self, core: &InstallerCore, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates {
        core.apply_states_with_bases(bases, degraded)
    }
}

/// Composes artifacts into the default program ADP last lowered, with the catalog it was lowered with.
struct DefaultProgramComposer<'a> {
    plan: &'a Plan,
    catalog: &'a AdpCatalog,
    registry: &'a OperatorRegistry,
}

impl Composer for DefaultProgramComposer<'_> {
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        compose_all(self.plan, artifacts, self.catalog, self.registry)
    }
}

/// The installer's end of the artifact channel.
struct ChannelEnd {
    end: InstallerEnd,
    /// Whether the transport still publishes: once it is gone, the last snapshot stays in effect.
    open: bool,
    /// The last snapshot the core finished handling, which a restarted worker does not hand it again.
    handled: Option<Arc<ArtifactSnapshot>>,
}

impl ChannelEnd {
    /// Prepares the channel for a new run of the worker: the latest snapshot is handed to the core again only if the
    /// core did not finish handling it, because a panic interrupted the previous run.
    ///
    /// Handling a snapshot again is not idempotent in general: once its artifact has failed under supervision and
    /// rolled back, its revision is at the high-water mark, so handling it again would reject it as stale, replacing
    /// its `failed` acknowledgment.
    fn restart(&mut self) {
        if self.end.feed.latest() != self.handled {
            self.end.feed.mark_unseen();
        }
    }
}

/// What the installer derives from the default program it last lowered, for the channel and the control surface.
struct DefaultProgramView {
    /// The catalog the default program was lowered and validated with.
    catalog: AdpCatalog,
    /// The export of every scope of the default program, or why the scope cannot be exported.
    exports: BTreeMap<ScopeName, Result<Arc<ExportBundle>, String>>,
    /// The base program hash an artifact derived from the export of each scope records, for every scope whose base can
    /// be computed.
    bases: BTreeMap<ScopeName, PlanHash>,
}

impl DefaultProgramView {
    /// Derives the view of a default program lowered with `catalog`, with output display labels `labels`.
    fn of(plan: &Plan, catalog: AdpCatalog, labels: &BTreeMap<OutputId, String>) -> Self {
        let document = catalog.to_document();
        let exports = plan
            .scopes
            .keys()
            .map(|scope| {
                let bundle = export(plan, scope, &document, labels)
                    .map(Arc::new)
                    .map_err(|error| error.to_string());
                (scope.clone(), bundle)
            })
            .collect();
        let bases = plan
            .scopes
            .keys()
            .filter_map(|scope| match base(plan, scope) {
                Ok(base) => Some((scope.clone(), base.program_hash)),
                Err(error) => {
                    debug!(
                        scope = %scope,
                        error = %error,
                        "The base program hash of a scope of the default program cannot be computed."
                    );
                    None
                }
            })
            .collect();
        Self {
            catalog,
            exports,
            bases,
        }
    }
}

/// Returns the display label of every output, by output ID, as exports carry them.
///
/// A label says what its output is, never where it delivers: the catalog's recipients identify that.
fn output_labels(outputs: &NativeOutputs) -> BTreeMap<OutputId, String> {
    outputs
        .iter()
        .map(|output| {
            let label = match &output.kind {
                OutputKind::ClientTelemetry => "DogStatsD client telemetry".to_string(),
                OutputKind::Main => "Main metrics output".to_string(),
                OutputKind::RoutingGroup(group) => format!("Endpoint routing group {}", group.index),
                OutputKind::MultiRegionFailover => "Multi-region failover".to_string(),
                OutputKind::AutoscalingFailover => "Autoscaling failover".to_string(),
            };
            (output.decl.id.clone(), label)
        })
        .collect()
}

/// One region's acknowledgment of a preparation.
struct Acknowledgment {
    generation: GenerationId,
    slot: SlotId,
    outcome: Result<(), String>,
}

/// Something the installer reacts to.
enum Event {
    /// A generation's token was dropped.
    Retired(GenerationId),

    /// A region failed a generation, for the reason given.
    Failed(GenerationId, String),

    /// A region acknowledged a preparation.
    Acknowledged(Acknowledgment),

    /// A preparation's timeout passed.
    TimedOut(GenerationId),

    /// The delay before trying a failed preparation again passed.
    Retry,

    /// A generation has been draining for the drain deadline.
    DrainDeadline(GenerationId),

    /// A poll of the admission points found what they say about the cut in flight to a generation.
    CutPolled(GenerationId, Polled),

    /// The artifact channel delivered a snapshot.
    Snapshot(Arc<ArtifactSnapshot>),

    /// The artifact channel's transport is gone: nothing will be delivered any more.
    ChannelClosed,

    /// The configuration changed.
    ConfigChanged(Box<SalukiConfiguration>),
}

/// A generation that is draining: superseded, with batches stamped with it possibly still in flight.
struct Draining {
    since: Instant,
    overdue: bool,
}

/// The admission points of the frame, as cuts lock and poll them.
#[derive(Clone, Debug)]
struct CutPoints {
    /// Every admission point, in the order a cancellation locks them: those that feed no state owner first.
    every: Vec<AdmissionPointId>,

    /// The admission points upstream of every isolation slot: those a cut of their state owners is armed over.
    affected: Vec<AdmissionPointId>,
}

impl CutPoints {
    fn of(frame: &Frame) -> Self {
        let isolation = frame
            .slots()
            .iter()
            .filter(|slot| slot.is_isolation())
            .map(|slot| slot.id)
            .collect::<Vec<_>>();
        Self {
            every: frame.cancel_lock_order(),
            affected: frame.affected_points(&isolation),
        }
    }
}

/// A cut the installer arms: the last [`Action::ArmCut`] it performed.
struct Arming {
    generation: GenerationId,
    old: GenerationId,
    affected: Vec<AdmissionPointId>,
    armed: bool,
}

/// A cut in flight that the installer watches, polling the admission points, from [`Action::WatchCut`] until it is
/// finalized or cancelled.
struct CutWatch {
    generation: GenerationId,
    /// The generation the cut replaces, which stays in flight until the cut is finalized.
    old: Option<GenerationId>,
    at: AdmissionTime,
    /// The admission points the cut is armed over.
    affected: Vec<AdmissionPointId>,
    next_poll: Instant,
    /// Whether an affected point has not passed the cut by its time plus the drain deadline.
    overdue: bool,
}

/// The installer: its core and everything it needs to perform the core's actions.
struct Installer {
    core: InstallerCore,
    lowering: Lowering,
    registry: OperatorRegistry,
    schedule: ScheduleHandle,
    /// The registry the frame's admission points register in, which cuts lock and poll.
    admission: AdmissionRegistry,
    cut_points: CutPoints,
    /// The cut the installer last armed or tried to.
    arming: Option<Arming>,
    /// The cut in flight the installer watches.
    cut: Option<CutWatch>,
    /// The sending side of every region's control mailbox, in slot order.
    regions: Vec<ControlSender<RegionControl>>,
    region_telemetry: RegionTelemetry,
    telemetry: InstallerTelemetry,
    /// The generation whose activation last set the gauges that describe the active plan: the binding gauges and the
    /// rewrites applied.
    gauges_describe: Option<GenerationId>,
    retired: mpsc::UnboundedReceiver<GenerationId>,
    failures: mpsc::UnboundedReceiver<(GenerationId, String)>,
    /// The configuration view the installer wakes on.
    config: Live<SalukiConfiguration>,
    config_changes: u64,
    /// The last default program lowered, against which a change's binding changes are counted, and into which
    /// artifacts are composed.
    last_lowered: Arc<Plan>,
    /// What is derived from the last default program lowered: its catalog, exports, and base program hash.
    default_program: DefaultProgramView,
    /// The display label of every output, by output ID.
    labels: BTreeMap<OutputId, String>,
    /// The validation errors of default-program candidates, by candidate, for as long as the core holds the candidate:
    /// waiting, being prepared, active, or kept as last-known-good, so that a rollback to one reports its errors again.
    degraded: BTreeMap<CandidateId, Vec<String>>,
    /// The validation errors of the active default program.
    active_degraded: Vec<String>,
    last_error: Option<String>,
    /// Whether `last_error` is the rejection of the artifact in effect composed again, which the core's status summary
    /// reports as degraded itself, so the installer does not repeat it.
    last_error_is_policy: bool,
    /// The candidate proposed for the latest configuration change: the default program, or the artifact in effect
    /// composed again.
    last_config_candidate: Option<CandidateId>,
    /// The core's actions not performed yet.
    actions: VecDeque<Action>,
    /// The acknowledgments still awaited.
    acks: FuturesUnordered<BoxFuture<'static, Acknowledgment>>,
    prepare_deadline: Option<(GenerationId, Instant)>,
    retry_at: Option<Instant>,
    draining: BTreeMap<GenerationId, Draining>,
    /// The installer's end of the artifact channel, if one is connected.
    channel: Option<ChannelEnd>,
    /// The core's artifact paths, or a test double.
    artifact_paths: Box<dyn ArtifactPaths>,
    /// The active generation's plan and explain document, once published.
    active_plan: Option<Arc<ActivePlan>>,
}

/// Waits until every region host runs and reads its mailbox, or will never run.
async fn wait_for_regions(regions: Vec<ControlSender<RegionControl>>) {
    for (slot, region) in regions.iter().enumerate() {
        if !region.running().await {
            debug!(
                slot,
                "A region of the metrics plan never ran; preparations sent to it fail as stopped."
            );
        }
    }
}

/// Waits for the next snapshot the artifact channel delivers, or never, without a channel or once its transport is
/// gone. Returns `None` when the transport goes.
async fn next_snapshot(channel: Option<&mut ChannelEnd>) -> Option<Arc<ArtifactSnapshot>> {
    match channel {
        Some(channel) if channel.open => channel.end.feed.changed().await,
        _ => std::future::pending().await,
    }
}

impl Installer {
    /// Waits for the next event.
    async fn next_event(&mut self) -> Event {
        let prepare_deadline = self.prepare_deadline;
        let retry_at = self.retry_at;
        // The generation a cut in flight replaces drains from the cut's finalization on (see `finalize_cut`).
        let replaced = self.cut.as_ref().and_then(|cut| cut.old);
        let drain_deadline = self
            .draining
            .iter()
            .filter(|(generation, draining)| !draining.overdue && Some(**generation) != replaced)
            .map(|(generation, draining)| (*generation, draining.since + limits::values::DRAIN_DEADLINE))
            .min_by_key(|(_, at)| *at);
        let poll = self
            .cut
            .as_ref()
            .map(|cut| (cut.generation, cut.next_poll, cut.affected.clone()));
        select! {
            biased;
            Some(generation) = self.retired.recv() => Event::Retired(generation),
            Some((generation, reason)) = self.failures.recv() => Event::Failed(generation, reason),
            Some(ack) = self.acks.next(), if !self.acks.is_empty() => Event::Acknowledged(ack),
            generation = at(prepare_deadline) => Event::TimedOut(generation),
            () = at(retry_at.map(|at| ((), at))) => Event::Retry,
            generation = at(drain_deadline) => Event::DrainDeadline(generation),
            (generation, polled) = poll_cut(&self.admission, &self.cut_points.every, poll) => {
                Event::CutPolled(generation, polled)
            }
            snapshot = next_snapshot(self.channel.as_mut()) => match snapshot {
                Some(snapshot) => Event::Snapshot(snapshot),
                None => Event::ChannelClosed,
            },
            config = self.config.changed() => Event::ConfigChanged(Box::new(config)),
        }
    }

    /// Reacts to an event, queueing the core's actions.
    fn handle(&mut self, event: Event) {
        let actions = match event {
            Event::Retired(generation) => {
                self.draining.remove(&generation);
                // Regions release a retired generation's program and kernels off the data path, between batches; an
                // idle region would otherwise keep them until its next batch.
                for region in &self.regions {
                    drop(region.send(RegionControl::Release));
                }
                self.core.retired(generation)
            }
            Event::Failed(generation, reason) => self.core.generation_failed(generation, &reason),
            Event::Acknowledged(ack) => self.core.prepare_acknowledged(ack.generation, ack.slot, ack.outcome),
            Event::TimedOut(generation) => {
                self.prepare_deadline = None;
                self.core.prepare_timed_out(generation)
            }
            Event::Retry => {
                self.retry_at = None;
                self.core.retry()
            }
            Event::DrainDeadline(generation) => {
                if let Some(draining) = self.draining.get_mut(&generation) {
                    draining.overdue = true;
                    warn!(
                        generation = generation.get(),
                        "A replaced generation of the metrics plan is still draining {:?} later: batches stamped with \
                         it are still in flight.",
                        limits::values::DRAIN_DEADLINE
                    );
                }
                Vec::new()
            }
            Event::CutPolled(generation, polled) => self.cut_polled(generation, polled),
            Event::Snapshot(snapshot) => {
                debug!(
                    product = %snapshot.product,
                    configs = snapshot.configs.len(),
                    "The artifact channel delivered a snapshot."
                );
                let composer = DefaultProgramComposer {
                    plan: &self.last_lowered,
                    catalog: &self.default_program.catalog,
                    registry: &self.registry,
                };
                let actions = self.artifact_paths.on_snapshot(&mut self.core, &snapshot, &composer);
                // The actions are queued in the installer's state, so a restarted worker performs them: the snapshot
                // is handled.
                if let Some(channel) = &mut self.channel {
                    channel.handled = Some(snapshot);
                }
                actions
            }
            Event::ChannelClosed => {
                if let Some(channel) = &mut self.channel {
                    channel.open = false;
                }
                debug!("The artifact channel's transport stopped. The last snapshot it delivered stays in effect.");
                Vec::new()
            }
            Event::ConfigChanged(config) => {
                self.config_changes += 1;
                self.propose_default_program(&config)
            }
        };
        self.actions.extend(actions);
    }

    /// Lowers the configuration into the default program and proposes it, returning the core's actions.
    ///
    /// While an artifact is in effect, the core composes it again against the new default program, and proposes that
    /// instead.
    fn propose_default_program(&mut self, current: &SalukiConfiguration) -> Vec<Action> {
        let lowered = match self.lowering.lower(current) {
            Ok(lowered) => lowered,
            Err(error) => {
                let error = format!("{:#}", error);
                warn!(
                    error = %error,
                    "Failed to lower the changed configuration into the default program. The active generation keeps \
                     running."
                );
                self.telemetry.installed(InstallOutcome::Rejected);
                self.last_error = Some(error);
                self.last_error_is_policy = false;
                // A candidate of an earlier change no longer stands for the configuration.
                self.last_config_candidate = None;
                return Vec::new();
            }
        };
        self.telemetry.count_binding_changes(&self.last_lowered, &lowered.plan);
        let report = Validator::new(&self.registry, &lowered.catalog).validate(&lowered.plan);
        let degraded = errors(&report);
        let plan = Arc::new(lowered.plan);
        if *plan != *self.last_lowered || lowered.catalog != self.default_program.catalog {
            self.default_program = DefaultProgramView::of(&plan, lowered.catalog, &self.labels);
        }
        self.last_lowered = Arc::clone(&plan);
        let candidate = Candidate {
            plan: Arc::clone(&plan),
            report,
            origin: Origin::DefaultProgram {
                config_change: self.config_changes,
            },
        };
        let composer = DefaultProgramComposer {
            plan: &plan,
            catalog: &self.default_program.catalog,
            registry: &self.registry,
        };
        let (candidate, actions) = self
            .artifact_paths
            .on_default_program(&mut self.core, candidate, &composer);
        self.last_config_candidate = Some(candidate);
        if !degraded.is_empty() {
            self.degraded.insert(candidate, degraded);
        }
        actions
    }

    /// Performs every queued action, in order, including those that performing them queues.
    async fn perform_actions(&mut self) {
        while let Some(action) = self.actions.pop_front() {
            self.perform(action).await;
        }
    }

    /// Performs one action. Only the actions of a cut wait: for the locks of admission points, at most the cut lock
    /// timeout.
    async fn perform(&mut self, action: Action) {
        match action {
            Action::Prepare(prepare) => self.prepare(prepare),
            Action::Abort { generation, slots } => {
                if self
                    .prepare_deadline
                    .is_some_and(|(preparing, _)| preparing == generation)
                {
                    self.prepare_deadline = None;
                }
                for slot in slots {
                    if let Some(region) = self.regions.get(slot.get()) {
                        // Aborting needs no acknowledgment: a region drops the generation, or never had it.
                        drop(region.send(RegionControl::Abort { generation }));
                    }
                }
            }
            Action::Commit { generation } => {
                let id = generation.id();
                // The commit: from this store on, every admission point stamps new batches with the generation.
                self.schedule.store(Arc::new(AdmissionSchedule::Steady(generation)));
                if self.prepare_deadline.is_some_and(|(preparing, _)| preparing == id) {
                    self.prepare_deadline = None;
                }
                self.describe_active_plan(id);
            }
            Action::RetryAfter(delay) => self.retry_at = Some(Instant::now() + delay),
            Action::InstallDefaultProgram => {
                let current = SalukiConfiguration::clone(&self.config);
                let actions = self.propose_default_program(&current);
                self.actions.extend(actions);
            }
            Action::Report(report) => self.report(report),
            Action::ArmCut(arm) => self.arm_cut(arm).await,
            Action::WatchCut { generation, at } => self.watch_cut(generation, at),
            Action::CancelCut { generation, old } => self.cancel_cut(generation, old).await,
            Action::FinalizeCut { generation } => self.finalize_cut(generation),
        }
    }

    /// Queues the cut step the core has in flight, ahead of every other action, for a worker that restarted while it
    /// may have been performing it.
    fn resume_cut(&mut self) {
        for action in self.core.resume_actions().into_iter().rev() {
            self.actions.push_front(action);
        }
    }

    /// Arms a cut over its affected admission points (see [`cut::arm`]), and reports the outcome to the core.
    async fn arm_cut(&mut self, arm: ArmCut) {
        let id = arm.generation.id();
        // Every region acknowledged the preparation, so it no longer times out: the cut may wait for its locks, and
        // then for its time, longer than any preparation does.
        if self.prepare_deadline.is_some_and(|(preparing, _)| preparing == id) {
            self.prepare_deadline = None;
        }
        let armed_before = self
            .arming
            .as_ref()
            .is_some_and(|arming| arming.generation == id && arming.armed);
        self.arming = Some(Arming {
            generation: id,
            old: arm.old.id(),
            affected: arm.affected.clone(),
            armed: armed_before,
        });

        let outcome = cut::arm(&self.admission, &self.schedule, &arm, limits::values::CUT_LOCK_TIMEOUT).await;
        let actions = match outcome {
            ArmOutcome::Armed { at } => {
                if let Some(arming) = &mut self.arming {
                    arming.armed = true;
                }
                info!(
                    generation = id.get(),
                    replaces = arm.old.id().get(),
                    at = %at,
                    "Armed the cut of a new generation of the metrics plan: batches admitted from the cut time on \
                     belong to it."
                );
                self.core.cut_armed(id, at)
            }
            ArmOutcome::Deferred(reason) => {
                debug!(
                    generation = id.get(),
                    ?reason,
                    "Arming the cut of a generation of the metrics plan was put off."
                );
                self.core.cut_deferred(id, reason)
            }
            ArmOutcome::Stranded(error) => {
                // Regions may have read the boundary the token records, so the core aborts the generation and prepares
                // its candidate again as a new one.
                warn!(
                    generation = id.get(),
                    error = %error,
                    "The cut of a generation of the metrics plan was interrupted before it was published, and can no \
                     longer be: the generation is aborted, and the change prepared again."
                );
                self.core.cut_arm_abandoned(id)
            }
            ArmOutcome::Failed(error) => {
                // Only a bug leaves a cut that cannot be armed: the core checked its widths when it planned it, and the
                // frame lists its admission points. Trying again would fail the same way, so the generation fails: the
                // core aborts it in every region, as it does a cut generation that fails before it is armed, and the
                // active generation keeps running.
                error!(
                    generation = id.get(),
                    error = %error,
                    "The cut of a generation of the metrics plan cannot be armed; this is a bug. The generation \
                     fails, and the active one keeps running."
                );
                self.core
                    .generation_failed(id, &format!("its cut cannot be armed: {}", error))
            }
        };
        self.actions.extend(actions);
    }

    /// Starts watching the cut of `generation` at `at`: the admission points are polled every `CUT_POLL` until it is
    /// finalized or cancelled.
    fn watch_cut(&mut self, generation: GenerationId, at: AdmissionTime) {
        let (old, affected) = match &self.arming {
            Some(arming) if arming.generation == generation => (Some(arming.old), arming.affected.clone()),
            // A cut armed by an earlier run of the installer than the one that remembers it: its replaced generation
            // is the one the schedule names, and it is armed over the points upstream of every isolation slot.
            _ => {
                let old = match &**self.schedule.load() {
                    AdmissionSchedule::Cut { old, new, .. } if new.id() == generation => Some(old.id()),
                    _ => None,
                };
                (old, self.cut_points.affected.clone())
            }
        };
        let overdue = self
            .cut
            .as_ref()
            .is_some_and(|watched| watched.generation == generation && watched.overdue);
        self.cut = Some(CutWatch {
            generation,
            old,
            at,
            affected,
            next_poll: Instant::now(),
            overdue,
        });
    }

    /// Reports what a poll of the admission points found about the cut in flight to the core.
    fn cut_polled(&mut self, generation: GenerationId, polled: Polled) -> Vec<Action> {
        let Some(watched) = self.cut.as_mut().filter(|watched| watched.generation == generation) else {
            return Vec::new();
        };
        watched.next_poll = Instant::now() + limits::values::CUT_POLL;
        // The replaced generation drains from the cut's finalization, which waits for every affected point to pass
        // the cut: one that has not passed it by the drain deadline after the cut time holds the replaced generation
        // in flight past it.
        let now = polled.wall.unwrap_or_else(|| SystemWallClock.now());
        let deadline = watched.at.saturating_add(limits::values::DRAIN_DEADLINE);
        if !watched.overdue && !polled.progress.all_passed() && now >= deadline {
            watched.overdue = true;
            warn!(
                generation = generation.get(),
                at = %watched.at,
                passed = polled.progress.passed,
                affected = polled.progress.affected,
                "The cut of a generation of the metrics plan is still waiting {:?} after its time for admission \
                 points to pass it: the generation it replaces is still in flight.",
                limits::values::DRAIN_DEADLINE
            );
        }
        self.core.cut_progress(generation, polled.progress)
    }

    /// Cancels the cut of `generation` (see [`cut::cancel`]), and reports the outcome to the core.
    async fn cancel_cut(&mut self, generation: GenerationId, old: GenerationRef) {
        let outcome = cut::cancel(
            &self.admission,
            &self.schedule,
            &self.cut_points.every,
            generation,
            &old,
            limits::values::CUT_LOCK_TIMEOUT,
        )
        .await;
        let actions = match outcome {
            CancelOutcome::Cancelled | CancelOutcome::AlreadyCancelled => {
                // A cut ends once: a cancellation performed again, by a restarted worker, finds it cancelled already.
                if outcome == CancelOutcome::Cancelled {
                    self.telemetry.cut(CutOutcome::Cancelled);
                    info!(
                        generation = generation.get(),
                        keeps = old.id().get(),
                        "Cancelled the cut of a generation of the metrics plan before any batch was admitted to it."
                    );
                }
                if self
                    .cut
                    .as_ref()
                    .is_some_and(|watched| watched.generation == generation)
                {
                    self.cut = None;
                }
                if self
                    .arming
                    .as_ref()
                    .is_some_and(|arming| arming.generation == generation)
                {
                    self.arming = None;
                }
                self.core.cut_cancelled(generation)
            }
            CancelOutcome::Refused(reason) => {
                self.telemetry.cut_cancellation_refused();
                info!(
                    generation = generation.get(),
                    reason = %reason,
                    "The cut of a generation of the metrics plan cannot be cancelled, and completes."
                );
                self.core.cut_cancel_refused(generation)
            }
        };
        self.actions.extend(actions);
    }

    /// Finalizes the cut of `generation`: one store of `Steady(generation)`, which every affected admission point,
    /// having passed the cut, stamps alike.
    fn finalize_cut(&mut self, generation: GenerationRef) {
        let id = generation.id();
        let replaced = match &**self.schedule.load() {
            AdmissionSchedule::Cut { old, new, .. } if new.id() == id => Some(old.id()),
            // Finalized already, by an earlier run of the installer.
            AdmissionSchedule::Steady(steady) if steady.id() == id => None,
            AdmissionSchedule::Steady(other) | AdmissionSchedule::Cut { new: other, .. } => {
                // Storing would replace another generation, or another generation's cut: never.
                error!(
                    generation = id.get(),
                    schedule = other.id().get(),
                    "The installer core asked to finalize the cut of a generation of the metrics plan that the \
                     schedule does not hold; this is a bug. The schedule is unchanged."
                );
                None
            }
        };
        if let Some(replaced) = replaced {
            self.schedule.store(Arc::new(AdmissionSchedule::Steady(generation)));
            self.telemetry.cut(CutOutcome::Completed);
            // The replaced generation's drain deadline starts now: batches stamped with it were admitted up to the cut
            // time, which every affected point has just passed.
            if let Some(draining) = self.draining.get_mut(&replaced) {
                draining.since = Instant::now();
                draining.overdue = false;
            }
            info!(
                generation = id.get(),
                replaced = replaced.get(),
                "The cut of a new generation of the metrics plan is complete."
            );
        }
        if self.cut.as_ref().is_some_and(|watched| watched.generation == id) {
            self.cut = None;
        }
        if self.arming.as_ref().is_some_and(|arming| arming.generation == id) {
            self.arming = None;
        }
    }

    /// Sets the gauges that describe the active plan, the binding gauges and the rewrites applied, from the core's
    /// active plan, for the activation of `generation`: at its commit, or, for a cut's generation, which becomes active
    /// with no commit, when the core reports it active. Whichever of the two comes first sets them, once per
    /// activation.
    fn describe_active_plan(&mut self, generation: GenerationId) {
        if self.gauges_describe == Some(generation) {
            return;
        }
        if let Some(plan) = self.core.active_plan() {
            self.telemetry.set_binding_gauges(plan);
        }
        self.telemetry.rewrites(self.core.active_rewrites());
        self.gauges_describe = Some(generation);
    }

    /// Counts a cut the core gave up on, rejecting its generation, before it was ever armed: arming was put off too
    /// many times, the cut could not be armed, or a newer proposal replaced it first.
    fn cut_given_up(&mut self, generation: GenerationId) {
        if self
            .arming
            .as_ref()
            .is_some_and(|arming| arming.generation == generation && !arming.armed)
        {
            self.arming = None;
            self.telemetry.cut(CutOutcome::NotArmed);
        }
    }

    /// Prepares a generation in every region: builds the kernels each region lacks, and sends the preparation.
    fn prepare(&mut self, prepare: Prepare) {
        let Prepare {
            generation,
            slots,
            timeout,
        } = prepare;
        let id = generation.id();
        self.prepare_deadline = Some((id, Instant::now() + timeout));
        let (budgets, artifact_nodes) = self.preparing_budgets(id);

        for slot in slots {
            let slot_id = slot.slot;
            let budget = budgets
                .get(slot_id.get())
                .copied()
                .unwrap_or(PanicBudget::DEFAULT_PROGRAM);
            let Some(region) = self.regions.get(slot_id.get()) else {
                let failure = Err("the topology has no region for the slot".to_string());
                let actions = self.core.prepare_acknowledged(id, slot_id, failure);
                self.actions.extend(actions);
                return;
            };
            match self.region_preparation(&generation, slot, budget, &artifact_nodes) {
                Ok(message) => {
                    let ack = region.send(RegionControl::Prepare(Box::new(message)));
                    self.acks.push(Box::pin(async move {
                        Acknowledgment {
                            generation: id,
                            slot: slot_id,
                            outcome: ack.await.map_err(|error| error.to_string()),
                        }
                    }));
                }
                Err(error) => {
                    // A kernel that fails to build fails the preparation at once: no other region needs it.
                    let actions = self.core.prepare_acknowledged(id, slot_id, Err(error.to_string()));
                    self.actions.extend(actions);
                    return;
                }
            }
        }
    }

    /// Returns what the region of `slot` prepares for `generation`: the slot's program, the kernels it lacks, built
    /// here, its panic budget and the nodes the budget covers, and how its state owners bind to instances of their
    /// state.
    ///
    /// # Errors
    ///
    /// Returns an error if a kernel fails to build.
    fn region_preparation(
        &self, generation: &GenerationRef, slot: SlotPrepare, budget: PanicBudget, artifact_nodes: &BTreeSet<NodeId>,
    ) -> Result<PrepareRegion, RegionError> {
        // Every part of the core's preparation of the slot is named, so that none is left behind: a region reuses the
        // instances it holds for the kernels the preparation does not list to build, whether or not the program is
        // unchanged, and an isolation slot needs its bindings of state to carry, start, or retire its state owners'
        // state.
        let SlotPrepare {
            slot: _,
            program,
            unchanged: _,
            reuse: _,
            build,
            state,
        } = slot;
        // Kernels are built off the data path, each in its own resource group under its region's.
        let kernels = build_kernels(&program, &build, |node| {
            self.region_telemetry.kernel_context(&program, node)
        })?;
        // The budget covers the artifact's nodes only: a native node's panic fails the region.
        Ok(
            PrepareRegion::new(generation.clone(), program, kernels, budget, &self.region_telemetry)
                .with_artifact_nodes(artifact_nodes.iter().cloned())
                .with_state_bindings(state),
        )
    }

    /// Returns the panic budget of every slot, in slot order, for generation `id`, which the core is preparing (see
    /// [`slot_budgets`]), and the nodes the budgets cover: those with an artifact's provenance.
    fn preparing_budgets(&self, id: GenerationId) -> (Vec<PanicBudget>, BTreeSet<NodeId>) {
        let preparing = self.core.status().preparing.filter(|preparing| preparing.id == id);
        match (preparing, self.core.preparing_plan()) {
            (Some(preparing), Some(plan)) => (
                slot_budgets(plan, &preparing.origin, &self.registry, self.core.frame()),
                artifact_nodes(plan),
            ),
            // A preparation the core replaced before it was sent: the regions' acknowledgments of it are ignored.
            _ => (
                vec![PanicBudget::DEFAULT_PROGRAM; self.core.frame().slots().len()],
                BTreeSet::new(),
            ),
        }
    }

    /// Records what happened to a candidate, in telemetry, the logs, and status.
    fn report(&mut self, report: Report) {
        let Report {
            candidate,
            origin,
            generation,
            outcome,
        } = report;
        let generation_id = generation.map(GenerationId::get);
        match outcome {
            Outcome::Unchanged => {
                self.telemetry.installed(InstallOutcome::Unchanged);
                self.degraded.remove(&candidate);
                // The plan the latest configuration change resolves to is what runs, or what is being prepared: an
                // earlier change that was not installed no longer describes the configuration in effect.
                if self.last_config_candidate == Some(candidate) {
                    self.last_error = None;
                }
                debug!(candidate = candidate.get(), %origin, "The metrics plan is unchanged.");
            }
            Outcome::Superseded => {
                self.telemetry.installed(InstallOutcome::Superseded);
                self.degraded.remove(&candidate);
                debug!(candidate = candidate.get(), %origin, "A newer metrics plan replaced a waiting one.");
            }
            Outcome::Pending(PendingReason::Retry { attempt }) => {
                self.telemetry.installed(InstallOutcome::Retried);
                info!(
                    candidate = candidate.get(),
                    %origin,
                    attempt,
                    "The metrics plan could not be prepared, and is tried again after a delay."
                );
            }
            Outcome::Pending(reason) => {
                debug!(candidate = candidate.get(), %origin, ?reason, "The metrics plan waits to be prepared.");
            }
            Outcome::Rejected(rejection) => {
                self.telemetry.installed(InstallOutcome::Rejected);
                self.degraded.remove(&candidate);
                if let Some(generation) = generation {
                    self.cut_given_up(generation);
                }
                let error = rejection.to_string();
                warn!(
                    candidate = candidate.get(),
                    %origin,
                    error = %error,
                    "The metrics plan was rejected. The active generation keeps running."
                );
                // A rejected artifact is acknowledged as such through the channel. What the status keeps is why the
                // configuration in effect is not: a default program, or the artifact in effect composed again after a
                // configuration change, that was not installed.
                if matches!(origin, Origin::DefaultProgram { .. }) || self.last_config_candidate == Some(candidate) {
                    self.last_error_is_policy = matches!(origin, Origin::Artifact { .. });
                    self.last_error = Some(error);
                }
            }
            Outcome::Generation(state) => {
                let Some(id) = generation else { return };
                match state {
                    GenerationState::Active => {
                        self.telemetry.installed(InstallOutcome::Committed);
                        // Only a default program runs despite validation errors: an artifact must validate. The errors
                        // stay with the candidate, which can be active again after a rollback to it.
                        self.active_degraded = if matches!(origin, Origin::DefaultProgram { .. }) {
                            self.degraded.get(&candidate).cloned().unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        // A cut's generation becomes active with no commit, when the first batch is admitted to it: the
                        // report sets the gauges that describe the active plan, whether or not the cut is finalized
                        // yet.
                        self.describe_active_plan(id);
                        self.last_error = None;
                        info!(generation = generation_id, %origin, "A new generation of the metrics plan is active.");
                        if !self.active_degraded.is_empty() {
                            warn!(
                                generation = generation_id,
                                errors = %self.active_degraded.join("; "),
                                "The active default program does not validate, and runs degraded."
                            );
                        }
                    }
                    GenerationState::Draining => {
                        self.draining.insert(
                            id,
                            Draining {
                                since: Instant::now(),
                                overdue: false,
                            },
                        );
                    }
                    GenerationState::Rejected | GenerationState::Failed => {
                        self.draining.remove(&id);
                        self.cut_given_up(id);
                        let reason = self
                            .core
                            .status()
                            .generations
                            .into_iter()
                            .find(|record| record.id == id)
                            .and_then(|record| record.reason);
                        let reason = reason.as_deref().unwrap_or("unknown");
                        if state == GenerationState::Failed {
                            warn!(
                                generation = generation_id,
                                %origin,
                                reason,
                                "A generation of the metrics plan failed under supervision."
                            );
                        } else {
                            warn!(
                                generation = generation_id,
                                %origin,
                                reason,
                                "A generation of the metrics plan was not installed."
                            );
                        }
                    }
                    GenerationState::Retired => {
                        self.draining.remove(&id);
                        debug!(generation = generation_id, "A generation of the metrics plan retired.");
                    }
                    GenerationState::Pending | GenerationState::Prepared => {
                        debug!(
                            generation = generation_id,
                            state = state.as_str(),
                            "Metrics plan generation."
                        );
                    }
                }
            }
        }
    }

    /// Publishes the installer's status, into `status`, its telemetry, and its acknowledgments and status summary,
    /// through the artifact channel.
    fn publish(&mut self, status: &ArcSwap<PlanStatus>) {
        let current = self.current_status();
        let active = current.installer.active.as_ref();
        self.telemetry
            .generations(active.map(|active| active.id), current.installer.live.len());
        // The artifact in effect is the one the core reports, which the active generation's origin does not always
        // say: an artifact that composes to the plan already running is served by that plan's generation.
        let artifact = current.apply_states.status.active_artifact.as_ref();
        self.telemetry
            .active_revision(artifact.map(|artifact| artifact.revision));
        if let Some(channel) = &self.channel {
            channel.end.reporter.report(current.apply_states.clone());
        }
        if **status.load() != current {
            debug!(status = %current, "The metrics plan's status changed.");
            status.store(Arc::new(current));
        }
    }

    /// Returns the installer's current status.
    fn current_status(&mut self) -> PlanStatus {
        let installer = self.core.status();
        // Only the candidates the core still holds can run again.
        let held = [&installer.active, &installer.last_known_good, &installer.preparing]
            .into_iter()
            .flatten()
            .map(|record| record.candidate)
            .chain(installer.pending.map(|(candidate, _)| candidate))
            .collect::<BTreeSet<_>>();
        self.degraded.retain(|candidate, _| held.contains(candidate));
        // A generation that a cut in flight replaces is still in flight past the deadline if the cut is overdue.
        let replaced = self
            .cut
            .as_ref()
            .filter(|watched| watched.overdue)
            .and_then(|watched| watched.old);
        let overdue = self
            .draining
            .iter()
            .filter(|(_, draining)| draining.overdue)
            .map(|(generation, _)| *generation)
            .chain(replaced)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let apply_states =
            self.artifact_paths
                .apply_states(&self.core, &self.default_program.bases, self.degraded_reason());
        PlanStatus {
            active: self.active_plan(installer.active.as_ref().map(|active| active.id)),
            installer,
            degraded: self.active_degraded.clone(),
            config_changes: self.config_changes,
            last_error: self.last_error.clone(),
            overdue,
            apply_states,
            exports: self.default_program.exports.clone(),
            default_program: Arc::clone(&self.last_lowered),
        }
    }

    /// Returns why the installer is degraded, in a few words, for the status summary: the active default program does
    /// not validate, or the last configuration change was not installed.
    ///
    /// A change the artifact in effect could not be composed again for is left out: the core adds that reason to the
    /// summary itself.
    fn degraded_reason(&self) -> Option<String> {
        if let Some(first) = self.active_degraded.first() {
            return Some(match self.active_degraded.len() - 1 {
                0 => format!("the active default program does not validate: {}", first),
                more => format!(
                    "the active default program does not validate: {} (and {} more)",
                    first, more
                ),
            });
        }
        self.last_error
            .as_ref()
            .filter(|_| !self.last_error_is_policy)
            .map(|error| format!("the last configuration change was not installed: {}", error))
    }

    /// Returns the plan and explain document of the active generation, `active`, describing it once per generation.
    fn active_plan(&mut self, active: Option<GenerationId>) -> Option<Arc<ActivePlan>> {
        let Some(generation) = active else {
            self.active_plan = None;
            return None;
        };
        if let Some(cached) = self
            .active_plan
            .as_ref()
            .filter(|cached| cached.generation == generation)
        {
            return Some(Arc::clone(cached));
        }
        let plan = Arc::clone(self.core.active_plan()?);
        let report = Validator::new(&self.registry, &self.default_program.catalog).validate(&plan);
        // The rewrites section lists what compiling the active generation considered, as the core recorded it.
        let rewrites = self.core.active_rewrites().unwrap_or_default();
        let explain = Explain::of(&plan, &self.registry, &report).with_rewrites(rewrites);
        // The physical sections describe the plan as it runs: after the logical rewrites, placed into the frame. The
        // core keeps only the programs, so the plan is compiled again, as the core compiled it: compilation is
        // deterministic, and the active plan compiled into the frame when it was installed. Its explain document goes
        // without the physical sections otherwise.
        let compiled = if report.is_valid() {
            compile(&plan, &report, &self.registry, self.core.frame())
        } else {
            compile_unvalidated(&plan, &self.registry, self.core.frame())
        };
        let explain = match compiled {
            Ok(compiled) => explain.with_physical(&compiled.plan, self.core.frame(), &compiled.placement),
            Err(error) => {
                warn!(
                    generation = generation.get(),
                    error = %error,
                    "The active plan cannot be compiled into the frame again: its explain document has no physical \
                     sections."
                );
                explain
            }
        };
        let active = Arc::new(ActivePlan {
            generation,
            plan,
            explain,
        });
        self.active_plan = Some(Arc::clone(&active));
        Some(active)
    }
}

/// Returns the nodes of `plan` with an artifact's provenance: those an artifact generation's panic budget covers.
fn artifact_nodes(plan: &Plan) -> BTreeSet<NodeId> {
    plan.nodes
        .iter()
        .filter(|node| matches!(node.provenance, Some(Provenance::Artifact(_))))
        .map(|node| node.id.clone())
        .collect()
}

/// Returns the panic budget of every slot of `frame`, in slot order, for a generation of `origin` that runs `plan`.
///
/// A default program's budget is zero in every slot. An artifact's budget applies only in the slots where `plan`
/// places a node with artifact provenance: a slot that runs native nodes only fails on a panic as the native component
/// does, so that a native kernel's panic is never charged to the artifact, nor survived silently. In a slot that runs
/// both kinds of node, the budget covers the artifact's nodes ([`artifact_nodes`]), and a native node's panic fails
/// the region all the same.
fn slot_budgets(plan: &Plan, origin: &Origin, registry: &OperatorRegistry, frame: &Frame) -> Vec<PanicBudget> {
    let budget = PanicBudget::for_origin(origin);
    let slots = frame.slots().len();
    if budget == PanicBudget::DEFAULT_PROGRAM {
        return vec![budget; slots];
    }
    let artifact_nodes = artifact_nodes(plan);
    match place_unvalidated(plan, registry, frame) {
        Ok(placement) => placement
            .slots()
            .iter()
            .map(|slot| {
                if slot.nodes.iter().any(|node| artifact_nodes.contains(node)) {
                    budget
                } else {
                    PanicBudget::DEFAULT_PROGRAM
                }
            })
            .collect(),
        // The core placed the plan into the frame to prepare it, so it places again. If it did not, nothing tells the
        // artifact's slots from the others: all of them get the artifact's budget, which still covers the artifact's
        // nodes only.
        Err(error) => {
            warn!(
                error = %error,
                "The plan being prepared cannot be placed into the frame again: every region runs it with the \
                 artifact's panic budget."
            );
            vec![budget; slots]
        }
    }
}

/// Polls the admission points once the next poll of the cut in flight is due, returning the cut's generation and what
/// the poll found, or never, without a cut in flight.
///
/// Dropping the returned future before it completes, as another event does, takes no lock: the next call polls again.
async fn poll_cut(
    registry: &AdmissionRegistry, every: &[AdmissionPointId],
    in_flight: Option<(GenerationId, Instant, Vec<AdmissionPointId>)>,
) -> (GenerationId, Polled) {
    let Some((generation, next_poll, affected)) = in_flight else {
        return std::future::pending().await;
    };
    sleep_until(next_poll).await;
    let polled = cut::poll(registry, every, &affected, limits::values::CUT_POLL).await;
    (generation, polled)
}

/// Returns the payload of a deadline once it passes, or never, without one.
async fn at<T>(deadline: Option<(T, Instant)>) -> T {
    match deadline {
        Some((payload, at)) => {
            sleep_until(at).await;
            payload
        }
        None => std::future::pending().await,
    }
}

/// Returns a report's errors, as text.
fn errors(report: &ValidationReport) -> Vec<String> {
    report.errors().map(ToString::to_string).collect()
}

mod cut;

#[cfg(test)]
mod artifact_tests;
#[cfg(test)]
mod cut_tests;
#[cfg(test)]
mod fault_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;

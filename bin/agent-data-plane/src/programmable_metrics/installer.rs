//! The installer: boots the plan-driven metrics path, and keeps it running the default program of the current
//! configuration.
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
//! performed. Standalone mode never changes configuration, so there the worker only waits.
//!
//! # Telemetry and status
//!
//! The worker publishes `plan_active_generation`, `plan_live_generations`, and `plan_installs_total{outcome}`, and the
//! native metrics that telemetry profiles say the installer records from bindings (see
//! [`InstallerTelemetry`](super::telemetry::InstallerTelemetry)). It keeps a [`PlanStatus`] current in an
//! [`ArcSwap`].

use std::{
    collections::{BTreeMap, VecDeque},
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
use saluki_core::{
    accounting::ComponentRegistry,
    components::ComponentContext,
    execution::{
        control_mailbox, AdmissionSchedule, ControlMailbox, ControlSender, GenerationId, GenerationRef, RetireNotifier,
        ScheduleHandle,
    },
    runtime::{InitializationError, Supervisable, SupervisorFuture},
    topology::{topology_identifier, ComponentId},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    compile::{Frame, RegionProgram, SlotId},
    install::{
        Action, Candidate, CandidateId, GenerationState, InstallerConfig, InstallerCore, InstallerStatus, Origin,
        Outcome, PendingReason, Prepare, Report,
    },
    ir::Plan,
    limits,
    operators::OperatorRegistry,
    runtime::{build_kernels, GenerationFailureReporter, PanicBudget, PrepareRegion, RegionControl},
    validate::{ValidationReport, Validator},
};
use tokio::{
    select,
    sync::{mpsc, Mutex},
    time::{sleep_until, Instant},
};
use tracing::{debug, info, warn};

use super::{
    host_facts::HostFacts,
    lowering::Lowering,
    outputs::NativeOutputs,
    telemetry::{InstallOutcome, InstallerTelemetry, RegionTelemetry},
    topology::slot_name,
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
}

/// The programmable metrics path of one process: what the metrics path of the topology is built from, and the
/// installer that keeps it current.
pub struct ProgrammableMetrics {
    topology: BootTopology,
    state: Arc<InstallerState>,
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
        Self::from_lowering(lowering, facts, &startup, config, component_registry).map(Some)
    }

    /// Prepares the metrics path and its installer from lowering prepared at boot, with the configuration the process
    /// started with and the view the installer wakes on.
    pub(super) fn from_lowering(
        lowering: Lowering, facts: HostFacts, startup: &SalukiConfiguration, config: Live<SalukiConfiguration>,
        component_registry: &ComponentRegistry,
    ) -> Result<Self, GenericError> {
        let lowered = lowering
            .lower(startup)
            .error_context("Failed to lower the metrics configuration into the default program.")?;
        for note in &lowered.notes {
            info!(node = %note.node, "{}", note.message);
        }
        let registry = OperatorRegistry::native();
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
        let region_telemetry =
            RegionTelemetry::new(topology_root.clone()).with_accounting(component_registry.clone(), region_identities);
        // Runs on a region's task, possibly mid-turn: it only queues.
        let (failures_tx, failures_rx) = mpsc::unbounded_channel();
        let failures: GenerationFailureReporter = Arc::new(move |generation, reason: &str| {
            let _ = failures_tx.send((generation, reason.to_string()));
        });
        let mut telemetry = InstallerTelemetry::new(topology_root);
        telemetry.set_binding_gauges(&plan);
        telemetry.register_change_counters(&plan);
        telemetry.rewrites(core.active_rewrites());

        let status = Arc::new(ArcSwap::from_pointee(PlanStatus {
            installer: core.status(),
            degraded: degraded.clone(),
            config_changes: 0,
            last_error: None,
            overdue: Vec::new(),
        }));
        let installer = Installer {
            core,
            lowering,
            registry,
            schedule: Arc::clone(&schedule),
            regions,
            region_telemetry: region_telemetry.clone(),
            telemetry,
            retired: retired_rx,
            failures: failures_rx,
            config,
            config_changes: 0,
            last_lowered: Arc::clone(&plan),
            degraded: BTreeMap::new(),
            active_degraded: degraded,
            last_error: None,
            actions: VecDeque::new(),
            acks: FuturesUnordered::new(),
            prepare_deadline: None,
            retry_at: None,
            draining: BTreeMap::new(),
        };

        Ok(Self {
            topology: BootTopology {
                frame,
                plan,
                registry,
                outputs: installer.lowering.outputs().clone(),
                facts,
                generation: boot.generation,
                programs: boot.programs,
                schedule,
                mailboxes,
                telemetry: region_telemetry,
                failures,
            },
            state: Arc::new(InstallerState {
                installer: Mutex::new(installer),
                status,
            }),
        })
    }

    /// Returns what the metrics path of the topology is built from.
    pub(super) fn boot_topology(&mut self) -> &mut BootTopology {
        &mut self.topology
    }

    /// Returns the installer worker, to run once the topology is built.
    pub fn into_installer(self) -> InstallerWorker {
        InstallerWorker { state: self.state }
    }
}

/// The installer's view of the metrics plan, for status reporting.
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
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.installer.active {
            Some(active) => write!(f, "generation {} active", active.id)?,
            None => f.write_str("no generation active")?,
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
        if self.installer.pending.is_some() {
            f.write_str(", a candidate pending")?;
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

/// The supervised worker that installs new generations of the metrics plan as the configuration changes.
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
            // A topology builds its region hosts long before it runs them: it waits for its environment to be ready
            // first, which can take a while. A preparation sent before a region reads its mailbox would only time
            // out, so the installer starts once every region runs, and installs the configuration current then.
            select! {
                biased;
                _ = &mut shutdown => return Ok(()),
                () = wait_for_regions(installer.regions.clone()) => {}
            }
            loop {
                installer.perform_actions();
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

    /// The configuration changed.
    ConfigChanged(Box<SalukiConfiguration>),
}

/// A generation that is draining: superseded, with batches stamped with it possibly still in flight.
struct Draining {
    since: Instant,
    overdue: bool,
}

/// The installer: its core and everything it needs to perform the core's actions.
struct Installer {
    core: InstallerCore,
    lowering: Lowering,
    registry: OperatorRegistry,
    schedule: ScheduleHandle,
    /// The sending side of every region's control mailbox, in slot order.
    regions: Vec<ControlSender<RegionControl>>,
    region_telemetry: RegionTelemetry,
    telemetry: InstallerTelemetry,
    retired: mpsc::UnboundedReceiver<GenerationId>,
    failures: mpsc::UnboundedReceiver<(GenerationId, String)>,
    /// The configuration view the installer wakes on.
    config: Live<SalukiConfiguration>,
    config_changes: u64,
    /// The last default program lowered, against which a change's binding changes are counted.
    last_lowered: Arc<Plan>,
    /// The validation errors of default-program candidates not installed yet, by candidate.
    degraded: BTreeMap<CandidateId, Vec<String>>,
    /// The validation errors of the active default program.
    active_degraded: Vec<String>,
    last_error: Option<String>,
    /// The core's actions not performed yet.
    actions: VecDeque<Action>,
    /// The acknowledgments still awaited.
    acks: FuturesUnordered<BoxFuture<'static, Acknowledgment>>,
    prepare_deadline: Option<(GenerationId, Instant)>,
    retry_at: Option<Instant>,
    draining: BTreeMap<GenerationId, Draining>,
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

impl Installer {
    /// Waits for the next event.
    async fn next_event(&mut self) -> Event {
        let prepare_deadline = self.prepare_deadline;
        let retry_at = self.retry_at;
        let drain_deadline = self
            .draining
            .iter()
            .filter(|(_, draining)| !draining.overdue)
            .map(|(generation, draining)| (*generation, draining.since + limits::values::DRAIN_DEADLINE))
            .min_by_key(|(_, at)| *at);
        select! {
            biased;
            Some(generation) = self.retired.recv() => Event::Retired(generation),
            Some((generation, reason)) = self.failures.recv() => Event::Failed(generation, reason),
            Some(ack) = self.acks.next(), if !self.acks.is_empty() => Event::Acknowledged(ack),
            generation = at(prepare_deadline) => Event::TimedOut(generation),
            () = at(retry_at.map(|at| ((), at))) => Event::Retry,
            generation = at(drain_deadline) => Event::DrainDeadline(generation),
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
            Event::ConfigChanged(config) => {
                self.config_changes += 1;
                self.propose_default_program(&config)
            }
        };
        self.actions.extend(actions);
    }

    /// Lowers the configuration into the default program and proposes it, returning the core's actions.
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
                return Vec::new();
            }
        };
        self.telemetry.count_binding_changes(&self.last_lowered, &lowered.plan);
        let report = Validator::new(&self.registry, &lowered.catalog).validate(&lowered.plan);
        let degraded = errors(&report);
        let plan = Arc::new(lowered.plan);
        self.last_lowered = Arc::clone(&plan);
        let (candidate, actions) = self.core.propose(Candidate {
            plan,
            report,
            origin: Origin::DefaultProgram {
                config_change: self.config_changes,
            },
        });
        if !degraded.is_empty() {
            self.degraded.insert(candidate, degraded);
        }
        actions
    }

    /// Performs every queued action, in order, including those that performing them queues.
    fn perform_actions(&mut self) {
        while let Some(action) = self.actions.pop_front() {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: Action) {
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
                if let Some(plan) = self.core.active_plan() {
                    self.telemetry.set_binding_gauges(plan);
                }
                self.telemetry.rewrites(self.core.active_rewrites());
            }
            Action::RetryAfter(delay) => self.retry_at = Some(Instant::now() + delay),
            Action::InstallDefaultProgram => {
                let current = SalukiConfiguration::clone(&self.config);
                let actions = self.propose_default_program(&current);
                self.actions.extend(actions);
            }
            Action::Report(report) => self.report(report),
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
        let budget = self
            .core
            .status()
            .preparing
            .filter(|preparing| preparing.id == id)
            .map_or(PanicBudget::DEFAULT_PROGRAM, |preparing| {
                PanicBudget::for_origin(&preparing.origin)
            });

        for slot in slots {
            let Some(region) = self.regions.get(slot.slot.get()) else {
                let failure = Err("the topology has no region for the slot".to_string());
                let actions = self.core.prepare_acknowledged(id, slot.slot, failure);
                self.actions.extend(actions);
                return;
            };
            // Kernels are built off the data path, each in its own resource group under its region's.
            let kernels = build_kernels(&slot.program, &slot.build, |node| {
                self.region_telemetry.kernel_context(&slot.program, node)
            });
            match kernels {
                Ok(kernels) => {
                    let message = PrepareRegion::new(
                        generation.clone(),
                        slot.program,
                        kernels,
                        budget,
                        &self.region_telemetry,
                    );
                    let ack = region.send(RegionControl::Prepare(Box::new(message)));
                    let slot = slot.slot;
                    self.acks.push(Box::pin(async move {
                        Acknowledgment {
                            generation: id,
                            slot,
                            outcome: ack.await.map_err(|error| error.to_string()),
                        }
                    }));
                }
                Err(error) => {
                    // A kernel that fails to build fails the preparation at once: no other region needs it.
                    let actions = self.core.prepare_acknowledged(id, slot.slot, Err(error.to_string()));
                    self.actions.extend(actions);
                    return;
                }
            }
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
                let error = rejection.to_string();
                warn!(
                    candidate = candidate.get(),
                    %origin,
                    error = %error,
                    "The metrics plan was rejected. The active generation keeps running."
                );
                self.last_error = Some(error);
            }
            Outcome::Generation(state) => {
                let Some(id) = generation else { return };
                match state {
                    GenerationState::Active => {
                        self.telemetry.installed(InstallOutcome::Committed);
                        self.active_degraded = self.degraded.remove(&candidate).unwrap_or_default();
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
                        let reason = self
                            .core
                            .status()
                            .generations
                            .into_iter()
                            .find(|record| record.id == id)
                            .and_then(|record| record.reason);
                        warn!(
                            generation = generation_id,
                            %origin,
                            state = state.as_str(),
                            reason = reason.as_deref().unwrap_or("unknown"),
                            "A generation of the metrics plan was not installed."
                        );
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

    /// Publishes the installer's status, into `status`, and its telemetry.
    fn publish(&mut self, status: &ArcSwap<PlanStatus>) {
        let installer = self.core.status();
        self.telemetry
            .generations(installer.active.as_ref().map(|active| active.id), installer.live.len());
        let overdue = self
            .draining
            .iter()
            .filter(|(_, draining)| draining.overdue)
            .map(|(generation, _)| *generation)
            .collect();
        let current = PlanStatus {
            installer,
            degraded: self.active_degraded.clone(),
            config_changes: self.config_changes,
            last_error: self.last_error.clone(),
            overdue,
        };
        if **status.load() != current {
            debug!(status = %current, "The metrics plan's status changed.");
            status.store(Arc::new(current));
        }
    }
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

#[cfg(test)]
mod fault_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;

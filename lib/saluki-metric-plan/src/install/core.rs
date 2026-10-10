//! The installer's decisions, without I/O.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{GenerationId, GenerationIdAllocator, GenerationRef, GenerationToken, RetireNotifier};

use super::{
    diff::{diff, kernel_reuse},
    lifecycle::{GenerationState, LifecycleEvent},
};
use crate::{
    canonical::{PlanHash, ResolvedValues},
    compile::{
        compile, compile_unvalidated, CompileError, CompiledPlan, Frame, KernelKey, PlacementError, RegionProgram,
        RewriteRecord, SlotId,
    },
    extension::TransitionClass,
    ir::{NodeId, Plan},
    limits,
    operators::OperatorRegistry,
    validate::ValidationReport,
};

/// The most generations whose final state the installer remembers, beyond those still alive.
const HISTORY: usize = 16;

/// Where a candidate plan came from.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Origin {
    /// Lowered from native configuration: the default program, after the given configuration change, counted per
    /// process.
    DefaultProgram {
        /// The configuration change it was lowered after.
        config_change: u64,
    },

    /// Composed from an approved artifact revision.
    Artifact {
        /// The artifact's revision.
        revision: u64,

        /// The artifact's hash.
        hash: PlanHash,
    },
}

impl Origin {
    /// Returns `true` if a generation of this origin can fail under supervision and roll back: only artifact
    /// generations can. A default-program generation's panic budget is zero, so a panic fails its component, as a
    /// native component failure does.
    pub const fn can_roll_back(&self) -> bool {
        matches!(self, Self::Artifact { .. })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultProgram { config_change } => {
                write!(f, "the default program after configuration change {}", config_change)
            }
            Self::Artifact { revision, hash } => write!(f, "artifact revision {} ({})", revision, hash),
        }
    }
}

/// A candidate plan, validated by the caller against its current catalog.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The plan, with every configuration binding resolved.
    pub plan: Arc<Plan>,

    /// The plan's validation report.
    ///
    /// An artifact whose report has errors is rejected. The default program is installed even then, as long as it is
    /// structurally sound and fits the frame: it is what native configuration says, and nothing else could run in its
    /// place, so the caller reports it degraded instead.
    pub report: ValidationReport,

    /// Where the plan came from.
    pub origin: Origin,
}

/// The identifier of a candidate, in the order the installer received them.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CandidateId(u64);

impl CandidateId {
    /// Returns the numeric value of the identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for CandidateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// How the installer schedules its work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InstallerConfig {
    /// The most generations alive at once: active, last-known-good, preparing, and draining together. A candidate that
    /// would exceed it waits until a generation retires. Defaults to the limits table's overlap budget, 4.
    pub overlap_budget: usize,

    /// How long every region has to acknowledge a preparation. Defaults to the limits table's prepare timeout, 10 s.
    pub prepare_timeout: Duration,

    /// How long a default-program candidate whose preparation failed waits before it is tried again, the first time.
    /// Each further failure doubles it, up to [`retry_max`][Self::retry_max]. Defaults to 1 second.
    pub retry_initial: Duration,

    /// The longest wait before trying a failed default-program candidate again. Defaults to 30 seconds.
    pub retry_max: Duration,
}

impl Default for InstallerConfig {
    fn default() -> Self {
        Self {
            overlap_budget: usize::try_from(limits::values::OVERLAP_BUDGET).unwrap_or(usize::MAX),
            prepare_timeout: limits::values::PREPARE_TIMEOUT,
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
        }
    }
}

impl InstallerConfig {
    /// Returns how long to wait before attempt number `attempt` (from 1) of a failed preparation.
    pub fn retry_delay(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(31);
        self.retry_initial
            .checked_mul(1 << doublings)
            .unwrap_or(self.retry_max)
            .min(self.retry_max)
    }
}

/// What one region must prepare for a generation.
#[derive(Clone, Debug)]
pub struct SlotPrepare {
    /// The slot.
    pub slot: SlotId,

    /// The slot's program for the generation. When it is unchanged, this is the active generation's program itself.
    pub program: Arc<RegionProgram>,

    /// Whether the program is the active generation's, so the region can alias it.
    pub unchanged: bool,

    /// The program's kernels that the active generation's program in this slot already has: the region reuses its
    /// instances.
    pub reuse: Vec<KernelKey>,

    /// The program's kernels to build, off the data path, from their specs in the program, and hand to the region with
    /// the program.
    pub build: Vec<KernelKey>,
}

/// A preparation: what every region must prepare for one generation, and how long they have.
#[derive(Clone, Debug)]
pub struct Prepare {
    /// The generation. Regions keep it behind a weak reference, so that the generation retires once nothing else
    /// holds it, and release its programs then.
    pub generation: GenerationRef,

    /// What each slot prepares, in slot order: every slot of the frame, unchanged ones included.
    pub slots: Vec<SlotPrepare>,

    /// How long the regions have to acknowledge, after which the caller reports [`InstallerCore::prepare_timed_out`].
    pub timeout: Duration,
}

/// Something the caller must do.
#[derive(Clone, Debug)]
pub enum Action {
    /// Send each slot's preparation to its region, and report each acknowledgment with
    /// [`InstallerCore::prepare_acknowledged`].
    Prepare(Prepare),

    /// Tell each listed region to drop the generation's programs. Only an uncommitted generation is aborted.
    Abort {
        /// The generation.
        generation: GenerationId,

        /// The slots whose regions were sent the preparation.
        slots: Vec<SlotId>,
    },

    /// Store `AdmissionSchedule::Steady(generation)` in the schedule handle of the topology, before handling anything
    /// else.
    ///
    /// The store is the commit: one store, irrevocable. The installer counts the generation as active from the moment
    /// it returns this action.
    Commit {
        /// The generation every admission point stamps from now on.
        generation: GenerationRef,
    },

    /// Call [`InstallerCore::retry`] after the delay.
    RetryAfter(Duration),

    /// Lower and propose the default program: an artifact generation failed and nothing is kept to roll back to.
    InstallDefaultProgram,

    /// Report what happened to a candidate, in status, telemetry, and acknowledgments.
    Report(Report),
}

/// What happened to a candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The candidate.
    pub candidate: CandidateId,

    /// Where it came from.
    pub origin: Origin,

    /// Its generation, once it has one.
    pub generation: Option<GenerationId>,

    /// What happened.
    pub outcome: Outcome,
}

/// One thing that happened to a candidate.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The candidate's plan equals the one that is active or being prepared: no generation is created.
    Unchanged,

    /// The candidate waits to be prepared.
    Pending(PendingReason),

    /// A newer candidate replaced the candidate before it was prepared.
    Superseded,

    /// The candidate's generation moved to a new state.
    Generation(GenerationState),

    /// The candidate was rejected; the active generation keeps running.
    Rejected(Rejection),
}

/// Why a candidate waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingReason {
    /// Another generation is being prepared.
    Preparing,

    /// Preparing it would exceed the overlap budget; it waits for a generation to retire.
    OverlapBudget {
        /// The generations alive.
        live: usize,

        /// The budget.
        budget: usize,
    },

    /// Its preparation failed, and it is tried again after a delay.
    Retry {
        /// The number of the next attempt, from 2.
        attempt: u32,
    },

    /// Another candidate's preparation failed, and it waits for that candidate's retry delay, so that a preparation
    /// that keeps failing is not tried in a tight loop.
    Backoff,
}

/// Why a candidate was rejected.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    /// The artifact's revision is not newer than the newest revision seen.
    Stale {
        /// The candidate's revision.
        revision: u64,

        /// The newest revision seen.
        high_water: u64,
    },

    /// The plan has no canonical form, so it has no hash.
    Unhashable {
        /// Why.
        message: String,
    },

    /// The artifact's plan is not valid.
    Invalid {
        /// The validation errors.
        problems: Vec<String>,
    },

    /// The plan cannot run in the physical structure built at boot.
    Unplaceable(PlacementError),

    /// The plan's region programs cannot be compiled.
    Compile {
        /// Why.
        message: String,
    },

    /// A state owner cannot move to the candidate: its transition class is `cut`, which needs state transitions
    /// (stage 6), or `reject`.
    Transition {
        /// The state owner.
        node: NodeId,

        /// Its transition class.
        class: TransitionClass,
    },

    /// A region failed to prepare, or not every region acknowledged in time.
    Prepare {
        /// Why.
        message: String,
    },

    /// A newer candidate cancelled the preparation before it committed.
    Cancelled,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale { revision, high_water } => write!(
                f,
                "revision {} is stale: revision {} has already been seen, and revisions must increase",
                revision, high_water
            ),
            Self::Unhashable { message } => write!(f, "the plan has no canonical form: {}", message),
            Self::Invalid { problems } => write!(f, "the plan is not valid: {}", problems.join("; ")),
            Self::Unplaceable(error) => error.fmt(f),
            Self::Compile { message } => f.write_str(message),
            Self::Transition {
                node,
                class: TransitionClass::Cut,
            } => write!(
                f,
                "state owner `{}` would change its time contract, which needs a cut at a bucket boundary: that \
                 requires stage 6 state transitions",
                node
            ),
            Self::Transition { node, class } => write!(
                f,
                "state owner `{}` cannot keep its state across the change (transition class `{}`)",
                node,
                class.as_str()
            ),
            Self::Prepare { message } => write!(f, "preparation failed: {}", message),
            Self::Cancelled => f.write_str("a newer candidate cancelled the preparation"),
        }
    }
}

/// What the topology needs from the boot generation, before it starts.
#[derive(Clone, Debug)]
pub struct Boot {
    /// The boot generation: the schedule of the topology starts as `AdmissionSchedule::Steady(generation)`.
    pub generation: GenerationRef,

    /// The program of every slot, in slot order, which each region host starts with, its kernels built from their
    /// specs.
    pub programs: Vec<Arc<RegionProgram>>,
}

/// What the installer knows about one generation.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationRecord {
    /// The generation.
    pub id: GenerationId,

    /// The candidate it was created for.
    pub candidate: CandidateId,

    /// Where its plan came from.
    pub origin: Origin,

    /// Its plan's hash, with resolved values.
    pub plan_hash: PlanHash,

    /// Its state.
    pub state: GenerationState,

    /// Why it was rejected or failed, if it was.
    pub reason: Option<String>,
}

/// A snapshot of the installer's state, for status and telemetry.
#[derive(Clone, Debug, PartialEq)]
pub struct InstallerStatus {
    /// The active generation.
    pub active: Option<GenerationRecord>,

    /// The generation kept prepared to roll back to.
    pub last_known_good: Option<GenerationRecord>,

    /// The generation being prepared.
    pub preparing: Option<GenerationRecord>,

    /// The candidate waiting to be prepared.
    pub pending: Option<(CandidateId, Origin)>,

    /// Every generation alive, in ID order: their tokens have not been dropped yet.
    pub live: Vec<GenerationId>,

    /// The newest artifact revision seen.
    pub revision_high_water: Option<u64>,

    /// Every generation the installer remembers, in ID order: those alive, and the most recent others.
    pub generations: Vec<GenerationRecord>,
}

/// A generation the installer holds: preparing, active, or kept as last-known-good.
#[derive(Clone)]
struct Installed {
    generation: GenerationRef,
    candidate: CandidateId,
    origin: Origin,
    plan: Arc<Plan>,
    plan_hash: PlanHash,
    programs: Vec<Arc<RegionProgram>>,
    /// The rewrites compilation considered, applied or skipped.
    rewrites: Arc<[RewriteRecord]>,
}

/// A candidate ready to be prepared.
struct Queued {
    candidate: CandidateId,
    origin: Origin,
    plan: Arc<Plan>,
    plan_hash: PlanHash,
    compiled: CompiledPlan,
    /// The preparations attempted so far.
    attempts: u32,
    /// The reason it was last reported pending for, so that it is reported once per reason.
    reported: Option<PendingReason>,
}

struct Preparation {
    installed: Installed,
    compiled: CompiledPlan,
    attempts: u32,
    waiting: BTreeSet<SlotId>,
}

type NotifierFactory = dyn Fn(GenerationId) -> RetireNotifier + Send + Sync;

/// The installer's state and decisions, without I/O.
///
/// The core turns candidate plans into generations: it places and compiles each candidate into the frame, diffs it
/// with the active plan, and decides what every region prepares, when the schedule commits, and what waits. The
/// caller performs the [actions][Action] it returns, in order, and reports what happens: acknowledgments, timeouts,
/// retirements, and failures. One task drives the core.
///
/// # Installation
///
/// A candidate is prepared in every region at once: the core sends a [`Prepare`] to every slot, with its program and
/// the kernels to reuse or build, and waits for every region to acknowledge. When all have, it commits with one
/// schedule store ([`Action::Commit`]). When one fails or the preparation times out, it aborts in every region: an
/// artifact is rejected, and a default-program candidate is tried again after a delay, unless a newer candidate
/// replaced it, so that native configuration is applied eventually.
///
/// Only one preparation runs at a time, and only the latest candidate waits: a newer default-program candidate
/// replaces a waiting one, and a newer artifact also cancels an uncommitted preparation. A candidate equal to the
/// plan being prepared, or to the active one when nothing is, changes nothing.
///
/// # Overlap budget
///
/// The core counts the generations alive through their tokens: a generation is alive from its preparation until
/// [`retired`][Self::retired] reports that its token was dropped. A preparation starts only while fewer generations
/// than the budget are alive; otherwise the candidate waits for a retirement. Nothing is dropped to make room.
///
/// # Last-known-good and failure
///
/// When an artifact generation commits, the generation it replaces stays prepared as last-known-good, so rolling back
/// to it is one schedule store; when a default-program generation commits, none is kept. When supervision fails the
/// active artifact generation, the core rolls back to last-known-good, or asks for the default program when there is
/// none. A failed generation stays failed: it is never kept to roll back to, and when a newer generation replaces it,
/// the generation kept before, if any, stays kept. A failure never changes the revision high-water mark.
pub struct InstallerCore {
    frame: Arc<Frame>,
    registry: OperatorRegistry,
    config: InstallerConfig,
    ids: GenerationIdAllocator,
    notifier: Box<NotifierFactory>,
    next_candidate: u64,
    high_water: Option<u64>,
    active: Option<Installed>,
    last_known_good: Option<Installed>,
    preparing: Option<Preparation>,
    pending: Option<Queued>,
    retry_scheduled: bool,
    live: BTreeSet<GenerationId>,
    records: BTreeMap<GenerationId, GenerationRecord>,
}

impl fmt::Debug for InstallerCore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallerCore").field("status", &self.status()).finish()
    }
}

impl InstallerCore {
    /// Creates the core and the boot generation.
    ///
    /// The boot candidate, normally the default program the frame was computed from, becomes active at once: the
    /// topology starts with its programs and with the schedule `Steady(generation)`, so there is nothing to prepare.
    /// `notifier` creates the retirement notifier of each generation's token, which must lead to a call to
    /// [`retired`][Self::retired].
    ///
    /// # Errors
    ///
    /// Returns the rejection if the boot candidate cannot be compiled into the frame, or if it is an artifact that is
    /// not valid.
    pub fn new<N>(
        frame: Arc<Frame>, registry: OperatorRegistry, config: InstallerConfig, notifier: N, boot: Candidate,
    ) -> Result<(Self, Boot), Rejection>
    where
        N: Fn(GenerationId) -> RetireNotifier + Send + Sync + 'static,
    {
        let mut core = Self {
            frame,
            registry,
            config,
            ids: GenerationIdAllocator::new(),
            notifier: Box::new(notifier),
            next_candidate: 0,
            high_water: None,
            active: None,
            last_known_good: None,
            preparing: None,
            pending: None,
            retry_scheduled: false,
            live: BTreeSet::new(),
            records: BTreeMap::new(),
        };
        let candidate = core.candidate_id();
        if let Origin::Artifact { revision, .. } = boot.origin {
            core.high_water = Some(revision);
        }
        let queued = core.queue(candidate, boot)?;
        let generation = core.generation(&queued);
        let id = generation.id();
        for event in [LifecycleEvent::BarrierPassed, LifecycleEvent::Committed] {
            core.transition(id, event, None);
        }
        let programs = queued.compiled.programs.clone();
        core.active = Some(Installed {
            generation: generation.clone(),
            candidate,
            origin: queued.origin,
            plan: queued.plan,
            plan_hash: queued.plan_hash,
            programs: programs.clone(),
            rewrites: queued.compiled.rewrites.into(),
        });
        Ok((core, Boot { generation, programs }))
    }

    /// Returns the frame the core places candidates into.
    pub fn frame(&self) -> &Arc<Frame> {
        &self.frame
    }

    /// Returns the active plan.
    pub fn active_plan(&self) -> Option<&Arc<Plan>> {
        self.active.as_ref().map(|active| &active.plan)
    }

    /// Returns the rewrites compilation considered for the active plan, applied or skipped, as
    /// [`CompiledPlan::rewrites`] lists them: for `explain` and the count of applied rewrites.
    pub fn active_rewrites(&self) -> Option<&[RewriteRecord]> {
        self.active.as_ref().map(|active| &*active.rewrites)
    }

    /// Returns the active generation's program of every slot, in slot order.
    pub fn active_programs(&self) -> Option<&[Arc<RegionProgram>]> {
        self.active.as_ref().map(|active| active.programs.as_slice())
    }

    /// Returns the installer's state.
    pub fn status(&self) -> InstallerStatus {
        let record = |installed: Option<&Installed>| {
            installed.and_then(|installed| self.records.get(&installed.generation.id()).cloned())
        };
        InstallerStatus {
            active: record(self.active.as_ref()),
            last_known_good: record(self.last_known_good.as_ref()),
            preparing: record(self.preparing.as_ref().map(|preparing| &preparing.installed)),
            pending: self.pending.as_ref().map(|queued| (queued.candidate, queued.origin)),
            live: self.live.iter().copied().collect(),
            revision_high_water: self.high_water,
            generations: self.records.values().cloned().collect(),
        }
    }

    /// Proposes a candidate plan, returning its identifier and what to do.
    pub fn propose(&mut self, candidate: Candidate) -> (CandidateId, Vec<Action>) {
        let id = self.candidate_id();
        let origin = candidate.origin;
        let mut actions = Vec::new();
        let report = |outcome| {
            Action::Report(Report {
                candidate: id,
                origin,
                generation: None,
                outcome,
            })
        };

        if let Origin::Artifact { revision, .. } = origin {
            if let Some(high_water) = self.high_water.filter(|high_water| revision <= *high_water) {
                actions.push(report(Outcome::Rejected(Rejection::Stale { revision, high_water })));
                return (id, actions);
            }
            self.high_water = Some(revision);
        }

        // Nothing changes if the candidate is what the schedule will hold once the current preparation, if any, ends. A
        // generation that supervision failed is not a state to keep: a candidate equal to it is installed anew.
        let target = match &self.preparing {
            Some(preparing) => Some(preparing.installed.plan_hash),
            None => self
                .active
                .as_ref()
                .filter(|active| !self.has_failed(active.generation.id()))
                .map(|active| active.plan_hash),
        };
        let plan_hash = candidate.plan.hash(ResolvedValues::Include).ok();
        if plan_hash.is_some() && plan_hash == target {
            if let Some(replaced) = self.pending.take() {
                actions.push(Self::report(&replaced, Outcome::Superseded));
            }
            actions.push(report(Outcome::Unchanged));
            return (id, actions);
        }

        let queued = match self.queue(id, candidate) {
            Ok(queued) => queued,
            Err(rejection) => {
                actions.push(report(Outcome::Rejected(rejection)));
                return (id, actions);
            }
        };
        if let Some(replaced) = self.pending.replace(queued) {
            actions.push(Self::report(&replaced, Outcome::Superseded));
        }
        if origin.can_roll_back() && self.preparing.is_some() {
            self.cancel_preparation(&mut actions);
        }
        self.try_start(&mut actions);
        (id, actions)
    }

    /// Reports one region's acknowledgment of a preparation: `Ok` if it holds the generation's program, or the error it
    /// failed with.
    ///
    /// Acknowledgments of a generation that is no longer being prepared are ignored.
    pub fn prepare_acknowledged(
        &mut self, generation: GenerationId, slot: SlotId, outcome: Result<(), String>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(preparing) = self
            .preparing
            .as_mut()
            .filter(|preparing| preparing.installed.generation.id() == generation)
        else {
            return actions;
        };
        match outcome {
            Ok(()) => {
                preparing.waiting.remove(&slot);
                if preparing.waiting.is_empty() {
                    self.commit(&mut actions);
                }
            }
            Err(message) => {
                let message = format!("region `{}` failed to prepare: {}", self.slot_name(slot), message);
                self.fail_preparation(message, GenerationState::Rejected, &mut actions);
            }
        }
        actions
    }

    /// Reports that a preparation's timeout passed.
    ///
    /// A timeout of a generation that is no longer being prepared is ignored.
    pub fn prepare_timed_out(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(preparing) = self
            .preparing
            .as_ref()
            .filter(|preparing| preparing.installed.generation.id() == generation)
        else {
            return actions;
        };
        let missing = preparing
            .waiting
            .iter()
            .map(|slot| format!("`{}`", self.slot_name(*slot)))
            .collect::<Vec<_>>();
        let message = format!(
            "the regions {} did not acknowledge within {:?}",
            missing.join(", "),
            self.config.prepare_timeout
        );
        self.fail_preparation(message, GenerationState::Rejected, &mut actions);
        actions
    }

    /// Reports that the delay of an [`Action::RetryAfter`] passed.
    pub fn retry(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        self.retry_scheduled = false;
        self.try_start(&mut actions);
        actions
    }

    /// Reports that a generation's token was dropped: nothing references it any more.
    pub fn retired(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        self.live.remove(&generation);
        if self.records.get(&generation).map(|record| record.state) == Some(GenerationState::Draining) {
            self.transition(generation, LifecycleEvent::Retired, None);
            if let Some(record) = self.records.get(&generation) {
                actions.push(Action::Report(Report {
                    candidate: record.candidate,
                    origin: record.origin,
                    generation: Some(generation),
                    outcome: Outcome::Generation(GenerationState::Retired),
                }));
            }
        }
        self.prune();
        self.try_start(&mut actions);
        actions
    }

    /// Reports that supervision failed a generation: it exceeded its panic budget.
    ///
    /// A failed active artifact generation rolls back to last-known-good with one schedule store, or, without one,
    /// asks for the default program. Rolling back first cancels any preparation in flight, which was diffed against
    /// the failed generation and reuses its kernels, and prepares its candidate again against the generation that is
    /// active after the rollback. A failed default-program generation changes nothing here: its component failed, as a
    /// native component does. A preparing generation is aborted; a kept or draining one is marked failed, and a kept
    /// one is no longer kept.
    ///
    /// A failed generation stays active until another commits, since the schedule still stamps it, but it is never
    /// kept to roll back to, and a candidate equal to it is not reported unchanged.
    pub fn generation_failed(&mut self, generation: GenerationId, reason: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        let reason = reason.to_string();
        let is = |installed: &Option<Installed>| {
            installed
                .as_ref()
                .is_some_and(|installed| installed.generation.id() == generation)
        };

        if self
            .preparing
            .as_ref()
            .is_some_and(|preparing| preparing.installed.generation.id() == generation)
        {
            self.fail_preparation(reason, GenerationState::Failed, &mut actions);
            return actions;
        }
        if is(&self.active) {
            if self.has_failed(generation) {
                return actions;
            }
            self.transition(generation, LifecycleEvent::Failed, Some(reason));
            self.report_generation(generation, &mut actions);
            let can_roll_back = self.active.as_ref().is_some_and(|active| active.origin.can_roll_back());
            if !can_roll_back {
                return actions;
            }
            // Only a generation every region still holds prepared can be rolled back to with one store.
            let kept = self
                .last_known_good
                .take()
                .filter(|kept| self.state_of(kept.generation.id()) == Some(GenerationState::Prepared));
            match kept {
                Some(kept) => {
                    // The preparation in flight, if any, was diffed against the failed generation and reuses kernels
                    // that only the failed generation may hold once its batches drain: prepare its candidate again,
                    // against the generation rolled back to.
                    self.restart_preparation(&mut actions);
                    let id = kept.generation.id();
                    self.transition(id, LifecycleEvent::Committed, None);
                    actions.push(Action::Commit {
                        generation: kept.generation.clone(),
                    });
                    self.report_generation(id, &mut actions);
                    // The failed generation is dropped here: the core no longer holds it.
                    self.active = Some(kept);
                    self.prune();
                    self.try_start(&mut actions);
                }
                // The failed generation stays active, and holds its kernels, until a new one commits.
                None => actions.push(Action::InstallDefaultProgram),
            }
            return actions;
        }
        if is(&self.last_known_good) {
            self.last_known_good = None;
        }
        let state = self.records.get(&generation).map(|record| record.state);
        if state.is_some_and(|state| !state.is_final()) {
            self.transition(generation, LifecycleEvent::Failed, Some(reason));
            self.report_generation(generation, &mut actions);
        }
        actions
    }

    fn candidate_id(&mut self) -> CandidateId {
        self.next_candidate += 1;
        CandidateId(self.next_candidate)
    }

    fn slot_name(&self, slot: SlotId) -> String {
        self.frame
            .slot(slot)
            .map_or_else(|| slot.to_string(), |slot| slot.name.to_string())
    }

    /// Validates, places, and compiles a candidate.
    fn queue(&self, id: CandidateId, candidate: Candidate) -> Result<Queued, Rejection> {
        let plan_hash = candidate
            .plan
            .hash(ResolvedValues::Include)
            .map_err(|error| Rejection::Unhashable {
                message: error.to_string(),
            })?;
        // The default program runs even when validation reports errors in it, reported degraded by the caller: it is
        // what native configuration says, and nothing else could run in its place. An artifact must validate.
        let compiled = if candidate.report.is_valid() {
            compile(&candidate.plan, &candidate.report, &self.registry, &self.frame)
        } else if matches!(candidate.origin, Origin::DefaultProgram { .. }) {
            compile_unvalidated(&candidate.plan, &self.registry, &self.frame)
        } else {
            return Err(Rejection::Invalid {
                problems: candidate.report.errors().map(ToString::to_string).collect(),
            });
        };
        let compiled = compiled.map_err(|error| match error {
            CompileError::Placement { source } => Rejection::Unplaceable(source),
            other => Rejection::Compile {
                message: other.to_string(),
            },
        })?;
        Ok(Queued {
            candidate: id,
            origin: candidate.origin,
            plan: candidate.plan,
            plan_hash,
            compiled,
            attempts: 0,
            reported: None,
        })
    }

    /// Creates a generation for a queued candidate, alive and pending.
    fn generation(&mut self, queued: &Queued) -> GenerationRef {
        let id = self.ids.next();
        let generation = Arc::new(GenerationToken::with_retire_notifier(id, (self.notifier)(id)));
        self.live.insert(id);
        self.records.insert(
            id,
            GenerationRecord {
                id,
                candidate: queued.candidate,
                origin: queued.origin,
                plan_hash: queued.plan_hash,
                state: GenerationState::Pending,
                reason: None,
            },
        );
        generation
    }

    /// Starts preparing the waiting candidate, if nothing stops it.
    fn try_start(&mut self, actions: &mut Vec<Action>) {
        let Some(queued) = self.pending.as_mut() else { return };
        let reason = if self.preparing.is_some() {
            Some(PendingReason::Preparing)
        } else if self.retry_scheduled {
            Some(if queued.attempts > 0 {
                PendingReason::Retry {
                    attempt: queued.attempts + 1,
                }
            } else {
                PendingReason::Backoff
            })
        } else if self.live.len() >= self.config.overlap_budget {
            Some(PendingReason::OverlapBudget {
                live: self.live.len(),
                budget: self.config.overlap_budget,
            })
        } else {
            None
        };
        if let Some(reason) = reason {
            if queued.reported != Some(reason) {
                queued.reported = Some(reason);
                actions.push(Self::report(queued, Outcome::Pending(reason)));
            }
            return;
        }
        let Some(queued) = self.pending.take() else { return };
        self.start(queued, actions);
    }

    fn start(&mut self, queued: Queued, actions: &mut Vec<Action>) {
        if let Some(active) = &self.active {
            let changes = diff(&active.plan, &queued.plan, &self.registry);
            if let Some(owner) = changes
                .state_owners
                .iter()
                .find(|owner| owner.class != TransitionClass::Retain)
            {
                let rejection = Rejection::Transition {
                    node: owner.node.clone(),
                    class: owner.class,
                };
                actions.push(Self::report(&queued, Outcome::Rejected(rejection)));
                return;
            }
        }

        let generation = self.generation(&queued);
        let active_programs = self.active.as_ref().map(|active| active.programs.as_slice());
        let reuse = kernel_reuse(active_programs, &queued.compiled.programs);
        let slots = reuse
            .into_iter()
            .zip(&queued.compiled.programs)
            .map(|(kernels, program)| {
                let program = match active_programs {
                    Some(active) if kernels.unchanged => Arc::clone(&active[kernels.slot.get()]),
                    _ => Arc::clone(program),
                };
                SlotPrepare {
                    slot: kernels.slot,
                    program,
                    unchanged: kernels.unchanged,
                    reuse: kernels.reuse,
                    build: kernels.build,
                }
            })
            .collect::<Vec<_>>();
        let programs = slots.iter().map(|slot| Arc::clone(&slot.program)).collect();
        let waiting = slots.iter().map(|slot| slot.slot).collect::<BTreeSet<_>>();
        actions.push(Action::Prepare(Prepare {
            generation: generation.clone(),
            slots,
            timeout: self.config.prepare_timeout,
        }));
        let id = generation.id();
        self.preparing = Some(Preparation {
            installed: Installed {
                generation,
                candidate: queued.candidate,
                origin: queued.origin,
                plan: queued.plan,
                plan_hash: queued.plan_hash,
                programs,
                rewrites: queued.compiled.rewrites.clone().into(),
            },
            compiled: queued.compiled,
            attempts: queued.attempts + 1,
            waiting,
        });
        self.report_generation(id, actions);
        if self
            .preparing
            .as_ref()
            .is_some_and(|preparing| preparing.waiting.is_empty())
        {
            self.commit(actions);
        }
    }

    /// Commits the generation every region prepared.
    fn commit(&mut self, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        self.transition(id, LifecycleEvent::BarrierPassed, None);
        self.transition(id, LifecycleEvent::Committed, None);
        actions.push(Action::Commit {
            generation: installed.generation.clone(),
        });
        self.report_generation(id, actions);

        let keep = installed.origin.can_roll_back();
        if let Some(replaced) = self.active.replace(installed) {
            let replaced_id = replaced.generation.id();
            let released = if self.state_of(replaced_id) == Some(GenerationState::Active) {
                self.transition(
                    replaced_id,
                    LifecycleEvent::Superseded {
                        kept_as_last_known_good: keep,
                    },
                    None,
                );
                self.report_generation(replaced_id, actions);
                if keep {
                    self.last_known_good.replace(replaced)
                } else {
                    self.last_known_good.take()
                }
            } else {
                // A generation that supervision failed stays failed, and is never kept to roll back to: the one
                // already kept, if any, stays kept below an artifact. The core drops the failed generation here.
                if keep {
                    None
                } else {
                    self.last_known_good.take()
                }
            };
            if let Some(released) = released {
                let released_id = released.generation.id();
                self.transition(released_id, LifecycleEvent::Released, None);
                self.report_generation(released_id, actions);
            }
        }
        self.prune();
        self.try_start(actions);
    }

    /// Cancels the preparation in flight, if any, and queues its candidate to be prepared again, unless a newer
    /// candidate already waits.
    fn restart_preparation(&mut self, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        actions.push(Action::Abort {
            generation: id,
            slots: self.frame.slots().iter().map(|slot| slot.id).collect(),
        });
        let reason = "the generation it was prepared against failed, so it is prepared again";
        self.transition(id, LifecycleEvent::Rejected, Some(reason.to_string()));
        self.report_generation(id, actions);
        if self.pending.is_some() {
            actions.push(Action::Report(Report {
                candidate: installed.candidate,
                origin: installed.origin,
                generation: Some(id),
                outcome: Outcome::Superseded,
            }));
            return;
        }
        self.pending = Some(Queued {
            candidate: installed.candidate,
            origin: installed.origin,
            plan: installed.plan,
            plan_hash: installed.plan_hash,
            compiled: preparing.compiled,
            attempts: preparing.attempts,
            reported: None,
        });
    }

    /// Returns the state of a generation the core remembers.
    fn state_of(&self, generation: GenerationId) -> Option<GenerationState> {
        self.records.get(&generation).map(|record| record.state)
    }

    /// Returns `true` if supervision failed the generation.
    fn has_failed(&self, generation: GenerationId) -> bool {
        self.state_of(generation) == Some(GenerationState::Failed)
    }

    /// Aborts the preparation in every region and decides what becomes of its candidate.
    fn fail_preparation(&mut self, message: String, state: GenerationState, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        actions.push(Action::Abort {
            generation: id,
            slots: self.frame.slots().iter().map(|slot| slot.id).collect(),
        });
        let event = if state == GenerationState::Failed {
            LifecycleEvent::Failed
        } else {
            LifecycleEvent::Rejected
        };
        self.transition(id, event, Some(message.clone()));
        self.report_generation(id, actions);

        let retried = !installed.origin.can_roll_back() && state == GenerationState::Rejected;
        let report = |outcome| {
            Action::Report(Report {
                candidate: installed.candidate,
                origin: installed.origin,
                generation: Some(id),
                outcome,
            })
        };
        if !retried {
            actions.push(report(Outcome::Rejected(Rejection::Prepare { message })));
        } else if self.pending.is_some() {
            actions.push(report(Outcome::Superseded));
        } else {
            // Native configuration is applied eventually: the candidate waits and is tried again.
            self.pending = Some(Queued {
                candidate: installed.candidate,
                origin: installed.origin,
                plan: installed.plan,
                plan_hash: installed.plan_hash,
                compiled: preparing.compiled,
                attempts: preparing.attempts,
                reported: None,
            });
        }
        if retried {
            self.retry_scheduled = true;
            actions.push(Action::RetryAfter(self.config.retry_delay(preparing.attempts)));
        }
        self.try_start(actions);
    }

    /// Cancels the preparation for a newer candidate.
    fn cancel_preparation(&mut self, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        actions.push(Action::Abort {
            generation: id,
            slots: self.frame.slots().iter().map(|slot| slot.id).collect(),
        });
        self.transition(id, LifecycleEvent::Rejected, Some(Rejection::Cancelled.to_string()));
        actions.push(Action::Report(Report {
            candidate: installed.candidate,
            origin: installed.origin,
            generation: Some(id),
            outcome: Outcome::Rejected(Rejection::Cancelled),
        }));
    }

    fn transition(&mut self, id: GenerationId, event: LifecycleEvent, reason: Option<String>) {
        let Some(record) = self.records.get_mut(&id) else {
            return;
        };
        match record.state.apply(event) {
            Ok(state) => {
                record.state = state;
                if reason.is_some() {
                    record.reason = reason;
                }
            }
            Err(error) => debug_assert!(false, "generation {}: {}", id, error),
        }
    }

    fn report(queued: &Queued, outcome: Outcome) -> Action {
        Action::Report(Report {
            candidate: queued.candidate,
            origin: queued.origin,
            generation: None,
            outcome,
        })
    }

    fn report_generation(&self, id: GenerationId, actions: &mut Vec<Action>) {
        if let Some(record) = self.records.get(&id) {
            actions.push(Action::Report(Report {
                candidate: record.candidate,
                origin: record.origin,
                generation: Some(id),
                outcome: Outcome::Generation(record.state),
            }));
        }
    }

    /// Forgets the oldest generations in a final state that are no longer alive, beyond the history kept.
    fn prune(&mut self) {
        let forgettable = self
            .records
            .values()
            .filter(|record| record.state.is_final() && !self.live.contains(&record.id))
            .map(|record| record.id)
            .collect::<Vec<_>>();
        if forgettable.len() > HISTORY {
            for id in &forgettable[..forgettable.len() - HISTORY] {
                self.records.remove(id);
            }
        }
    }
}

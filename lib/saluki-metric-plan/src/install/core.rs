//! The installer's decisions, without I/O.

mod artifacts;

mod cut;

mod declared;

mod resources;

mod retry;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

#[cfg(test)]
use saluki_core::execution::AdmissionPointId;
use saluki_core::execution::{
    AdmissionTime, GenerationId, GenerationIdAllocator, GenerationRef, GenerationToken, RetireNotifier,
};

use self::artifacts::{ArtifactState, Settle};
use self::cut::CutSetup;
pub use self::cut::{AfterCancel, ArmCut, CutDeferReason, CutInFlight, CutPhase, CutProgress, CutStatus};
#[cfg(test)]
pub(super) use self::declared::keeps;
use self::declared::DeclaredState;
use self::resources::TransactionState;
use self::retry::{Wait, Waits};
use super::resources::{
    Creator, CreditKey, HostPrepare, HostPublish, LeaseRef, NoResourceBounds, RecipientRef, ResourceBounds,
    ResourceTable, ResourcesRevision,
};
use super::{
    diff::{diff, kernel_reuse, state_bindings, CutPlan},
    lifecycle::{GenerationState, LifecycleEvent},
};
use crate::{
    canonical::{PlanHash, ResolvedValues},
    compile::{
        compile, compile_unvalidated, CompileError, CompiledPlan, Frame, KernelKey, LeaseTable, LineageKey,
        PlacementError, RegionProgram, RewriteRecord, SlotId,
    },
    extension::TransitionClass,
    ir::{HostId, NodeId, Plan},
    limits::{self, values::MAX_TRANSITION_LCM},
    operators::OperatorRegistry,
    runtime::state::rules::{lcm_secs, CutError},
    validate::ValidationReport,
};

/// The most generations whose final state the installer remembers, beyond those still alive.
const HISTORY: usize = 16;

/// Why a preparation is cancelled when the generation it was prepared against fails.
const PREPARED_AGAINST_FAILED: &str = "the generation it was prepared against failed, so it is prepared again";

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
    ///
    /// A rollback to the plan of an artifact revision has that revision's origin: rolling back never changes the
    /// revision high-water mark.
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

    /// How the slot binds each of the program's state owners to an instance of its state, and which of the active
    /// generation's instances retire at the cut (see [`state_bindings`][super::diff::state_bindings]): empty for a
    /// slot without state owners.
    pub state: Vec<StateBinding>,

    /// The generation's lease table: the lease of every output its plan declares, which the slot's exits to outputs
    /// write into each buffer's header. Every slot of a generation shares one table.
    pub leases: Arc<LeaseTable>,
}

/// How a region binds one state owner of a prepared generation to an instance of its state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateBinding {
    /// Bind the state owner to the instance of its lineage that the generation `from` uses: the state carries over.
    Retain {
        /// The state owner.
        node: NodeId,

        /// Its lineage.
        key: LineageKey,

        /// The generation whose instance it binds.
        from: GenerationId,
    },

    /// Bind the state owner to the new, empty instance built for the prepared generation.
    New {
        /// The state owner.
        node: NodeId,

        /// Its lineage.
        key: LineageKey,
    },

    /// Mark the instance of the lineage that the generation `from` uses as retiring: the prepared generation replaces
    /// it at a cut, after which the instance makes its final flush and holds nothing. Only cut installations retire
    /// instances.
    Retire {
        /// The state owner.
        node: NodeId,

        /// Its lineage.
        key: LineageKey,

        /// The generation whose instance retires.
        from: GenerationId,
    },
}

/// A preparation: what every region must prepare for one generation, and how long they have.
#[derive(Clone, Debug)]
pub struct Prepare {
    /// The generation. Regions keep it behind a weak reference, so that the generation retires once nothing else
    /// holds it, and release its programs then. A region that retires an instance at a cut (a
    /// [`StateBinding::Retire`]) holds it, and the generation the instance served, strongly until the instance's final
    /// flush.
    pub generation: GenerationRef,

    /// What each slot prepares, in slot order: every slot of the frame, unchanged ones included.
    pub slots: Vec<SlotPrepare>,

    /// What each host prepares for the generation: the leases and recipients it creates, reopens, or requires. Each
    /// host acknowledges its part with [`PrepareSite::Host`], and the barrier waits for every host listed as for every
    /// region.
    pub hosts: Vec<HostPrepare>,

    /// The credits the preparation adds to the resource budget, per resource, in bytes: its new leases and recipients,
    /// an idle host's queue, and its new kernel instances in each slot. The caller acquires every one it does not hold
    /// yet before it sends anything, and reports a refusal as the failed acknowledgment of
    /// [`PrepareSite::Budget`].
    pub admitted: Vec<(CreditKey, u64)>,

    /// How long the regions and hosts have to acknowledge, after which the caller reports
    /// [`InstallerCore::prepare_timed_out`].
    pub timeout: Duration,
}

/// Where a part of a preparation was prepared, which acknowledges it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PrepareSite {
    /// A slot's region, with the slot's program.
    Slot(SlotId),

    /// A delivery host and its encode host, with the leases and recipients the preparation lists for them.
    Host(HostId),

    /// The caller's resource budget, from which the caller acquires the credits the preparation adds before it sends
    /// anything: a refusal is this site's failed acknowledgment, naming the credit refused, so the core attributes it
    /// to its resource.
    Budget {
        /// The credit the budget refused.
        resource: CreditKey,
    },
}

impl From<SlotId> for PrepareSite {
    fn from(slot: SlotId) -> Self {
        Self::Slot(slot)
    }
}

/// What a site answered for its part of a preparation, or a host for its part of a resource transaction.
///
/// `Result<(), String>` converts into it: `Ok` is [`Prepared`][Self::Prepared], and `Err` [`Failed`][Self::Failed].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareOutcome {
    /// The site holds its part: a region the generation's program; a host every lease and recipient its part lists,
    /// with every recipient the part requires live; the budget every credit the preparation adds.
    Prepared,

    /// A delivery host holds its part, except recipients it was asked to reopen whose I/O loops had already exited:
    /// it answered their reopen with `Exited`, which is a race with their drains ending, not a failure. Each one's
    /// exit arrives on the host's event channel ([`InstallerCore::recipient_retired`] or
    /// [`InstallerCore::recipient_failed`]), after which the core prepares its next incarnation, whose queue adopts
    /// the directory.
    ///
    /// Only a host answers this; from a region or the budget, it is a failure.
    ReopenExited {
        /// The recipients whose loops had exited.
        recipients: Vec<RecipientRef>,
    },

    /// The site failed to prepare its part, with why: a host's error names the lease or recipient.
    Failed(String),
}

impl From<Result<(), String>> for PrepareOutcome {
    fn from(outcome: Result<(), String>) -> Self {
        match outcome {
            Ok(()) => Self::Prepared,
            Err(message) => Self::Failed(message),
        }
    }
}

/// Something the caller must do.
#[derive(Clone, Debug)]
pub enum Action {
    /// Send each slot's preparation to its region, and report each acknowledgment with
    /// [`InstallerCore::prepare_acknowledged`].
    Prepare(Prepare),

    /// Tell each listed region to drop the generation's programs, the state instances built for it, and the
    /// retirements it marked, and each listed delivery host to drop the recipients the generation created that no
    /// other preparation holds and to return the recipients it reopened to draining, with their original deadlines.
    /// Only a generation the schedule never stamped is aborted.
    ///
    /// An encode host has no abort: the leases the generation created follow as [`Action::ReleaseLeases`], which
    /// drains a lease nothing addressed at once.
    Abort {
        /// The generation.
        generation: GenerationId,

        /// The slots whose regions were sent the preparation.
        slots: Vec<SlotId>,

        /// The delivery hosts that were sent a part of the preparation.
        hosts: Vec<HostId>,
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

    /// Lower the current configuration into the default program and hand it to
    /// [`InstallerCore::on_default_program`].
    ///
    /// The core asks for it when the plan the schedule holds must change without a configuration change: an artifact
    /// generation failed with nothing kept to roll back to, the policy was revoked, or a rollback or a rejected
    /// artifact left a plan running that was composed before the latest configuration change. With a policy in effect,
    /// `on_default_program` composes the policy against the default program; otherwise it proposes the default program.
    InstallDefaultProgram,

    /// Report what happened to a candidate, in status, telemetry, and acknowledgments.
    Report(Report),

    /// Arm a cut, after every region acknowledged its preparation, and report the outcome with
    /// [`InstallerCore::cut_armed`] or [`InstallerCore::cut_deferred`].
    ///
    /// The caller locks the affected admission points within the cut lock timeout (deferring with
    /// [`CutDeferReason::Timeout`] if it cannot), defers with [`CutDeferReason::ClockAhead`] if an affected point's
    /// clock runs ahead of its wall clock by more than the margin, and otherwise computes the cut time from the
    /// points' latest stamps and wall clocks, records the cut's boundary on the generation's token, and publishes
    /// `AdmissionSchedule::Cut { old, new: generation, at }` while it holds every lock. It no longer times the
    /// preparation out.
    ///
    /// The core asks for it again after a restart ([`InstallerCore::resume_actions`]): a caller that finds the
    /// schedule already holding the cut reports it armed at the boundary the token records, and one that finds the
    /// boundary recorded but the cut not published reports [`InstallerCore::cut_arm_abandoned`], since regions may have
    /// read that boundary.
    ArmCut(ArmCut),

    /// Poll every admission point while the cut of `generation` at `at` is in flight, and report what each poll says
    /// with [`InstallerCore::cut_progress`].
    WatchCut {
        /// The generation that takes over at the cut.
        generation: GenerationId,

        /// The cut time.
        at: AdmissionTime,
    },

    /// Cancel the armed cut of `generation`: lock every admission point, those that feed no state owner first, and, if
    /// none stamped the new generation, publish `AdmissionSchedule::Steady(old)` and report
    /// [`InstallerCore::cut_cancelled`]; otherwise, or if the locks are not free in time, report
    /// [`InstallerCore::cut_cancel_refused`].
    CancelCut {
        /// The generation whose cut is cancelled.
        generation: GenerationId,

        /// The generation the schedule keeps.
        old: GenerationRef,
    },

    /// Store `AdmissionSchedule::Steady(generation)`, which finalizes its cut: every affected admission point passed
    /// the cut, so every later stamp gets the generation under either schedule. No lock is needed.
    FinalizeCut {
        /// The generation whose cut completes.
        generation: GenerationRef,
    },

    /// Prepare, in each listed host, the native recipients a resource transaction adds or reopens and the next
    /// incarnations of failed recipients, under the transaction's creator ([`Creator::Transaction`]), and report each
    /// host's acknowledgment with [`InstallerCore::resources_prepared`].
    ///
    /// A resource transaction carries only state that outlives generations: it waits for no metrics preparation and
    /// no cut, and a metrics preparation failure does not hold it back. The caller acquires the credits the
    /// transaction lists before it sends anything; a refusal of a credit of one host's part is that host's failed
    /// acknowledgment, and that host is sent nothing.
    PrepareResources {
        /// The revision the transaction publishes once every host prepared its part.
        revision: ResourcesRevision,

        /// What each host prepares.
        hosts: Vec<HostPrepare>,

        /// How long the hosts have to acknowledge, after which the caller reports
        /// [`InstallerCore::resources_timed_out`].
        timeout: Duration,

        /// The credits the transaction adds to the resource budget, per resource, in bytes.
        admitted: Vec<(CreditKey, u64)>,
    },

    /// Publish, in each listed host, its native recipient set, its settings, every recipient's settings, and the
    /// recipients it withdraws. A host ignores a publication older than the one it runs. Nothing is reported back.
    PublishResources {
        /// The revision published.
        revision: ResourcesRevision,

        /// What each host publishes.
        hosts: Vec<HostPublish>,
    },

    /// Tell each listed host to drop what a resource transaction that has not published prepared and no other
    /// preparation holds, and to return what it reopened to draining: a newer lowering replaced it, or a host did not
    /// acknowledge its part in time.
    AbortResources {
        /// The revision the transaction would have published.
        revision: ResourcesRevision,

        /// The hosts that were sent a part of the transaction.
        hosts: Vec<HostId>,
    },

    /// Drain the listed leases of an encode host, which no live or kept generation uses any more or whose generation
    /// was aborted, and report each lease's release with [`InstallerCore::lease_released`] once its last payload is in
    /// the delivery host's input, or at once if the encode host does not hold it.
    ReleaseLeases {
        /// The delivery host whose encode host runs the leases.
        host: HostId,

        /// The leases.
        leases: Vec<LeaseRef>,
    },

    /// Release the listed recipients of a delivery host, which no lease names and no user holds any more: each drains
    /// its queued work for at most the drain deadline, then retires. The host reports each exit on its event channel,
    /// which the caller hands to [`InstallerCore::recipient_retired`]; the caller does not wait for it before
    /// performing the next action.
    ReleaseRecipients {
        /// The delivery host.
        host: HostId,

        /// The recipients.
        recipients: Vec<RecipientRef>,
    },

    /// Return the listed credits to the resource budget: the creations they were acquired for ended, however they
    /// ended (retired, released, aborted, refused, or superseded). Credits the caller does not hold are ignored.
    ReleaseCredits {
        /// The credits.
        credits: Vec<CreditKey>,
    },
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

    /// The candidate waits to be prepared, or its prepared cut waits to be armed.
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

    /// A cut installation is in flight: every other candidate waits until its cut is finalized or cancelled, and is
    /// then prepared against the generation the schedule holds.
    CutInProgress,

    /// The candidate's generation is prepared, and arming its cut was put off; it is tried again after a delay.
    CutDeferred {
        /// Why arming was put off.
        reason: CutDeferReason,

        /// The consecutive times the affected admission points' locks were not free in time.
        attempts: u32,
    },

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

    /// It needs a recipient whose incarnation's fate is not known yet: its loop is exiting, and the candidate is
    /// prepared once the host reports the exit, with the recipient's next incarnation, whose queue adopts the
    /// directory; or its loop failed before the preparation that created it committed, and the candidate is prepared
    /// once that preparation commits or aborts.
    RecipientsExiting,

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

    /// A state owner cannot move to the candidate: its transition class is `reject`.
    Transition {
        /// The state owner.
        node: NodeId,

        /// Its transition class.
        class: TransitionClass,
    },

    /// A cut installation's bucket widths allow no cut: they line up less often than a cut may wait for, or one of them
    /// is not known.
    CutWidths {
        /// The bucket widths of the affected state owners, before and after.
        widths: Vec<Duration>,

        /// Why, and how to change through widths that allow a cut.
        message: String,
    },

    /// The cut of the candidate's prepared generation could not be armed: the affected admission points' locks were
    /// not free within the lock timeout, this many times in a row.
    CutNotArmed {
        /// The consecutive attempts.
        attempts: u32,
    },

    /// A region failed to prepare, or not every region acknowledged in time.
    Prepare {
        /// Why.
        message: String,
    },

    /// A newer candidate cancelled the preparation, or the cut, before it committed.
    Cancelled,

    /// An artifact from the channel was refused before it became a plan: its snapshot, config, envelope, document, or
    /// composition is wrong.
    Refused {
        /// Why, in plain text.
        message: String,
    },
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
            Self::Transition { node, class } => write!(
                f,
                "state owner `{}` cannot keep its state across the change (transition class `{}`)",
                node,
                class.as_str()
            ),
            Self::CutWidths { message, .. } => f.write_str(message),
            Self::CutNotArmed { attempts } => write!(
                f,
                "the cut could not be armed: the admission points upstream of the changed state owners stayed busy \
                 through {} attempts in a row; propose the change again when intake is lighter",
                attempts
            ),
            Self::Prepare { message } => write!(f, "preparation failed: {}", message),
            Self::Cancelled => f.write_str("a newer candidate cancelled the preparation"),
            Self::Refused { message } => f.write_str(message),
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

    /// The boot generation's lease table, which every region host starts with.
    pub leases: Arc<LeaseTable>,

    /// What each host is built with: the leases and recipients the boot generation needs, with their identifiers and
    /// slots.
    pub hosts: Vec<HostPrepare>,

    /// What each delivery host publishes from its start: its native recipient set and its settings, at
    /// [`revision`][Self::revision].
    pub publish: Vec<HostPublish>,

    /// The revision of [`publish`][Self::publish], the first published revision: each delivery host runs it from its
    /// start, and ignores any later publication older than it.
    pub revision: ResourcesRevision,

    /// The credits the boot generation's resources hold, per resource, in bytes: its leases, its recipients' own
    /// bounds, and its kernel instances. The boot verification counts them in the boot bound, so the caller adds them
    /// to the resource budget's capacity and acquires them at once; each returns when its resource ends, as every
    /// later credit does.
    pub admitted: Vec<(CreditKey, u64)>,
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

    /// The generation being prepared. A cut installation past its barrier is not being prepared: it is the cut in
    /// flight.
    pub preparing: Option<GenerationRecord>,

    /// The candidate waiting to be prepared.
    pub pending: Option<(CandidateId, Origin)>,

    /// Every generation alive, in ID order: their tokens have not been dropped yet.
    pub live: Vec<GenerationId>,

    /// The newest artifact revision seen.
    pub revision_high_water: Option<u64>,

    /// Every generation the installer remembers, in ID order: those alive, and the most recent others.
    pub generations: Vec<GenerationRecord>,

    /// The cut installation in flight, if there is one.
    pub cut: Option<CutStatus>,
}

/// A generation the installer holds: preparing, active, or kept as last-known-good.
#[derive(Clone)]
struct Installed {
    generation: GenerationRef,
    candidate: CandidateId,
    origin: Origin,
    plan: Arc<Plan>,
    plan_hash: PlanHash,
    /// The plan's validation report, for a rollback that installs the plan again.
    report: ValidationReport,
    programs: Vec<Arc<RegionProgram>>,
    /// The rewrites compilation considered, applied or skipped.
    rewrites: Arc<[RewriteRecord]>,
}

/// What a new candidate does to a preparation in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arrival {
    /// It waits for the preparation to end, replacing any candidate already waiting, so that a stream of
    /// configuration changes always commits: a default program, or an artifact in effect composed again after a
    /// configuration change.
    Queue,

    /// It cancels a preparation that has not committed, as the ordering rule says a newer proposal does: a newly
    /// accepted artifact, or an artifact proposed directly. A cut that is armed is cancelled only if no admission point
    /// stamped its generation yet; one that is irrevocable is not cancelled, and the candidate waits for it. A
    /// candidate that could not follow the active plan cancels nothing: it waits for what is in flight if it can follow
    /// that, and is rejected otherwise.
    Cancel,
}

/// A candidate ready to be prepared.
struct Queued {
    candidate: CandidateId,
    origin: Origin,
    plan: Arc<Plan>,
    plan_hash: PlanHash,
    report: ValidationReport,
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
    /// The regions and hosts that have not acknowledged the preparation yet.
    waiting: BTreeSet<PrepareSite>,
    /// The cut, for a cut installation: at the barrier, the generation is armed at a cut instead of committed.
    cut: Option<CutSetup>,
}

impl Preparation {
    /// Returns the candidate again, to be prepared anew.
    fn into_queued(self) -> Queued {
        Queued {
            candidate: self.installed.candidate,
            origin: self.installed.origin,
            plan: self.installed.plan,
            plan_hash: self.installed.plan_hash,
            report: self.installed.report,
            compiled: self.compiled,
            attempts: self.attempts,
            reported: None,
        }
    }
}

/// The plan and provenance of a generation a failed cut generation rolls back to, by installing it again.
#[derive(Clone, Debug)]
struct Rollback {
    candidate: Candidate,
    /// The candidate the plan was installed for, whose artifact the rollback serves.
    from: CandidateId,
}

impl Rollback {
    fn of(installed: &Installed) -> Self {
        Self {
            candidate: Candidate {
                plan: Arc::clone(&installed.plan),
                report: installed.report.clone(),
                origin: installed.origin,
            },
            from: installed.candidate,
        }
    }
}

type NotifierFactory = dyn Fn(GenerationId) -> RetireNotifier + Send + Sync;

/// What a test makes of a diff in place of the classifier: the cut, and the admission points it is armed over.
#[cfg(test)]
type CutOverride = dyn Fn(&Plan, &Plan) -> Option<(CutPlan, Vec<AdmissionPointId>)> + Send + Sync;

/// The installer's state and decisions, without I/O.
///
/// The core turns candidate plans into generations: it places and compiles each candidate into the frame, diffs it
/// with the active plan, and decides what every region prepares, when the schedule commits, and what waits. The
/// caller performs the [actions][Action] it returns, in order, and reports what happens: acknowledgments, timeouts,
/// retirements, failures, and the outcomes of cut actions. One task drives the core.
///
/// # Installation
///
/// A candidate is prepared in every region at once: the core sends a [`Prepare`] to every slot, with its program, the
/// kernels to reuse or build, and how its state owners bind state, and waits for every region to acknowledge. When all
/// have, it commits with one schedule store ([`Action::Commit`]). When one fails or the preparation times out, it
/// aborts in every region: an artifact is rejected, and a default-program candidate is tried again after a delay,
/// unless a newer candidate replaced it, so that native configuration is applied eventually.
///
/// Only one preparation runs at a time, and only the latest candidate waits: a newer default-program candidate, or a
/// re-composition of the artifact in effect after a configuration change, replaces a waiting one, and a newly accepted
/// artifact also cancels an uncommitted preparation, without waiting out the retry delay of a candidate that failed to
/// prepare. A candidate equal to the plan being prepared, or to the active one when nothing is, changes nothing; nor
/// does a newly accepted artifact equal to the active plan, beyond cancelling the preparation, nor a candidate that
/// waited and equals the active plan when its turn comes.
///
/// # Cut installations
///
/// A candidate whose state owners change their time contract, or that adds or removes a state owner, is a cut
/// installation ([`PlanDiff::cut`][super::PlanDiff::cut]): its affected lineages switch at a cut time `T`, not at a
/// batch boundary. At its barrier it moves out of the preparation into the cut in flight, which the core arms
/// ([`Action::ArmCut`]), watches ([`Action::WatchCut`]) until some admission point stamps its generation, which makes
/// it irrevocable and active, and finalizes ([`Action::FinalizeCut`]) once every affected point passed `T`. Until it is
/// irrevocable, a newer artifact or a revocation cancels it ([`Action::CancelCut`]), which can be refused. One cut is
/// in flight at a time, and every other candidate waits for it: those that arrive while it is irrevocable are prepared
/// after its finalization, against its generation. A cut installation keeps no last-known-good generation: the
/// generation kept, if any, is released when the cut becomes irrevocable, and the core keeps the plan of the generation
/// the cut replaced instead, which a failed cut generation rolls back to by installing it again, with a cut of its own
/// if its lineages differ. See [`cut_progress`][Self::cut_progress] and the other cut events.
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
/// to it is one schedule store; when a default-program generation commits, none is kept. A composition of the same
/// artifact document (a re-composition after a configuration change, or under a newer revision) replaces the earlier
/// composition without keeping it, and so does any generation over one that serves a revoked artifact: the generation
/// kept before stays kept. When supervision fails the active artifact generation, the core
/// rolls back to last-known-good, or installs the plan a cut replaced, or asks for the default program when there is
/// neither. A failed generation stays failed: it is never kept to roll back to, and when a newer generation replaces
/// it, the generation kept before, if any, stays kept. A failure never changes the revision high-water mark.
///
/// # Artifacts
///
/// Snapshots from the artifact channel arrive through [`on_snapshot`][Self::on_snapshot], and configuration changes
/// through [`on_default_program`][Self::on_default_program], which composes the policy in effect again. Every event
/// keeps each config's acknowledgment ([`config_states`][Self::config_states]) and the status summary
/// ([`status_summary`][Self::status_summary]) current. The resources product arrives through
/// [`on_resources`][Self::on_resources], and lowering declares resources from
/// [`accepted_resources`][Self::accepted_resources].
///
/// # Resources
///
/// Each generation's preparation also prepares, in the fixed encode and delivery hosts, the leases and recipients its
/// plan needs ([`ResourceTable`]), and the barrier waits for every host as for every region. Every lowering is followed
/// by a resource transaction that publishes what outlives generations: native sets, settings, and withdrawn sets
/// ([`resources_prepared`][Self::resources_prepared]). Leases drain after the last generation using them retires
/// ([`lease_released`][Self::lease_released]), recipients after every lease naming them is released and nothing else
/// holds them, and each recipient's exit arrives from its host ([`recipient_retired`][Self::recipient_retired],
/// [`recipient_failed`][Self::recipient_failed]). At every lowering, the generation kept to roll back to, and the plan
/// a cut replaced, are kept only while the latest lowering declares every output they declare with an equal
/// fingerprint, and every recipient with the same protocol, so a rollback never resurrects a removed destination.
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
    /// What waits for a delay, on the caller's one retry timer.
    waits: Waits,
    live: BTreeSet<GenerationId>,
    records: BTreeMap<GenerationId, GenerationRecord>,
    artifacts: ArtifactState,
    /// The cut installation past its barrier, if any.
    cut: Option<CutInFlight>,
    /// The plan a failed cut generation rolls back to.
    rollback: Option<Rollback>,
    /// The candidates proposed as rollbacks: arming their cuts is retried without limit.
    rollbacks: BTreeSet<CandidateId>,
    /// The generation whose cut was finalized last, while the schedule holds nothing later: a caller that restarted
    /// before performing the finalization performs it again.
    finalized: Option<GenerationRef>,
    /// The latest default program handed to the core: an artifact's bucket widths must line up with its, so that it
    /// can always replace the artifact at a cut.
    default_program: Option<Arc<Plan>>,
    /// The leases and recipients the hosts run for live generations.
    resources: ResourceTable,
    /// The retry and the failures of resource transactions.
    transactions: TransactionState,
    /// The resources product: its revisions, acknowledgments, and the revision lowering declares resources from.
    declared: DeclaredState,
    /// The credits the event being handled released, which its last action returns.
    released_credits: Vec<CreditKey>,
    #[cfg(test)]
    cut_override: Option<Box<CutOverride>>,
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
        Self::with_resource_bounds(frame, registry, config, Arc::new(NoResourceBounds), notifier, boot)
    }

    /// Creates the core and the boot generation, as [`new`][Self::new] does, with the bounds that size each resource's
    /// credit of the resource budget: the host's, which can read the parameters of leases and recipients.
    ///
    /// # Errors
    ///
    /// Returns the rejection if the boot candidate cannot be compiled into the frame, or if it is an artifact that is
    /// not valid.
    pub fn with_resource_bounds<N>(
        frame: Arc<Frame>, registry: OperatorRegistry, config: InstallerConfig, bounds: Arc<dyn ResourceBounds>,
        notifier: N, boot: Candidate,
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
            waits: Waits::default(),
            live: BTreeSet::new(),
            records: BTreeMap::new(),
            artifacts: ArtifactState::default(),
            cut: None,
            rollback: None,
            rollbacks: BTreeSet::new(),
            finalized: None,
            default_program: (!boot.origin.can_roll_back()).then(|| Arc::clone(&boot.plan)),
            resources: ResourceTable::with_bounds(bounds),
            transactions: TransactionState::default(),
            declared: DeclaredState::default(),
            released_credits: Vec::new(),
            #[cfg(test)]
            cut_override: None,
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
        // The boot generation builds every kernel, and its resources and the boot transaction are built with the
        // topology: nothing is prepared or published after its start.
        let reuse = kernel_reuse(None, &programs);
        let resources = core.resources.plan(&queued.plan);
        let kernels = reuse
            .iter()
            .map(|slot| {
                let held = slot.build.iter().map(|key| (key.clone(), id)).collect::<Vec<_>>();
                let bytes = self::resources::slot_instance_bytes(
                    &queued.plan,
                    &core.registry,
                    &core.frame,
                    slot.slot,
                    &slot.build,
                );
                (slot.slot, (held, bytes))
            })
            .collect();
        let (hosts, publish, admitted) = core.resources.boot(id, &queued.plan, &resources, kernels);
        let revision = core.resources.revision();
        core.active = Some(Installed {
            generation: generation.clone(),
            candidate,
            origin: queued.origin,
            plan: queued.plan,
            plan_hash: queued.plan_hash,
            report: queued.report,
            programs: programs.clone(),
            rewrites: queued.compiled.rewrites.into(),
        });
        Ok((
            core,
            Boot {
                generation,
                programs,
                leases: resources.leases,
                hosts,
                publish,
                revision,
                admitted,
            },
        ))
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

    /// Returns the plan of the generation being prepared, if one is: what an [`Action::Prepare`] prepares, for the
    /// caller to derive what each region needs beside the programs, such as the panic budget of each slot.
    pub fn preparing_plan(&self) -> Option<&Arc<Plan>> {
        self.preparing.as_ref().map(|preparing| &preparing.installed.plan)
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
            cut: self.cut.as_ref().map(CutInFlight::status),
        }
    }

    /// Proposes a candidate plan, returning its identifier and what to do.
    pub fn propose(&mut self, candidate: Candidate) -> (CandidateId, Vec<Action>) {
        let id = self.candidate_id();
        let origin = candidate.origin;
        let mut actions = Vec::new();
        if let Origin::Artifact { revision, .. } = origin {
            if let Some(high_water) = self.high_water.filter(|high_water| revision <= *high_water) {
                actions.push(Action::Report(Report {
                    candidate: id,
                    origin,
                    generation: None,
                    outcome: Outcome::Rejected(Rejection::Stale { revision, high_water }),
                }));
                return (id, self.settle(actions, Settle::Quiet));
            }
            self.high_water = Some(revision);
        }
        let arrival = if origin.can_roll_back() {
            Arrival::Cancel
        } else {
            self.lowered(Arc::clone(&candidate.plan), &mut actions);
            Arrival::Queue
        };
        self.submit(id, candidate, arrival, &mut actions);
        (id, self.settle(actions, Settle::Quiet))
    }

    /// Records the latest lowering of native configuration: later candidates are composed against it and lowered with
    /// the resources artifact lowering declares resources from now, and a resource transaction follows it, before any
    /// generation lowered with it is prepared.
    fn lowered(&mut self, plan: Arc<Plan>, actions: &mut Vec<Action>) {
        self.default_program = Some(plan);
        self.declared.lowering = self.declared.installed_revision();
        self.follow_lowering(actions);
    }

    /// Proposes a candidate whose revision, if it is an artifact's, has been checked against the high-water mark.
    ///
    /// Returns `true` if the candidate is unchanged: it equals what the schedule will hold.
    fn submit(&mut self, id: CandidateId, candidate: Candidate, arrival: Arrival, actions: &mut Vec<Action>) -> bool {
        self.declared.note_candidate(id);
        let origin = candidate.origin;
        let report = |outcome| {
            Action::Report(Report {
                candidate: id,
                origin,
                generation: None,
                outcome,
            })
        };

        // Nothing changes if the candidate is what the schedule will hold once what is in flight, if anything, ends. A
        // generation that supervision failed is not a state to keep: a candidate equal to it is installed anew. A newly
        // accepted artifact cancels a preparation, or a cut that is not armed yet, so the schedule will then hold the
        // active plan: an artifact equal to it cancels them and changes nothing more. An armed cut may refuse to be
        // cancelled, so an artifact equal to the active plan waits for its cancellation as any other does.
        let active = self
            .active
            .as_ref()
            .filter(|active| !self.has_failed(active.generation.id()))
            .map(|active| active.plan_hash);
        // A cut that failed, or that is being cancelled, may not be what the schedule ends up holding.
        let target = match (&self.preparing, &self.cut) {
            (Some(preparing), _) => Some(preparing.installed.plan_hash),
            (None, Some(cut)) if cut.failed.is_none() && !cut.cancelling => Some(cut.installed.plan_hash),
            (None, Some(_)) => None,
            (None, None) => active,
        };
        let cancels = arrival == Arrival::Cancel
            && (self.preparing.is_some() || self.cut.as_ref().is_some_and(|cut| cut.phase == CutPhase::Barrier));
        let plan_hash = candidate.plan.hash(ResolvedValues::Include).ok();
        if plan_hash.is_some() && (plan_hash == target || (cancels && plan_hash == active)) {
            if let Some(replaced) = self.pending.take() {
                actions.push(Self::report(&replaced, Outcome::Superseded));
            }
            if plan_hash != target {
                self.cancel_in_flight(actions);
            }
            actions.push(report(Outcome::Unchanged));
            return true;
        }

        let queued = match self.queue(id, candidate) {
            Ok(queued) => queued,
            Err(rejection) => {
                actions.push(report(Outcome::Rejected(rejection)));
                return false;
            }
        };
        // A candidate that cancels what is in flight is checked first against the plan it would be prepared after:
        // one that could not follow the active plan neither cancels nor replaces anything, and waits instead if it can
        // follow the plan in flight.
        let cancel = match arrival {
            Arrival::Cancel => match self.cancel_check(&queued) {
                Ok(checked) => checked == Arrival::Cancel,
                Err(rejection) => {
                    actions.push(report(Outcome::Rejected(rejection)));
                    return false;
                }
            },
            Arrival::Queue => false,
        };
        if let Some(replaced) = self.pending.replace(queued) {
            actions.push(Self::report(&replaced, Outcome::Superseded));
        }
        if cancel {
            self.cancel_in_flight(actions);
            // A newer approval takes over at once: it does not wait out the retry delay of a candidate that failed to
            // prepare, which it replaces. A retry delay that passes later starts nothing that is not waiting.
            self.waits.cancel(&Wait::Candidate);
        } else if arrival == Arrival::Queue
            && self.cut.as_ref().is_some_and(|cut| {
                cut.phase == CutPhase::Barrier && self.serve_same_artifact(id, cut.installed.candidate)
            })
        {
            // A re-composition of the artifact whose cut is not armed yet replaces it: nothing has read its cut time.
            self.abort_cut(Rejection::Cancelled, actions);
        }
        self.try_start(actions);
        false
    }

    /// Reports one site's acknowledgment of a preparation: [`Prepared`][PrepareOutcome::Prepared] if a region holds
    /// the generation's program, or a host the generation's leases and recipients; for a host, which recipients it
    /// could not reopen because their loops had exited ([`ReopenExited`][PrepareOutcome::ReopenExited]); or the error
    /// the site failed with.
    ///
    /// A [`SlotId`] converts into [`PrepareSite::Slot`], and `Result<(), String>` into a [`PrepareOutcome`].
    /// Acknowledgments of a generation that is no longer being prepared are ignored.
    pub fn prepare_acknowledged(
        &mut self, generation: GenerationId, site: impl Into<PrepareSite>, outcome: impl Into<PrepareOutcome>,
    ) -> Vec<Action> {
        let site = site.into();
        let outcome = outcome.into();
        let mut actions = Vec::new();
        let Some(preparing) = self
            .preparing
            .as_mut()
            .filter(|preparing| preparing.installed.generation.id() == generation)
        else {
            // A host that holds a part of an aborted generation, which a worker restart sent it again, aborts it again.
            if let (PrepareSite::Host(host), PrepareOutcome::Prepared | PrepareOutcome::ReopenExited { .. }) =
                (&site, &outcome)
            {
                if self.resources.is_dropped_part(Creator::Generation(generation), host) {
                    actions.push(Action::Abort {
                        generation,
                        slots: Vec::new(),
                        hosts: vec![host.clone()],
                    });
                }
            }
            return actions;
        };
        // A failure a resource caused is not the candidate's: an artifact composed over it is tried again, as a default
        // program is, and a declared resource's failure rejects its resources revision instead.
        let mut resource_failure = false;
        let failure = match (outcome, site) {
            (PrepareOutcome::Prepared, site @ (PrepareSite::Slot(_) | PrepareSite::Host(_))) => {
                if preparing.waiting.remove(&site) && preparing.waiting.is_empty() {
                    self.pass_barrier(&mut actions);
                }
                None
            }
            // The budget is acquired before anything is sent: its success is not waited for.
            (PrepareOutcome::Prepared, PrepareSite::Budget { .. }) => None,
            (PrepareOutcome::ReopenExited { recipients }, PrepareSite::Host(host)) => {
                // The generation's leases name the slots of incarnations whose loops exited: it is prepared again once
                // their exits are reported, with their next incarnations. A host that was sent no part has nothing to
                // reopen, and its answer changes nothing.
                if preparing.waiting.contains(&PrepareSite::Host(host.clone())) {
                    let mut released = super::resources::Released::default();
                    self.resources
                        .reopen_exited(Creator::Generation(generation), &recipients, &mut released);
                    self.push_released(released, &mut actions);
                    self.restart_for_exits(&host, &mut actions);
                }
                None
            }
            (PrepareOutcome::ReopenExited { recipients }, PrepareSite::Slot(slot)) => Some(format!(
                "region `{}` failed to prepare: it answered as a delivery host does, with {} recipients it could \
                 not reopen; report a region's preparation as prepared or failed",
                self.slot_name(slot),
                recipients.len()
            )),
            (PrepareOutcome::ReopenExited { recipients }, PrepareSite::Budget { resource }) => Some(format!(
                "the resource budget answered for {} as a delivery host does, with {} recipients it could not \
                 reopen; report the budget's acquisition as prepared or failed",
                resource,
                recipients.len()
            )),
            (PrepareOutcome::Failed(message), PrepareSite::Slot(slot)) => Some(format!(
                "region `{}` failed to prepare: {}",
                self.slot_name(slot),
                message
            )),
            (PrepareOutcome::Failed(message), PrepareSite::Host(host)) => {
                resource_failure = true;
                let message = format!("host `{}` failed to prepare: {}", host, message);
                self.attribute_host_failure(generation, &host, &message, &mut actions);
                Some(message)
            }
            (PrepareOutcome::Failed(message), PrepareSite::Budget { resource }) => {
                let message = format!("the resource budget refused {}: {}", resource, message);
                // A refusal of an artifact's kernel instances is that artifact's own failure.
                if !matches!(resource, CreditKey::Instances(..)) {
                    resource_failure = true;
                    self.attribute_credit_failure(&resource, &message, &mut actions);
                }
                Some(message)
            }
        };
        if let Some(message) = failure {
            self.fail_preparation_with(message, GenerationState::Rejected, resource_failure, &mut actions);
        }
        self.settle(actions, Settle::AfterFailure { rolled_back: false })
    }

    /// Reports that a preparation's timeout passed.
    ///
    /// A timeout of a generation that is no longer being prepared is ignored, and so is one of a cut installation past
    /// its barrier, which arming took over.
    pub fn prepare_timed_out(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(preparing) = self
            .preparing
            .as_ref()
            .filter(|preparing| preparing.installed.generation.id() == generation)
        else {
            return actions;
        };
        // Every site acknowledged it: it waits only for the resource transaction in flight to publish, which that
        // transaction's own timeout ends.
        if preparing.waiting.is_empty() {
            return actions;
        }
        let regions = preparing
            .waiting
            .iter()
            .filter_map(|site| match site {
                PrepareSite::Slot(slot) => Some(format!("`{}`", self.slot_name(*slot))),
                PrepareSite::Host(_) | PrepareSite::Budget { .. } => None,
            })
            .collect::<Vec<_>>();
        let hosts = preparing
            .waiting
            .iter()
            .filter_map(|site| match site {
                PrepareSite::Host(host) => Some(format!("`{}`", host)),
                PrepareSite::Slot(_) | PrepareSite::Budget { .. } => None,
            })
            .collect::<Vec<_>>();
        let missing = match (regions.is_empty(), hosts.is_empty()) {
            (false, true) => format!("the regions {}", regions.join(", ")),
            (true, false) => format!("the hosts {}", hosts.join(", ")),
            _ => format!("the regions {} and the hosts {}", regions.join(", "), hosts.join(", ")),
        };
        let message = format!(
            "{} did not acknowledge within {:?}",
            missing, self.config.prepare_timeout
        );
        self.fail_preparation(message, GenerationState::Rejected, &mut actions);
        self.settle(actions, Settle::AfterFailure { rolled_back: false })
    }

    /// Reports that the delay of the latest [`Action::RetryAfter`] passed.
    ///
    /// What waited for its own delay, and waited that long, is tried again: a cut whose arming was put off is armed
    /// again, a candidate that waited for the delay is prepared, and a resource transaction that left an addition out,
    /// or a failed recipient's next incarnation, is tried again. What still waits asks for another
    /// [`Action::RetryAfter`]: the caller keeps one timer, which each one replaces, and several things wait on it, each
    /// for its own delay.
    pub fn retry(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        let (due, next) = self.waits.passed();
        if let Some(delay) = next {
            actions.push(Action::RetryAfter(delay));
        }
        self.retry_resources(&due, &mut actions);
        if due.contains(&Wait::Cut) {
            if let Some(cut) = self
                .cut
                .as_mut()
                .filter(|cut| cut.arm_due && cut.phase == CutPhase::Barrier)
            {
                cut.arm_due = false;
                actions.extend(self.arm_action());
            }
        }
        self.try_start(&mut actions);
        self.settle(actions, Settle::Quiet)
    }

    /// Starts a wait for `delay`, setting the caller's retry timer if the wait needs it set.
    fn start_wait(&mut self, wait: Wait, delay: Duration, actions: &mut Vec<Action>) {
        if let Some(delay) = self.waits.start(wait, delay) {
            actions.push(Action::RetryAfter(delay));
        }
    }

    /// Reports that a generation's token was dropped: nothing references it any more.
    ///
    /// The leases no other live generation uses drain ([`Action::ReleaseLeases`]), the recipients nothing holds any
    /// more are released ([`Action::ReleaseRecipients`]), and the credits of its kernel instances that no live
    /// generation holds return.
    pub fn retired(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        self.live.remove(&generation);
        self.release_retired(generation, &mut actions);
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
        self.settle(actions, Settle::Quiet)
    }

    /// Reports that supervision failed a generation: it exceeded its panic budget.
    ///
    /// A failed active artifact generation rolls back to last-known-good with one schedule store, or, without one,
    /// installs the plan a cut replaced, or asks for the default program. Rolling back first cancels any preparation in
    /// flight, which was diffed against the failed generation and reuses its kernels, and prepares its candidate again
    /// against the generation that is active after the rollback. A failed default-program generation changes nothing
    /// here: its component failed, as a native component does. A preparing generation is aborted; a kept or draining
    /// one is marked failed, and a kept one is no longer kept.
    ///
    /// During a cut:
    ///
    /// - a cut generation that fails before its cut is armed is aborted; one that is armed is cancelled (failed if the
    ///   cancellation succeeds); one that is irrevocable keeps processing its input until it is finalized, and the plan
    ///   the cut replaced is installed again then;
    /// - a replaced generation that fails before the cut is armed aborts the cut, rolls back as above, and prepares the
    ///   cut's candidate again against what it rolled back to; one that fails while the cut is armed cancels the cut,
    ///   and rolls back if the cancellation succeeds; one that fails once the cut is irrevocable is draining, and is
    ///   only recorded failed.
    ///
    /// A failed generation stays active until another commits, since the schedule still stamps it, but it is never
    /// kept to roll back to, and a candidate equal to it is not reported unchanged.
    ///
    /// This is also the artifact paths' failure entry point. A failed active artifact generation takes its artifact out
    /// of effect: the artifact's config is acknowledged `failed`, its re-compositions in flight are dropped rather than
    /// prepared again, and the policy in effect becomes the artifact of the generation rolled back to, if any. When the
    /// generation rolled back to predates the latest configuration change, the core asks for the default program
    /// ([`Action::InstallDefaultProgram`]), so that the plan catches up with it.
    pub fn generation_failed(&mut self, generation: GenerationId, reason: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        self.withdraw_failing(generation, reason, &mut actions);
        let before = self.active.as_ref().map(|active| active.generation.id());
        actions.extend(self.fail_generation(generation, reason));
        self.try_start(&mut actions);
        let rolled_back = self.active.as_ref().map(|active| active.generation.id()) != before;
        self.settle(actions, Settle::AfterFailure { rolled_back })
    }

    fn fail_generation(&mut self, generation: GenerationId, reason: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.fail_during_cut(generation, reason, &mut actions) {
            return actions;
        }
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
            self.roll_back_active(&mut actions);
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

    /// Rolls back from the active generation, which supervision failed, if it is an artifact's: to last-known-good
    /// with one store, or, unless a candidate in flight replaces it anyway, by installing the plan a cut replaced.
    /// Without either, the failed generation stays active, and holds its kernels, until a new one commits: unless a
    /// candidate in flight replaces it, the default program is asked for when the event settles.
    fn roll_back_active(&mut self, actions: &mut Vec<Action>) {
        let can_roll_back = self.active.as_ref().is_some_and(|active| active.origin.can_roll_back());
        if !can_roll_back {
            return;
        }
        // Only a generation every region still holds prepared can be rolled back to with one store.
        let kept = self
            .last_known_good
            .take()
            .filter(|kept| self.state_of(kept.generation.id()) == Some(GenerationState::Prepared));
        if let Some(kept) = kept {
            // The preparation in flight, if any, was diffed against the failed generation and reuses kernels that only
            // the failed generation may hold once its batches drain: prepare its candidate again, against the
            // generation rolled back to.
            self.restart_preparation(actions);
            let id = kept.generation.id();
            self.transition(id, LifecycleEvent::Committed, None);
            self.finalized = None;
            actions.push(Action::Commit {
                generation: kept.generation.clone(),
            });
            self.report_generation(id, actions);
            // The failed generation is dropped here: the core no longer holds it.
            self.active = Some(kept);
            self.prune();
            self.try_start(actions);
        } else if self.preparing.is_none() && self.pending.is_none() {
            self.install_rollback(actions);
        }
    }

    /// Installs the plan a cut replaced again, as a new installation, after the generation that replaced it failed.
    ///
    /// The rollback serves the artifact the plan was composed from, under that artifact's revision, which leaves the
    /// high-water mark as it is. A plan composed before the latest configuration change is composed again after it:
    /// the core then also asks for the default program, which composes the policy in effect again.
    fn install_rollback(&mut self, actions: &mut Vec<Action>) {
        let Some(rollback) = self.rollback.take() else { return };
        let id = self.candidate_id();
        self.adopt_source(id, rollback.from);
        self.declared.adopt(id, rollback.from);
        self.rollbacks.insert(id);
        let behind = self.lowered_after(rollback.from);
        self.submit(id, rollback.candidate, Arrival::Cancel, actions);
        if behind {
            actions.push(Action::InstallDefaultProgram);
        }
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

    /// Returns every slot of the frame, which an abort is sent to.
    fn all_slots(&self) -> Vec<SlotId> {
        self.frame.slots().iter().map(|slot| slot.id).collect()
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
            report: candidate.report,
            compiled,
            attempts: 0,
            reported: None,
        })
    }

    /// Checks a candidate as queueing it for preparation does, without queueing it: its hash, its validation report,
    /// and its placement and compilation into the frame.
    ///
    /// # Errors
    ///
    /// Returns the rejection preparing the candidate would report.
    fn check(&self, candidate: Candidate) -> Result<(), Rejection> {
        self.queue(CandidateId(0), candidate).map(|_| ())
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
        } else if self.cut.is_some() {
            Some(PendingReason::CutInProgress)
        } else if self.waits.waiting(&Wait::Candidate) {
            Some(if queued.attempts > 0 {
                PendingReason::Retry {
                    attempt: queued.attempts + 1,
                }
            } else {
                PendingReason::Backoff
            })
        } else if !self.resources.waiting_for(&queued.plan).is_empty() {
            Some(PendingReason::RecipientsExiting)
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
        // A candidate that waited may equal the active plan by now, such as an artifact equal to it that cancelled an
        // armed cut: it creates no generation, and the active generation serves what it was created for.
        let unchanged = self
            .active
            .as_ref()
            .filter(|active| active.plan_hash == queued.plan_hash && !self.has_failed(active.generation.id()))
            .map(|active| active.candidate);
        if let Some(target) = unchanged {
            actions.push(Self::report(&queued, Outcome::Unchanged));
            self.serve_unchanged(queued.candidate, queued.origin, target);
            return;
        }
        let mut cut = None;
        if let Some(active) = &self.active {
            match self.check_transition(active, &queued) {
                Ok(planned) => cut = planned,
                Err(rejection) => {
                    actions.push(Self::report(&queued, Outcome::Rejected(rejection)));
                    return;
                }
            }
        }

        let generation = self.generation(&queued);
        let active_programs = self.active.as_ref().map(|active| active.programs.clone());
        let active_id = self.active.as_ref().map(|active| active.generation.id());
        let active = active_id.zip(active_programs.as_deref());
        let active_programs = active_programs.as_deref();
        let owners = cut
            .as_ref()
            .map(|cut: &CutSetup| cut.plan.owners.iter().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let bindings = state_bindings(active, &queued.compiled.programs, &owners);
        let reuse = kernel_reuse(active_programs, &queued.compiled.programs);
        let resources = self.plan_generation(generation.id(), &queued.plan, active_id, &reuse);
        let slots = reuse
            .into_iter()
            .zip(&queued.compiled.programs)
            .zip(bindings)
            .map(|((kernels, program), state)| {
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
                    state,
                    leases: Arc::clone(&resources.leases),
                }
            })
            .collect::<Vec<_>>();
        let programs = slots.iter().map(|slot| Arc::clone(&slot.program)).collect();
        let waiting = slots
            .iter()
            .map(|slot| PrepareSite::Slot(slot.slot))
            .chain(resources.hosts.iter().map(|part| PrepareSite::Host(part.host.clone())))
            .collect::<BTreeSet<_>>();
        actions.push(Action::Prepare(Prepare {
            generation: generation.clone(),
            slots,
            hosts: resources.hosts,
            admitted: resources.admitted,
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
                report: queued.report,
                programs,
                rewrites: queued.compiled.rewrites.clone().into(),
            },
            compiled: queued.compiled,
            attempts: queued.attempts + 1,
            waiting,
            cut,
        });
        self.report_generation(id, actions);
        if self
            .preparing
            .as_ref()
            .is_some_and(|preparing| preparing.waiting.is_empty())
        {
            self.pass_barrier(actions);
        }
    }

    /// Checks that a candidate can follow the plan of `from`, returning the cut its installation needs, if it is a cut
    /// installation.
    ///
    /// No state owner's transition may be rejected, and a cut's bucket widths must line up within the cut wait. An
    /// artifact's widths must also line up with the default program's ([`check_way_back`][Self::check_way_back]).
    ///
    /// # Errors
    ///
    /// Returns the rejection preparing the candidate after `from` reports.
    fn check_transition(&self, from: &Installed, queued: &Queued) -> Result<Option<CutSetup>, Rejection> {
        let changes = diff(&from.plan, &queued.plan, &self.registry);
        if let Some(owner) = changes
            .state_owners
            .iter()
            .find(|owner| owner.class == TransitionClass::Reject)
        {
            return Err(Rejection::Transition {
                node: owner.node.clone(),
                class: owner.class,
            });
        }
        let cut = self.plan_cut(changes.cut(), from, queued)?;
        self.check_way_back(queued)?;
        Ok(cut)
    }

    /// Checks that the default program could replace an artifact candidate at a cut, as it does when the channel
    /// revokes the artifact or a failure leaves nothing else to run: the bucket widths of the cut back to the latest
    /// default program must line up within the cut wait.
    ///
    /// Only the step from the active plan to a candidate is a cut, so without this a chain of accepted artifacts could
    /// reach widths the default program's do not line up with (such as 120 s, then 60 s, then 180 s), and the default
    /// program, refused, would leave a revoked artifact in effect. A rollback reinstalls a plan checked when it was
    /// first installed, and is not checked again.
    ///
    /// # Errors
    ///
    /// Returns [`Rejection::CutWidths`] if the widths do not line up.
    fn check_way_back(&self, queued: &Queued) -> Result<(), Rejection> {
        if !queued.origin.can_roll_back() || self.rollbacks.contains(&queued.candidate) {
            return Ok(());
        }
        let Some(default) = &self.default_program else {
            return Ok(());
        };
        let Some(back) = self.cut_between(&queued.plan, default) else {
            return Ok(());
        };
        match lcm_secs(&back.widths) {
            Err(CutError::TooLong { lcm, .. }) => Err(Rejection::CutWidths {
                message: format!(
                    "The bucket widths of the artifact's state owners and of the default program's, {}, line up only \
                     every {} seconds, more than the {} seconds a cut may wait for its boundary, so the default \
                     program could not replace the artifact, as it does when the channel revokes it. Use widths that \
                     line up with the default program's within {} seconds, such as divisors of 60 seconds.",
                    back.widths
                        .iter()
                        .map(|width| format!("{} s", width.as_secs()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    lcm,
                    MAX_TRANSITION_LCM.as_secs(),
                    MAX_TRANSITION_LCM.as_secs()
                ),
                widths: back.widths,
            }),
            // A width that is not known allows no cut from the active plan either, which reports it.
            Ok(_) | Err(_) => Ok(()),
        }
    }

    /// Returns the cut an installation of `to` after `from` needs, if it is a cut installation.
    fn cut_between(&self, from: &Plan, to: &Plan) -> Option<CutPlan> {
        #[cfg(test)]
        if let Some(cut_override) = &self.cut_override {
            return cut_override(from, to).map(|(plan, _)| plan);
        }
        diff(from, to, &self.registry).cut()
    }

    /// Decides what a candidate that cancels what is in flight does, before it cancels anything.
    ///
    /// A candidate that can follow the active plan cancels what is in flight, as the ordering rule says. One that
    /// cannot neither cancels nor replaces anything: it waits for a preparation or a cut that is not being cancelled if
    /// it can follow that one's plan, and is rejected otherwise. Nothing is checked while nothing is in flight:
    /// preparing the candidate checks it then.
    ///
    /// # Errors
    ///
    /// Returns the rejection preparing the candidate after the active plan would report.
    fn cancel_check(&self, queued: &Queued) -> Result<Arrival, Rejection> {
        // What the candidate would wait for instead: a preparation, or a cut that can still be cancelled and that no
        // cancellation or failure already ends.
        let in_flight = match (&self.preparing, &self.cut) {
            (Some(preparing), _) => Some(&preparing.installed),
            (None, Some(cut)) => match cut.phase {
                // An irrevocable cut is never cancelled: the candidate waits for it, and is checked after it.
                CutPhase::Irrevocable { .. } => return Ok(Arrival::Cancel),
                CutPhase::Barrier | CutPhase::Armed { .. } => {
                    (cut.failed.is_none() && !cut.cancelling).then_some(&cut.installed)
                }
            },
            (None, None) => return Ok(Arrival::Cancel),
        };
        let Some(active) = &self.active else {
            return Ok(Arrival::Cancel);
        };
        match self.check_transition(active, queued) {
            Ok(_) => Ok(Arrival::Cancel),
            Err(rejection) => match in_flight {
                Some(installed) if self.check_transition(installed, queued).is_ok() => Ok(Arrival::Queue),
                _ => Err(rejection),
            },
        }
    }

    /// Returns the cut a candidate's installation needs, if it is a cut installation: the cut's owners and widths, and
    /// the admission points it is armed over.
    ///
    /// # Errors
    ///
    /// Returns the rejection if the widths allow no cut, or the frame cannot be cut over: a cut's transactions lock
    /// admission points of sources only, so every admission point must belong to a source.
    fn plan_cut(
        &self, plan: Option<CutPlan>, active: &Installed, queued: &Queued,
    ) -> Result<Option<CutSetup>, Rejection> {
        #[cfg(test)]
        if let Some(cut_override) = &self.cut_override {
            return cut_override(&active.plan, &queued.plan)
                .map(|(plan, affected)| CutSetup::checked(plan, affected))
                .transpose();
        }
        let Some(plan) = plan else { return Ok(None) };
        plan.check_frame(&self.frame).map_err(Rejection::Unplaceable)?;
        let affected = plan.affected_points(&self.frame, &active.programs, &queued.compiled.programs);
        CutSetup::checked(plan, affected).map(Some)
    }

    /// Ends a preparation every region acknowledged: commits it, or, for a cut installation, arms its cut.
    ///
    /// A generation that uses a recipient its host withdraws while the latest lowering declares it again, as when an
    /// endpoint removed is added back before the resource transaction that restores it published, waits for that
    /// transaction: its leases would otherwise send to a recipient its host drops every payload for. The transaction
    /// publishes once its hosts answer, or at its timeout.
    fn pass_barrier(&mut self, actions: &mut Vec<Action>) {
        if self.waits_for_publication() {
            tracing::debug!(
                "A prepared generation waits for the resource transaction that restores a recipient it uses."
            );
            return;
        }
        let is_cut = self.preparing.as_ref().is_some_and(|preparing| preparing.cut.is_some());
        if is_cut && self.active.is_some() {
            self.begin_cut(actions);
        } else {
            self.commit(actions);
        }
    }

    /// Returns `true` if the preparation every site acknowledged waits for the resource transaction in flight to
    /// publish a recipient it uses again ([`pass_barrier`][Self::pass_barrier]).
    fn waits_for_publication(&self) -> bool {
        let (Some(preparing), Some(lowered)) = (&self.preparing, &self.default_program) else {
            return false;
        };
        self.resources.transaction_in_flight().is_some()
            && self
                .resources
                .withdraws_declared(&preparing.installed.plan, &lowered.resources)
    }

    /// Passes the barrier of a preparation every site acknowledged, which waited for a publication.
    pub(super) fn resume_barrier(&mut self, actions: &mut Vec<Action>) {
        if self
            .preparing
            .as_ref()
            .is_some_and(|preparing| preparing.waiting.is_empty())
        {
            self.pass_barrier(actions);
        }
    }

    /// Commits the generation every region prepared.
    fn commit(&mut self, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        self.transition(id, LifecycleEvent::BarrierPassed, None);
        self.transition(id, LifecycleEvent::Committed, None);
        self.resources.commit(id);
        // A recipient the generation reopened may have failed meanwhile: its next incarnation follows.
        self.after_exit(actions);
        self.finalized = None;
        actions.push(Action::Commit {
            generation: installed.generation.clone(),
        });
        self.report_generation(id, actions);

        let keep = installed.origin.can_roll_back();
        let candidate = installed.candidate;
        if let Some(replaced) = self.active.replace(installed) {
            let replaced_id = replaced.generation.id();
            let released = if self.state_of(replaced_id) == Some(GenerationState::Active) {
                // A composition of the same artifact document (a re-composition after a configuration change, or under
                // a newer revision) replaces its earlier composition, which is not a plan to roll back to, and nothing
                // a revoked artifact installed is either: the generation kept before stays kept.
                let kept = keep
                    && !self.serve_same_artifact(candidate, replaced.candidate)
                    && !self.serves_revoked(replaced.candidate);
                self.transition(
                    replaced_id,
                    LifecycleEvent::Superseded {
                        kept_as_last_known_good: kept,
                    },
                    None,
                );
                self.report_generation(replaced_id, actions);
                if kept {
                    // The generation kept is newer than any plan a cut replaced before it.
                    self.rollback = None;
                    self.last_known_good.replace(replaced)
                } else if keep {
                    None
                } else {
                    self.rollback = None;
                    self.last_known_good.take()
                }
            } else {
                // A generation that supervision failed stays failed, and is never kept to roll back to: the one
                // already kept, if any, stays kept below an artifact. The core drops the failed generation here.
                if keep {
                    None
                } else {
                    self.rollback = None;
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
        let id = preparing.installed.generation.id();
        self.push_abort(id, actions);
        self.transition(id, LifecycleEvent::Rejected, Some(PREPARED_AGAINST_FAILED.to_string()));
        self.report_generation(id, actions);
        self.requeue(preparing.into_queued(), Some(id), actions);
    }

    /// Cancels the preparation in flight because `host` answered that recipients it was asked to reopen had exited,
    /// and queues its candidate to be prepared again, unless a newer candidate already waits: it waits for the exits,
    /// and is then prepared with the recipients' next incarnations. It is not a failure, so nothing is retried.
    fn restart_for_exits(&mut self, host: &HostId, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let id = preparing.installed.generation.id();
        self.push_abort(id, actions);
        let reason = format!(
            "host `{}` could not reopen recipients whose loops had exited, so it is prepared again once their exits \
             are reported",
            host
        );
        self.transition(id, LifecycleEvent::Rejected, Some(reason));
        self.report_generation(id, actions);
        self.requeue(preparing.into_queued(), Some(id), actions);
        self.try_start(actions);
    }

    /// Queues a candidate whose generation was aborted to be prepared again, unless a newer candidate already waits.
    fn requeue(&mut self, queued: Queued, generation: Option<GenerationId>, actions: &mut Vec<Action>) {
        if self.pending.is_some() {
            actions.push(Action::Report(Report {
                candidate: queued.candidate,
                origin: queued.origin,
                generation,
                outcome: Outcome::Superseded,
            }));
            return;
        }
        self.pending = Some(queued);
    }

    /// Returns the state of a generation the core remembers.
    fn state_of(&self, generation: GenerationId) -> Option<GenerationState> {
        self.records.get(&generation).map(|record| record.state)
    }

    /// Returns `true` if supervision failed the generation.
    fn has_failed(&self, generation: GenerationId) -> bool {
        self.state_of(generation) == Some(GenerationState::Failed)
    }

    /// Aborts the preparation in every region and host, and decides what becomes of its candidate.
    fn fail_preparation(&mut self, message: String, state: GenerationState, actions: &mut Vec<Action>) {
        self.fail_preparation_with(message, state, false, actions);
    }

    /// Aborts the preparation in every region and host, and decides what becomes of its candidate: with
    /// `resource_failure`, a resource failed to prepare, which is no fault of the candidate, so an artifact is tried
    /// again as a default program is.
    fn fail_preparation_with(
        &mut self, message: String, state: GenerationState, resource_failure: bool, actions: &mut Vec<Action>,
    ) {
        let Some(preparing) = self.preparing.take() else { return };
        let id = preparing.installed.generation.id();
        self.push_abort(id, actions);
        let event = if state == GenerationState::Failed {
            LifecycleEvent::Failed
        } else {
            LifecycleEvent::Rejected
        };
        self.transition(id, event, Some(message.clone()));
        self.report_generation(id, actions);
        let attempts = preparing.attempts;
        let queued = preparing.into_queued();
        // A default program, the re-composition of an artifact that was running when the configuration changed, and
        // any candidate whose preparation failed on a resource, are tried again; a failure under supervision is not.
        let retried = (!queued.origin.can_roll_back() || self.is_retried(queued.candidate) || resource_failure)
            && state == GenerationState::Rejected;
        self.end_attempt(queued, Some(id), message, attempts, retried, actions);
    }

    /// Decides what becomes of a candidate whose generation was aborted after it failed to prepare, or to be armed: one
    /// that is `retried` waits, and is tried again after a delay, so that configuration changes are applied
    /// eventually; any other is rejected.
    fn end_attempt(
        &mut self, queued: Queued, generation: Option<GenerationId>, message: String, attempts: u32, retried: bool,
        actions: &mut Vec<Action>,
    ) {
        let report = |outcome| {
            Action::Report(Report {
                candidate: queued.candidate,
                origin: queued.origin,
                generation,
                outcome,
            })
        };
        if !retried {
            actions.push(report(Outcome::Rejected(Rejection::Prepare { message })));
        } else if self.pending.is_some() {
            actions.push(report(Outcome::Superseded));
        } else {
            // Native configuration is applied eventually: the candidate waits and is tried again.
            self.pending = Some(queued);
        }
        if retried {
            self.start_wait(Wait::Candidate, self.config.retry_delay(attempts), actions);
        }
        self.try_start(actions);
    }

    /// Cancels what is in flight for a newer candidate: the preparation, or a cut that is not irrevocable. A cut that
    /// is not armed yet is aborted at once; one that is armed is asked to cancel, after which the newer candidate is
    /// prepared.
    fn cancel_in_flight(&mut self, actions: &mut Vec<Action>) {
        if self.preparing.is_some() {
            self.cancel_preparation(actions);
            return;
        }
        match self.cut.as_ref().map(|cut| cut.phase) {
            Some(CutPhase::Barrier) => self.abort_cut(Rejection::Cancelled, actions),
            Some(CutPhase::Armed { .. }) => self.request_cancel(Some(AfterCancel::PrepareNewer), actions),
            Some(CutPhase::Irrevocable { .. }) | None => {}
        }
    }

    /// Cancels the preparation for a newer candidate.
    fn cancel_preparation(&mut self, actions: &mut Vec<Action>) {
        let Some(preparing) = self.preparing.take() else { return };
        let installed = preparing.installed;
        let id = installed.generation.id();
        self.push_abort(id, actions);
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

    /// Forgets the oldest generations in a final state that are no longer alive, beyond the history kept, and the
    /// rollback marks of candidates the core no longer holds.
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
        let held = self.held_candidates();
        self.rollbacks.retain(|candidate| held.contains(candidate));
    }

    /// Returns every candidate the core holds: pending, preparing, cut, active, kept, rolled back to, or with a
    /// generation still alive.
    fn held_candidates(&self) -> BTreeSet<CandidateId> {
        let mut held = self
            .records
            .values()
            .filter(|record| self.live.contains(&record.id))
            .map(|record| record.candidate)
            .collect::<BTreeSet<_>>();
        held.extend(self.active.as_ref().map(|active| active.candidate));
        held.extend(self.last_known_good.as_ref().map(|kept| kept.candidate));
        held.extend(self.preparing.as_ref().map(|preparing| preparing.installed.candidate));
        held.extend(self.pending.as_ref().map(|queued| queued.candidate));
        held.extend(self.cut.as_ref().map(|cut| cut.installed.candidate));
        held.extend(self.rollback.as_ref().map(|rollback| rollback.from));
        held
    }
}

#[cfg(test)]
impl InstallerCore {
    /// Makes the core classify every candidate with `classify` instead of its plan diff: a candidate it returns a cut
    /// for is a cut installation over the admission points it returns, with the cut's widths checked as any are.
    ///
    /// No operator of this stage's registries places a state owner in an isolation slot, so the core's cut paths are
    /// exercised with stateless candidates declared to be cuts.
    pub(in crate::install) fn classify_cuts_with<F>(&mut self, classify: F)
    where
        F: Fn(&Plan, &Plan) -> Option<(CutPlan, Vec<AdmissionPointId>)> + Send + Sync + 'static,
    {
        self.cut_override = Some(Box::new(classify));
    }
}

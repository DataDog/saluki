//! The driver of a region holding a state owner: its instances, its watermark, its flush ticks, and the drains that run
//! its time-driven work.
//!
//! See [`MetricRegionDriver`][super::MetricRegionDriver]'s documentation, [State regions](super#state-regions), for the
//! rules; this module implements them.
//!
//! # State lanes
//!
//! A state owner's region may run on several lanes, each a region of its own holding a disjoint share of every lineage's
//! contexts, by partition key ([`KeyGroups`]), and fed by every upstream point of one source clock. Every lane follows
//! the stage 6 rules on its own share, with these additions, all of which change nothing with one lane and one point:
//!
//! - **Epochs.** An instance holds data of one epoch of the upstream clock. A lane's [reset window][ResetWindow]
//!   opens at the first header of a newer epoch from any point: every instance holding older-epoch data becomes
//!   reset-retiring, ticks no more, and drains at its epoch's exact watermark once every point switched, when the
//!   lineage continues in the newer epoch, through an instance the window created for it there, or through the drained
//!   instance itself. Inside the window each batch goes to its lineage's instance of the batch's own epoch, created
//!   empty when it has none. Bindings name the lineage's bound instance; its other instances stand beside it. While
//!   the window is open the lane ticks no instance, and once it closes its lineages catch up the ticks it held, so
//!   that how a lineage ticks does not depend on which of its instances continues it.
//! - **Records outlive their last lane.** A record its last lane leaves, to continue in a newer epoch or at a final
//!   retirement, is kept while another lane may still reach its epoch ([`SlotLanes::keep_while_reachable`]), with the
//!   state owner's record of the epoch, so that a lane that hears of a reset late ticks and counts the epoch on the
//!   records the others shared.
//! - **Successors keep the schedule.** A lineage's record of a newer epoch starts its ticks at the current deadline of
//!   its record of the newest older epoch, never of an older one whose ticks stopped earlier, as one core keeps its
//!   schedule across every reset of a window open over several epochs.
//! - **Shared records.** With more than one lane, every lane's instance of one logical lineage instance (lineage,
//!   creator, epoch) is a member of one [`LineageLanes`] record, which gives every lane the instants of its ticks by
//!   ordinal, each tick's flush second, the reset mark that stops old-epoch ticks in every lane at once, and one close
//!   time at shutdown; and of the state owner's own record ([`LaneShare`]), which the lane tells how it resolved each
//!   ordinal, and from which the kernel takes where its idle-counter zeros start ([`LaneTick`] on every time-driven
//!   call). A lane joins a record when it binds an instance to it, and leaves it when the instance is freed, retires,
//!   or continues in a newer epoch.
//! - **Routing.** Every event a lane admits is checked against the key groups: one another lane owns is counted as
//!   routed to the wrong lane, which only a wiring fault produces.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Duration, Instant},
};

use saluki_core::{
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, ClockReset, DrainPolicy, ExitSink, GenerationId,
        GenerationRef, LaneContext, LaneFlush, LaneShare, LaneTick, LossClass, LossCount, PortIndex, Progress,
        RegionTurn, ScheduleHandle, Turn, TurnCounters, WallClock,
    },
    topology::EventsBuffer,
};
use saluki_metric_kernels::aggregate::{
    lanes::{ContextCount, CountHeld, Restarted, TakeLaneFallbacks},
    ContextSnapshot, PassProgress, RoomToAdmit, TakeSuppressed,
};
use tracing::{debug, error, warn};

#[cfg(any(test, feature = "test-util"))]
use super::StateKernelWrap;
use super::{
    add_loss, fold_loss, keyed::SentMark, points_in, Cursor, Fault, KernelBinding, KernelControl, KernelTarget,
    LiveGeneration, MetricRegionDriver, NodeRef, Op, Run,
};
#[cfg(any(test, feature = "test-util"))]
use crate::runtime::harness::LaneTimeEvent;
use crate::{
    compile::{KernelIndex, KernelKey, KeyGroups, LineageKey, ProgramKernel, StateParamsIndex, UpstreamPoint},
    install::StateBinding,
    ir::NodeId,
    limits::values::{FLUSH_SPREAD, FLUSH_STEP_PERIOD, MAX_OPEN_EPOCHS},
    runtime::{
        kernels::{KernelSlot, PreparedKernel},
        state::{
            lanes::{LineageLanes, LogicalInstance, SlotLanes},
            rules::{
                epoch_newer, retiring_flush_time, shutdown_time, stamping_generation, BoundaryRefs, Observation,
                OutputClock, ResetWindow, RetiringRefs, TickDecision, TickGate, WatermarkTracker, WindowStep,
            },
            slab::{InstanceHandle, Slab},
            StateBudgets,
        },
        telemetry::{StateLaneTelemetry, StateTelemetry},
    },
};

/// The state of a region holding a state owner.
pub(super) struct StateSlot {
    /// The state owner.
    pub node: NodeId,
    /// The points upstream of the state owner in every live generation that runs it.
    pub upstream: Vec<UpstreamPoint>,
    /// Those of them that are derived points of state owners upstream, whose batches are admitted before the ticks
    /// their headers release.
    pub derived: BTreeSet<AdmissionPointId>,
    /// The progress of the admission points upstream of the state owner, and the watermark it makes.
    pub tracker: WatermarkTracker,
    /// Stamps the state owner's time-driven output, at its derived point.
    pub output: OutputClock,
    /// Decides each flush tick.
    pub gate: TickGate,
    /// The schedule of the topology, read to decide the generation of time-driven output.
    pub schedule: ScheduleHandle,
    /// The wall clock the tick gate, the shutdown time, and the watermark lag read.
    pub wall: Arc<dyn WallClock>,
    /// The host's state budgets, which every state owner's kernel must draw on, if the host gave the region any.
    pub budgets: Option<StateBudgets>,
    /// The instances of the slot's state owners' state.
    pub instances: Slab<Instance>,
    /// The drain in progress, if any.
    pub drain: Option<DrainCursor>,
    /// The batch whose arrival started the drain in progress, admitted once the drain completes.
    pub stashed: Option<EventsBuffer>,
    /// Whether the region started, so that ticks are scheduled.
    pub started: bool,
    /// Whether the shutdown drain was queued.
    pub shutting_down: bool,
    /// Whether the host called `finish`: once a drain pending then completes, the shutdown drains follow.
    pub finishing: bool,
    /// Whether the shutdown drains follow the drain in progress: a lane whose input ends inside a reset window closes
    /// the window first.
    pub shutdown_after_drain: bool,
    /// The clock resets the region ran since it last published.
    pub clock_resets: u64,
    /// The gated ticks whose grace ended since the region last published.
    pub grace_expired: u64,
    /// The handles the region publishes its own telemetry to.
    pub telemetry: StateTelemetry,
    /// The lane of the slot the region runs as.
    pub lane: LaneContext,
    /// What the lanes of the slot share, for a slot with more than one lane.
    pub lanes: Option<Arc<SlotLanes>>,
    /// The table that routes partition keys to the slot's lanes, for a slot with more than one lane.
    pub groups: Option<KeyGroups>,
    /// The lane's reset window: its epoch, and the older epochs it has not closed yet.
    pub window: ResetWindow,
    /// What the lane counted since the region last published.
    pub counts: LaneCounts,
    /// The handles the lane publishes its own telemetry to.
    pub lane_telemetry: StateLaneTelemetry,
    /// What the lane did with its time since a harness last took it: its ticks, reset drains, crossings, and
    /// continuations past a reset window.
    #[cfg(any(test, feature = "test-util"))]
    pub time_events: Vec<LaneTimeEvent>,
    /// Whether the region interleaves its flushes with admission ([`RegionConfig::with_interleaving`]).
    ///
    /// [`RegionConfig::with_interleaving`]: super::RegionConfig::with_interleaving
    pub interleaving: bool,
    /// What wraps every kernel of the slot's instances when it is built or rebuilt, for tests.
    #[cfg(any(test, feature = "test-util"))]
    pub wrap: Option<StateKernelWrap>,
}

/// What a state lane counts between publications.
#[derive(Debug, Default)]
pub(super) struct LaneCounts {
    /// Events admitted whose partition key another lane owns.
    pub misrouted: u64,
    /// Ticks this lane may have resolved apart from the other lanes: a grace that expired, a gated tick dropped at a
    /// reset window's opening with several upstream points, a tick skipped holding contexts while a window is open,
    /// and a flush whose idle-counter zeros could not start where the lanes left them, as its kernel reports it
    /// ([`TakeLaneFallbacks`]).
    pub divergence: u64,
    /// Ticks older than the shared record keeps, which the lane resolved on its own second.
    pub stragglers: u64,
    /// Epochs a reset window closed at once because it kept as many open as it may.
    pub overflows: u64,
    /// Final retirements at cuts.
    pub cut_crossings: u64,
    /// Reset windows closed.
    pub reset_crossings: u64,
    /// Contexts interleaved flushes visited because admission reached them first.
    pub touched: u64,
}

impl StateSlot {
    /// Creates the state of the region of state owner `node`, running as `lane` of the slot's lanes, if it has lanes,
    /// with what they share. It follows no upstream point until its region's generations give it some
    /// ([`follow`][Self::follow]).
    pub fn new(
        node: NodeId, output_point: AdmissionPointId, schedule: ScheduleHandle, wall: Arc<dyn WallClock>,
        budgets: Option<StateBudgets>, telemetry: StateTelemetry, lane: Option<(LaneContext, Arc<SlotLanes>)>,
        lane_telemetry: StateLaneTelemetry,
    ) -> Self {
        // Records are shared only between lanes: one lane keeps every rule itself.
        let (lane, lanes) = match lane {
            Some((lane, lanes)) if !lane.is_only() => (lane, Some(lanes)),
            Some((lane, _)) => (lane, None),
            None => (LaneContext::ONE, None),
        };
        Self {
            node,
            upstream: Vec::new(),
            derived: BTreeSet::new(),
            tracker: WatermarkTracker::new(&[]),
            output: OutputClock::new(output_point),
            gate: TickGate::default(),
            schedule,
            wall,
            budgets,
            instances: Slab::default(),
            drain: None,
            stashed: None,
            started: false,
            shutting_down: false,
            finishing: false,
            shutdown_after_drain: false,
            clock_resets: 0,
            grace_expired: 0,
            telemetry,
            groups: lanes.as_ref().map(|_| KeyGroups::new(lane.count)),
            lane,
            lanes,
            window: ResetWindow::default(),
            counts: LaneCounts::default(),
            lane_telemetry,
            #[cfg(any(test, feature = "test-util"))]
            time_events: Vec::new(),
            interleaving: false,
            #[cfg(any(test, feature = "test-util"))]
            wrap: None,
        }
    }

    /// Follows the upstream points of every live generation that runs the state owner, keeping what the tracker knows
    /// of the points it still follows.
    pub fn follow(&mut self, upstream: &[UpstreamPoint]) {
        let ids = |points: &[UpstreamPoint]| points.iter().map(|point| point.id).collect::<BTreeSet<_>>();
        if ids(upstream) != ids(&self.upstream) {
            self.tracker = self.tracker.with_upstream(upstream);
        }
        self.upstream = upstream.to_vec();
        self.derived = upstream
            .iter()
            .filter(|point| point.derived)
            .map(|point| point.id)
            .collect();
    }

    /// Returns `true` if the state holds no instance and runs no drain: the region can let it go.
    pub fn is_idle(&self) -> bool {
        self.instances.is_empty() && self.drain.is_none() && self.stashed.is_none()
    }

    /// Returns the drain in progress if its current step is a pass that does not hold admission: on pace, with no batch
    /// waiting for it and no panic having ended the call that ran it.
    fn interleaved_pass(&self) -> Option<&DrainCursor> {
        self.drain
            .as_ref()
            .filter(|drain| drain.pass.is_some() && drain.current.is_some() && !drain.blocking && !drain.orphaned)
    }

    /// Returns `true` if a tick is due at `now` that the drain in progress keeps from firing: one scheduled or whose
    /// grace ended, of an instance the drain holds no tick of, or the next tick of the instance whose tick is the
    /// drain's current step, one interval after the one it runs.
    fn tick_due_behind_drain(&self, now: Instant) -> bool {
        let Some(drain) = self.drain.as_ref() else {
            return false;
        };
        if self.window.is_open() {
            return false;
        }
        let current = drain.current.as_ref().and_then(|step| match step.kind {
            DrainKind::Tick { scheduled, next, .. } => Some((step.instance, next.unwrap_or(scheduled + step.interval))),
            _ => None,
        });
        self.instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed && !instance.reset_retiring)
            .any(|(handle, instance)| match current {
                Some((ticking, next)) if ticking == handle => next <= now,
                _ if self.drains(handle, |kind| matches!(kind, DrainKind::Tick { .. })) => false,
                _ => match instance.tick {
                    TickState::Scheduled { at } => at <= now,
                    TickState::Gated { until, .. } => until <= now,
                    TickState::Unstarted | TickState::Off => false,
                },
            })
    }

    /// Returns `true` if the drain in progress has started or queued an entry of instance `handle` that `is_kind`
    /// matches. A tick or boundary flush queued or pending is not released again by a later header: the instance's
    /// tick and boundary change only once its drain completes, which a pass lets batches in before.
    fn drains(&self, handle: InstanceHandle, is_kind: impl Fn(&DrainKind) -> bool) -> bool {
        self.drain.as_ref().is_some_and(|drain| {
            drain
                .current
                .as_ref()
                .is_some_and(|step| step.instance == handle && is_kind(&step.kind))
                || drain
                    .queue
                    .iter()
                    .any(|(queued, kind)| *queued == handle && is_kind(kind))
        })
    }

    /// Returns `true` if a drain step is a pass, which admission interleaves with: a tick or boundary flush, while the
    /// region interleaves, of an instance not on lanes whose kernel admits while advancing.
    fn passes(&self, step: &DrainStep) -> bool {
        self.interleaving
            && self.lanes.is_none()
            && matches!(step.kind, DrainKind::Tick { .. } | DrainKind::Boundary { .. })
            && self
                .instances
                .get(step.instance)
                .is_some_and(|instance| instance.lanes.is_none() && instance.kernel.kernel.admits_while_advancing())
    }

    /// Wraps an instance's kernel as the region's tests ask, if they do.
    #[cfg_attr(not(any(test, feature = "test-util")), allow(clippy::unused_self))]
    fn wrap_kernel(&self, _slot: &mut KernelSlot) {
        #[cfg(any(test, feature = "test-util"))]
        if let Some(wrap) = &self.wrap {
            wrap_kernel(wrap, _slot);
        }
    }

    /// Returns `true` if the drain in progress refers to the instance.
    fn drain_refers_to(&self, handle: InstanceHandle) -> bool {
        self.drain.as_ref().is_some_and(|drain| {
            drain.current.as_ref().is_some_and(|step| step.instance == handle)
                || drain.queue.iter().any(|(queued, _)| *queued == handle)
        })
    }

    /// Returns the instance live generations bind for the lineage of `handle`: itself, or the one it stands beside.
    fn bound(&self, handle: InstanceHandle) -> InstanceHandle {
        self.instances
            .get(handle)
            .and_then(|instance| instance.primary)
            .unwrap_or(handle)
    }

    /// Returns the cut retiring the lineage of `handle`, if any: an instance standing beside the bound one retires with
    /// it.
    fn retiring_of(&self, handle: InstanceHandle) -> Option<&Retiring> {
        self.instances
            .get(self.bound(handle))
            .and_then(|instance| instance.retiring.as_ref())
    }

    /// Returns the watermark of the instance's epoch.
    fn watermark_of(&self, instance: &Instance) -> Option<AdmissionTime> {
        self.tracker.watermark_in(instance.epoch)
    }

    /// Returns the instance of the lineage bound as `bound` holding data of `epoch`, if the lane has one.
    fn in_epoch(&self, bound: InstanceHandle, epoch: u32) -> Option<InstanceHandle> {
        self.instances
            .iter()
            .find(|(handle, instance)| {
                instance.epoch == epoch && !instance.removed && (*handle == bound || instance.primary == Some(bound))
            })
            .map(|(handle, _)| handle)
    }

    /// Returns `true` if the instance is no longer used: a bound instance no live generation binds and no cut retires,
    /// or an instance standing beside one that is gone or unused, or that the lane removed.
    fn unused(&self, handle: InstanceHandle) -> bool {
        let Some(instance) = self.instances.get(handle) else {
            return true;
        };
        match instance.primary {
            None => instance.users == 0 && instance.retiring.is_none(),
            Some(primary) => instance.removed || primary == handle || self.unused(primary),
        }
    }

    /// Makes the instance leave the shared records it is a member of, if any: the slot keeps them while another lane
    /// may still reach their epoch, as one that hears of a reset late does, so that it binds its instance to them.
    fn leave(&mut self, handle: InstanceHandle) {
        let lane = self.lane.index;
        let Some(instance) = self.instances.get_mut(handle) else {
            return;
        };
        if let Some(lanes) = instance.lanes.take() {
            lanes.record.share().leave(instance.epoch, lane);
            if let Some(slot_lanes) = &self.lanes {
                slot_lanes.keep_while_reachable(instance.epoch, lanes.record);
            }
        }
    }

    /// Makes the instance a member of the record of its lineage in its epoch, creating the record with its first tick
    /// at what `first` returns if no lane holds it. Returns the deadline of the ordinal it resolves next, if the record
    /// knows it.
    fn join(&mut self, handle: InstanceHandle, first: Option<Instant>) -> Option<Instant> {
        let lane = self.lane.index;
        let slot_lanes = Arc::clone(self.lanes.as_ref()?);
        let instance = self.instances.get_mut(handle)?;
        let share = lane_share(&instance.kernel);
        let logical = LogicalInstance {
            key: instance.key.clone(),
            created_by: instance.created_by,
            epoch: instance.epoch,
        };
        let record = slot_lanes.lineage(&logical, || LineageLanes::new(share, logical.epoch));
        if let Some(first) = first {
            record.first_at(|| first);
        }
        let joined = record.join();
        record.share().join(instance.epoch, lane, joined.through);
        instance.lanes = Some(InstanceLanes {
            record,
            ordinal: joined.ordinal,
        });
        joined.at
    }
}

/// Returns the record the instances of a state owner's lineage share across lanes, as the kernel was built with it, or
/// a record of no rules for a kernel that keeps none.
fn lane_share(slot: &KernelSlot) -> Arc<dyn LaneShare> {
    slot.context
        .lanes()
        .and_then(|lanes| lanes.cell.share())
        .unwrap_or_else(|| Arc::new(NoShare))
}

/// The state owner's record of a kernel that shares no rules across lanes.
struct NoShare;

impl LaneShare for NoShare {
    fn join(&self, _: u32, _: u16, _: u64) {}
    fn leave(&self, _: u32, _: u16) {}
    fn forget(&self, _: u32) {}
    fn skipped(&self, _: u32, _: u64, _: u16) {}
}

/// One instance of a state owner's state: one lineage, created by one generation, holding data of one epoch.
pub(super) struct Instance {
    /// The lineage.
    pub key: LineageKey,
    /// The generation that created it, which with the key identifies it: two instances of one lineage can coexist.
    pub created_by: GenerationId,
    /// The kernel.
    pub kernel: KernelSlot,
    /// The number of live generations whose programs bind it.
    pub users: usize,
    /// The flush interval of the latest generation that bound it, which a skipped tick moves its deadline by.
    pub interval: Duration,
    /// When it next ticks.
    pub tick: TickState,
    /// The flush time of its latest tick that fired in this epoch, which later flush times never fall below.
    pub last_flush: Option<AdmissionTime>,
    /// The generations retiring it at a cut, from its preparation until its final retirement.
    pub retiring: Option<Retiring>,
    /// The cut of a state owner downstream of it at which it makes a boundary flush, from the preparation of the cut's
    /// generation until the flush.
    pub boundary: Option<Boundary>,
    /// Whether it made its final flush at a cut: it holds nothing, binds nothing new, and never ticks.
    pub retired: bool,
    /// Whether its last binder went while a drain referred to it: it is skipped, and freed once the drain ends.
    pub removed: bool,
    /// The epoch of the upstream clock whose data it holds.
    pub epoch: u32,
    /// The instance live generations bind for its lineage, which this one stands beside, holding data of another epoch;
    /// `None` for the bound instance itself.
    pub primary: Option<InstanceHandle>,
    /// Whether it holds data of an epoch a reset window is closing: it ticks no more, and drains once the window
    /// closes.
    pub reset_retiring: bool,
    /// Whether its reset drain was its final retirement, at the cut retiring it in the epoch it drained.
    pub retires_at_close: bool,
    /// Its membership in the record its lineage's instances share across lanes, for a slot with more than one lane.
    pub lanes: Option<InstanceLanes>,
}

/// An instance's membership in the record of its logical lineage instance across lanes.
pub(super) struct InstanceLanes {
    /// The record.
    pub record: Arc<LineageLanes>,
    /// The ordinal the instance resolves next.
    pub ordinal: u64,
}

/// When an instance next ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TickState {
    /// The region has not started: the first tick is one interval after it does.
    Unstarted,

    /// A tick is due at `at`.
    Scheduled { at: Instant },

    /// The tick due at `at` waits for the watermark to reach its wall-clock second, `second`, at most until `until`.
    Gated { at: Instant, second: u64, until: Instant },

    /// The instance never ticks again: it retired.
    Off,
}

impl TickState {
    /// Returns the deadline of the tick due, if one is.
    fn at(self) -> Option<Instant> {
        match self {
            Self::Scheduled { at } | Self::Gated { at, .. } => Some(at),
            Self::Unstarted | Self::Off => None,
        }
    }
}

/// The generations of an instance retiring at a cut, both held strongly: the one it served, whose generation its
/// output keeps, and the one replacing it, whose cut says when it retires.
pub(super) struct Retiring {
    pub owner: GenerationRef,
    pub by: GenerationRef,
}

/// The generations of an instance that makes a boundary flush at the cut of a state owner downstream of it, both held
/// strongly: the one it served, whose generation the flush keeps, and the one whose cut says when it flushes.
pub(super) struct Boundary {
    pub served: GenerationRef,
    pub by: GenerationRef,
}

/// The marks a preparation, or a retirement, sets on instances of the region's state: those it retires at its cut, and
/// those that make a boundary flush there, each with the generation it served.
#[derive(Default)]
pub(super) struct StateMarks {
    retiring: Vec<(InstanceHandle, GenerationRef)>,
    boundary: Vec<(InstanceHandle, GenerationRef)>,
}

/// What a drain runs for one instance.
#[derive(Clone, Copy, Debug)]
pub(super) enum DrainKind {
    /// A flush tick scheduled at `scheduled`, flushing at `flush`: with lanes, the tick of `ordinal`, after which the
    /// instance's next deadline is `next`, the instant the lanes' record gives the next ordinal.
    Tick {
        scheduled: Instant,
        flush: AdmissionTime,
        ordinal: Option<u64>,
        next: Option<Instant>,
    },

    /// A clock reset: a flush at the watermark before the reset, `pre`, then everything the instance holds, stamped at
    /// `stamp_at`. `in_publication_epoch` is whether `pre` is in the publication epoch of the cut retiring the
    /// instance, if any. `ends_epochs` is how many output epochs end once it
    /// completes: one for the last drain of its epoch, more when epochs before it had no instance to drain.
    Reset {
        pre: Option<AdmissionTime>,
        stamp_at: AdmissionTime,
        in_publication_epoch: bool,
        ends_epochs: u32,
    },

    /// The final retirement of an instance at its cut: a flush at the cut's time, then everything it holds.
    FinalRetire,

    /// The boundary flush of a retained instance upstream of a state owner a cut affects, at the cut's time `at`: a flush
    /// that closes exactly the buckets ending by it, under the generation it served.
    Boundary { at: AdmissionTime },

    /// The final flush at shutdown, at `at`, then the open buckets discarded.
    Shutdown { at: AdmissionTime },
}

/// A drain in progress: the instances it still runs, in order, and the one it is running.
pub(super) struct DrainCursor {
    pub queue: VecDeque<(InstanceHandle, DrainKind)>,
    pub current: Option<DrainStep>,
    /// The clock-reset entries not completed yet: once the last one completes, the lane continues past the epochs they
    /// closed.
    pub resets: usize,
    /// Whether the host ended the call running the drain, after a panic: the next `advance_time`, or `finish` once
    /// input has ended, resumes it.
    pub orphaned: bool,
    /// How long the drain has taken so far, for the lane's flush telemetry: kept for a drain of flush ticks only.
    pub timing: Option<FlushTiming>,
    /// The pace of the current step, if it is a pass: a tick or boundary flush that admission interleaves with.
    pub pass: Option<Pass>,
    /// Whether the drain runs ahead of admission until it completes, every entry left included: once its pass fell
    /// behind its pace, once a batch waits for it, or once input ended.
    pub blocking: bool,
}

impl DrainCursor {
    /// Returns a drain of `queue`, of which `resets` entries are clock resets, timed as `timing` says.
    fn new(queue: VecDeque<(InstanceHandle, DrainKind)>, resets: usize, timing: Option<FlushTiming>) -> Self {
        Self {
            queue,
            current: None,
            resets,
            orphaned: false,
            timing,
            pass: None,
            blocking: false,
        }
    }

    /// Makes the drain run ahead of admission until it completes, from `now` on.
    fn block(&mut self, now: Instant) {
        self.blocking = true;
        if let Some(timing) = self.timing.as_mut() {
            timing.hold(now);
        }
    }
}

/// How far a call ran the drain in progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DrainProgress {
    /// The drain completed.
    Done,

    /// The drain's current step is a pass on pace, pending: the region lets batches in, and runs its next step when it
    /// is due or the region is idle.
    Paused,

    /// The drain yielded, holding admission: the host resumes it.
    Yielded,
}

/// The pace of a pass: a flush of `total` contexts started at `started` is due to have visited them all within
/// `spread`, [`FLUSH_SPREAD`] of its flush interval, and to run a step at least every [`FLUSH_STEP_PERIOD`], whichever
/// is faster.
///
/// The pass is due for a step once either pace reaches what it has done: the instant the contexts pace reaches the
/// contexts it visited, by its cursor or on touch, or the instant the steps pace reaches the steps it ran. It is behind
/// once it missed that instant by a whole step period, which a pass whose steps run when they are due never does.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pass {
    started: Instant,
    spread: Duration,
    /// The contexts the pass visits, as its kernel reports them ([`PassProgress`]); zero while it does not know.
    total: usize,
    /// The contexts it visited so far.
    visited: usize,
    /// The steps it ran.
    steps: u32,
}

impl Pass {
    /// Starts the pace of a pass at `now`, flushing an instance whose flush interval is `interval`.
    fn start(now: Instant, interval: Duration) -> Self {
        Self {
            started: now,
            spread: interval.mul_f64(FLUSH_SPREAD),
            total: 0,
            visited: 0,
            steps: 0,
        }
    }

    /// Returns when the pass is due for its next step: when either pace reaches what it has done.
    fn due(&self) -> Instant {
        let by_steps = self.started + FLUSH_STEP_PERIOD * self.steps;
        if self.total == 0 || self.visited >= self.total {
            return by_steps;
        }
        let by_contexts = self.started + self.spread.mul_f64(self.visited as f64 / self.total as f64);
        by_steps.min(by_contexts)
    }

    /// Returns `true` if the pass is behind its pace at `now`: it missed its due step by a whole step period.
    fn behind(&self, now: Instant) -> bool {
        now >= self.due() + FLUSH_STEP_PERIOD
    }

    /// Records what the kernel reports of the pass's progress, and returns the contexts it visited since the last
    /// report.
    fn progress(&mut self, progress: PassProgress) -> usize {
        let visited = progress.visited.saturating_sub(self.visited);
        self.total = progress.total;
        self.visited = progress.visited;
        visited
    }
}

/// How long a drain of flush ticks has taken so far, and how long it held admission: the time the region took no input
/// because of it.
///
/// A drain holds admission from its start, and for as long as its current step is no pass, or a pass that runs ahead
/// of admission; a pass on pace lets batches in, and holds nothing.
#[derive(Clone, Copy, Debug)]
pub(super) struct FlushTiming {
    started: Instant,
    held_since: Option<Instant>,
    held: Duration,
}

impl FlushTiming {
    /// Starts timing a flush at `now`, holding admission.
    pub(super) fn start(now: Instant) -> Self {
        Self {
            started: now,
            held_since: Some(now),
            held: Duration::ZERO,
        }
    }

    /// Records that the flush holds admission from `now`, if it did not already.
    pub(super) fn hold(&mut self, now: Instant) {
        self.held_since.get_or_insert(now);
    }

    /// Records that the flush lets batches in from `now`.
    pub(super) fn release(&mut self, now: Instant) {
        if let Some(since) = self.held_since.take() {
            self.held += now.saturating_duration_since(since);
        }
    }

    /// Returns how long the flush took, from its start to `now`, and how long it held admission.
    pub(super) fn finish(&mut self, now: Instant) -> (Duration, Duration) {
        self.release(now);
        (now.saturating_duration_since(self.started), self.held)
    }
}

/// The drain of one instance, decided when it starts and kept across yields: one drain, one generation.
pub(super) struct DrainStep {
    pub instance: InstanceHandle,
    pub kind: DrainKind,
    /// The generation the output is stamped with, held for the whole drain.
    pub generation: GenerationRef,
    /// The instance's kernel index in that generation's program.
    pub kernel: usize,
    /// The position of the kernel's time-driven output entry in that generation's program.
    pub output: usize,
    /// That generation's state parameters for the kernel.
    pub params: Option<Arc<dyn Any + Send + Sync>>,
    /// The flush interval under that generation.
    pub interval: Duration,
    /// The flush time: the kernel's turn watermark.
    pub flush: Option<AdmissionTime>,
    /// The header the output is stamped with.
    pub stamp: BatchHeader,
    /// The slot's output clock once the stamp counts: the region's takes it when the step emits something, so that a
    /// step that emits nothing, such as the final retirement of an instance a reset drain already emptied, moves no
    /// later stamp.
    pub latch: OutputClock,
    /// Which shared flush of its lineage the step is, for an instance on several lanes.
    pub lane_tick: Option<LaneTick>,
    /// Whether the step wrote to each exit, over every call that ran it and every batch that flushed contexts of its
    /// pass on touch, for its progress: apart from what a batch writes ([`Scratch::written`]).
    ///
    /// [`Scratch::written`]: super::Scratch
    pub written: Vec<bool>,
}

/// How a preparation binds one state owner of its program.
pub(super) enum StateResolution {
    /// To an instance the region holds.
    Retain(InstanceHandle),

    /// To a new instance built for the preparation.
    New(Box<PreparedKernel>, usize),
}

/// What the state steps of one batch admit into: the instances of the slot's state, as one state lane admits.
///
/// The region's program runs a batch's state step against the instance its generation binds, or, inside a reset
/// window, against the bound lineage's instance of the batch's own epoch, which it creates empty when the lane has
/// none. With more than one lane, it checks that every event it admits is one its lane owns.
pub(super) struct StateAdmission<'a> {
    state: &'a mut StateSlot,
    now: Instant,
    /// Whether the last instance asked for was one of another epoch that could not be built.
    unbuilt: bool,
}

impl<'a> StateAdmission<'a> {
    /// Admits into the slot's instances, at `now` on the host's clock.
    pub fn new(state: &'a mut StateSlot, now: Instant) -> Self {
        Self {
            state,
            now,
            unbuilt: false,
        }
    }

    /// Returns the instance a state step of the batch with header `header` admits into, given the instance its
    /// generation binds: that instance when it holds data of the header's epoch, and otherwise the lineage's instance
    /// of that epoch, created empty if the lane has none.
    ///
    /// Returns `None` if the generation binds no instance, or if the lineage's instance of the header's epoch could not
    /// be built ([`unbuilt`][Self::unbuilt]): the step's batches are then invalid. Otherwise returns, with the
    /// instance, whether a pass of it is pending: its kernel's `advance_time` is yielded, and what the kernel writes
    /// while it admits is output of that call.
    pub fn instance(
        &mut self, bound: InstanceHandle, header: &BatchHeader,
    ) -> Option<(InstanceHandle, &mut Instance, bool)> {
        let epoch = self.state.tracker.epoch_of(header);
        self.unbuilt = false;
        let handle = match self.state.instances.get(bound) {
            Some(instance) if instance.epoch == epoch => bound,
            Some(_) => match self.state.in_epoch(bound, epoch) {
                Some(handle) => handle,
                None => match fresh_instance(self.state, bound, epoch, self.now) {
                    Some(handle) => handle,
                    None => {
                        self.unbuilt = true;
                        return None;
                    }
                },
            },
            None => return None,
        };
        let advancing = self
            .state
            .interleaved_pass()
            .and_then(|drain| drain.current.as_ref())
            .is_some_and(|step| step.instance == handle);
        self.state
            .instances
            .get_mut(handle)
            .map(|instance| (handle, instance, advancing))
    }

    /// Returns `true` if the last [`instance`][Self::instance] asked for was a lineage's instance of another epoch that
    /// could not be built: its batches are lost as invalid, which no fault of the program's translation causes.
    pub fn unbuilt(&self) -> bool {
        self.unbuilt
    }

    /// Checks that every event of a buffer about to be admitted is one this lane owns, counting those another lane
    /// owns as routed to the wrong lane: with one lane, every event is its own.
    pub fn check_routing(&mut self, buffer: &EventsBuffer) {
        let Some(groups) = &self.state.groups else {
            return;
        };
        let lane = self.state.lane.index;
        let misrouted = buffer
            .into_iter()
            .filter_map(|event| event.try_as_metric())
            .filter(|metric| groups.lane_of(metric.context().key().as_u64()) != lane)
            .count() as u64;
        debug_assert_eq!(misrouted, 0, "a state lane admitted events another lane owns");
        self.state.counts.misrouted += misrouted;
    }
}

/// Creates the lineage's instance of `epoch` beside the bound instance `bound`, empty, for the first batch of that
/// epoch inside a reset window: built from the lineage's kernel spec in its build context, so against the lineage's
/// lane cell, and joined to the record of the lineage in that epoch. Returns `None` if its kernel cannot be built.
fn fresh_instance(state: &mut StateSlot, bound: InstanceHandle, epoch: u32, now: Instant) -> Option<InstanceHandle> {
    let primary = state.instances.get(bound)?;
    let mut kernel = match primary.kernel.build_fresh() {
        Ok(kernel) => kernel,
        Err(reason) => {
            warn!(
                node = %primary.key.node,
                epoch,
                reason,
                "Could not build an instance of a state owner's state for another clock epoch; its batches of that \
                 epoch are lost as invalid."
            );
            return None;
        }
    };
    state.wrap_kernel(&mut kernel);
    let primary = state.instances.get(bound)?;
    // An older epoch than the lane's newest is one the window is closing.
    let reset_retiring = epoch != state.window.target();
    let predecessor = predecessor_deadline(state, bound, epoch);
    let tick = if !state.started {
        TickState::Unstarted
    } else {
        TickState::Scheduled {
            at: predecessor.unwrap_or(now + primary.interval),
        }
    };
    let instance = Instance {
        key: primary.key.clone(),
        created_by: primary.created_by,
        kernel,
        users: 0,
        interval: primary.interval,
        tick,
        last_flush: None,
        retiring: None,
        boundary: None,
        retired: false,
        removed: false,
        epoch,
        primary: Some(bound),
        reset_retiring,
        retires_at_close: false,
        lanes: None,
    };
    let handle = state.instances.insert(instance);
    let first = predecessor.unwrap_or(now + state.instances.get(handle)?.interval);
    if let Some(at) = state.join(handle, Some(first)) {
        if let Some(instance) = state.instances.get_mut(handle) {
            if state.started {
                instance.tick = TickState::Scheduled { at };
            }
        }
    }
    debug!(
        epoch,
        "Created an instance of a lineage for a newer clock epoch inside a reset window."
    );
    Some(handle)
}

/// Returns where the ticks of the lineage bound as `bound` continue in its instance of `epoch`, for a lane that binds
/// one there and may create its record: the current deadline of the lineage's record of the newest epoch older than
/// `epoch`, as a reset's successor takes its predecessor's (5.5), and one core keeps its schedule across every reset.
///
/// Inside a window open over several epochs, that is the record of the newest older epoch, never of the bound
/// instance's oldest one, whose ticks stopped earlier: another lane may have ticked the epochs between, and their
/// record outlives them while a lane may still reach them. A record of an epoch older than the lane's newest that no
/// lane marked yet is marked now, since the lane passed the epoch, so that its current deadline stops moving. Without
/// lanes, or without any such record, it is the deadline of the lane's own newest older instance of the lineage, or of
/// the bound one.
fn predecessor_deadline(state: &StateSlot, bound: InstanceHandle, epoch: u32) -> Option<Instant> {
    let lineage = state.instances.get(bound)?;
    let newest_older = state
        .instances
        .iter()
        .filter(|(handle, instance)| {
            (*handle == bound || instance.primary == Some(bound))
                && !instance.removed
                && epoch_newer(epoch, instance.epoch)
        })
        .map(|(_, instance)| instance)
        .reduce(|newest, instance| {
            if epoch_newer(instance.epoch, newest.epoch) {
                instance
            } else {
                newest
            }
        });
    if let Some(slot_lanes) = &state.lanes {
        let target = state.window.target();
        let oldest = newest_older.map_or(lineage.epoch, |instance| instance.epoch);
        let mut older = epoch.wrapping_sub(1);
        // The epochs between the lane's newest older instance and `epoch`, newest first: at most the few a window keeps
        // open.
        for _ in 0..=MAX_OPEN_EPOCHS {
            let logical = LogicalInstance {
                key: lineage.key.clone(),
                created_by: lineage.created_by,
                epoch: older,
            };
            if let Some(record) = slot_lanes.existing(&logical) {
                if epoch_newer(target, older) {
                    record.mark_reset(target);
                }
                if let Some(at) = record.current_deadline() {
                    return Some(at);
                }
            }
            if older == oldest || !epoch_newer(older, oldest) {
                break;
            }
            older = older.wrapping_sub(1);
        }
    }
    let instance = newest_older.unwrap_or(lineage);
    instance
        .lanes
        .as_ref()
        .and_then(|lanes| lanes.record.current_deadline())
        .or(instance.tick.at())
}

/// Returns the generation a drain of `handle` is stamped with: rule 3.2 of the time rules.
fn stamping(generations: &[LiveGeneration], state: &StateSlot, handle: InstanceHandle) -> Option<GenerationRef> {
    state.instances.get(handle)?;
    let retiring = state.retiring_of(handle).map(|retiring| RetiringRefs {
        owner: &retiring.owner,
        by: &retiring.by,
    });
    let schedule = state.schedule.load();
    let bound = state.bound(handle);
    // A lineage that owes a boundary flush drains under the generation it served until it made it.
    let boundary = state
        .instances
        .get(bound)
        .and_then(|instance| instance.boundary.as_ref())
        .and_then(|boundary| {
            Some(BoundaryRefs {
                served: &boundary.served,
                at: boundary.by.cut()?.at,
            })
        });
    stamping_generation(
        &schedule,
        |id| binds(generations, id, bound),
        |id| state.tracker.switched(id),
        retiring,
        boundary,
    )
}

/// Returns `true` if the live generation `id` binds the instance.
fn binds(generations: &[LiveGeneration], id: GenerationId, handle: InstanceHandle) -> bool {
    generations
        .iter()
        .find(|live| live.id == id)
        .is_some_and(|live| live.kernels.contains(&KernelBinding::Instance(handle)))
}

/// Returns the flush interval of the program a drain of `handle` would run under, or the instance's own.
fn stamping_interval(generations: &[LiveGeneration], state: &StateSlot, handle: InstanceHandle) -> Duration {
    let instance_interval = state
        .instances
        .get(handle)
        .map_or(Duration::ZERO, |instance| instance.interval);
    let bound = state.bound(handle);
    stamping(generations, state, handle)
        .and_then(|generation| generations.iter().find(|live| live.id == generation.id()))
        .and_then(|live| {
            let kernel = live
                .kernels
                .iter()
                .position(|binding| *binding == KernelBinding::Instance(bound))?;
            live.program
                .kernel(KernelIndex::new(kernel as u16))
                .and_then(|kernel| kernel.lineage.as_ref())
                .map(|lineage| lineage.flush_interval)
        })
        .unwrap_or(instance_interval)
}

/// Orders the drains one event runs together as the reference interpreter runs them: by the generation that stamps
/// each, the oldest first, then in plan order of their state owners in that generation's program (its kernels are in
/// plan order). The output clock latches each stamp to the latest before it, so the order decides the stamps of drains
/// that flush at different times.
fn in_drain_order(
    generations: &[LiveGeneration], state: &StateSlot, drains: Vec<(InstanceHandle, DrainKind)>,
) -> impl Iterator<Item = (InstanceHandle, DrainKind)> {
    let mut keyed = drains
        .into_iter()
        .map(|(handle, kind)| {
            let generation = stamping(generations, state, handle).map(|generation| generation.id());
            let bound = state.bound(handle);
            let position = generation
                .and_then(|id| generations.iter().find(|live| live.id == id))
                .and_then(|live| {
                    live.kernels
                        .iter()
                        .position(|binding| *binding == KernelBinding::Instance(bound))
                });
            ((generation, position), (handle, kind))
        })
        .collect::<Vec<_>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, drain)| drain)
}

/// Returns the gated ticks whose second the watermark has now reached, resolved as fires, in drain order: what the
/// latest header released. A gated tick the drain in progress already holds stays gated until its drain completes, and
/// is not released again. Allocates nothing when it releases none.
fn released_ticks(generations: &[LiveGeneration], state: &mut StateSlot) -> Vec<(InstanceHandle, DrainKind)> {
    let due = state
        .instances
        .iter()
        .filter(|(_, instance)| !instance.reset_retiring && !instance.retired && !instance.removed)
        .filter(|(handle, _)| !state.drains(*handle, |kind| matches!(kind, DrainKind::Tick { .. })))
        .filter_map(|(handle, instance)| {
            let TickState::Gated { at, second, .. } = instance.tick else {
                return None;
            };
            let flush = state
                .gate
                .on_watermark(second, state.watermark_of(instance), instance.last_flush)?;
            Some((handle, at, flush))
        })
        .collect::<Vec<_>>();
    let mut released = Vec::new();
    for (handle, at, flush) in due {
        let interval = stamping_interval(generations, state, handle);
        if let Some(kind) = resolve_fire(state, handle, at, flush, interval, true) {
            released.push((handle, kind));
        }
    }
    in_drain_order(generations, state, released).collect()
}

/// Returns `ticks`, drains in drain order, with the boundary flush of every instance whose cut the watermark has
/// reached among them, unless the drain in progress already holds it: after the ticks that flush by its time, and
/// before the others, so that no flush after the cut's time runs before it.
fn with_boundaries(
    generations: &[LiveGeneration], state: &StateSlot, ticks: Vec<(InstanceHandle, DrainKind)>,
) -> Vec<(InstanceHandle, DrainKind)> {
    let boundaries = state
        .instances
        .iter()
        .filter(|(_, instance)| !instance.retired && !instance.removed && !instance.reset_retiring)
        .filter(|(handle, _)| !state.drains(*handle, |kind| matches!(kind, DrainKind::Boundary { .. })))
        .filter_map(|(handle, instance)| {
            let cut = instance.boundary.as_ref()?.by.cut()?;
            let watermark = state.watermark_of(instance)?;
            (watermark >= cut.at).then_some((handle, DrainKind::Boundary { at: cut.at }))
        })
        .collect::<Vec<_>>();
    let Some(at) = boundaries
        .iter()
        .filter_map(|(_, kind)| match kind {
            DrainKind::Boundary { at } => Some(*at),
            _ => None,
        })
        .min()
    else {
        return ticks;
    };
    let (before, after): (Vec<_>, Vec<_>) = ticks
        .into_iter()
        .partition(|(_, kind)| matches!(kind, DrainKind::Tick { flush, .. } if *flush <= at));
    before
        .into_iter()
        .chain(in_drain_order(generations, state, boundaries))
        .chain(after)
        .collect()
}

impl MetricRegionDriver {
    /// Returns the region's state.
    fn state(&self) -> &StateSlot {
        self.state
            .as_deref()
            .expect("only a state owner's region runs its state")
    }

    /// Returns the region's state, mutably.
    fn state_mut(&mut self) -> &mut StateSlot {
        self.state
            .as_deref_mut()
            .expect("only a state owner's region runs its state")
    }

    /// Schedules every instance's first tick one interval after the region started, or, with lanes, at the instant of
    /// the ordinal its lineage's record says it resolves next.
    pub(super) fn start_isolated(&mut self, turn: &mut RegionTurn<'_>) {
        let now = turn.now();
        let state = self.state_mut();
        state.started = true;
        for (_, instance) in state.instances.iter_mut() {
            if instance.tick != TickState::Unstarted {
                continue;
            }
            let first = now + instance.interval;
            let at = match &instance.lanes {
                Some(lanes) if lanes.ordinal <= 1 => lanes.record.first_at(|| first),
                Some(lanes) => lanes.record.at_of(lanes.ordinal).unwrap_or(first),
                None => first,
            };
            instance.tick = TickState::Scheduled { at };
        }
    }

    /// Processes a batch that reached the region of its state owner: observes its header, runs whatever it starts before
    /// it is admitted (a reset window's opening or close, final retirements, boundary flushes, gated ticks now due),
    /// then admits it. A batch from a derived point is admitted before the gated ticks its header releases.
    pub(super) fn process_isolated(
        &mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink,
    ) -> Progress {
        let header = batch.take_header();
        // A batch from a point not upstream of the state owner whose entry reaches no state step only passes through
        // the region's stateless steps, as the output of a state owner of another region does on its way to an exit:
        // the state owner's watermark does not follow its point.
        let upstream = header
            .admission_point
            .is_some_and(|point| self.state().tracker.upstream().any(|upstream| upstream == point));
        if !batch.is_empty() && !upstream && !self.entry_runs_state(&header) {
            return self.pass_through(turn, header, batch, exits);
        }
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        if state.tracker.observe_with(&header, batch.is_empty()) == Observation::Fault {
            // A batch whose generation runs no state owner in the region only passes through its stateless steps, as
            // during a cut that moves the state owner, and a header-only one only carries the progress of its point,
            // as one forwarded while the generations that list it are being released: neither needs its point upstream
            // of the state owner.
            let stateless = header
                .generation_id()
                .and_then(|id| generations.iter().find(|live| live.id == id))
                .is_some_and(|live| !live.program.kernels().iter().any(|kernel| kernel.lineage.is_some()));
            if stateless || batch.is_empty() {
                return self.pass_through(turn, header, batch, exits);
            }
            self.reject(Fault::NotUpstream, &header, &batch);
            return Progress::Done;
        }

        let previous_target = state.window.target();
        let closed = match state.window.step(&state.tracker, &header) {
            WindowStep::Closed { epochs } => epochs,
            WindowStep::None | WindowStep::Opened { .. } | WindowStep::Route(_) => Vec::new(),
        };
        let target = state.window.target();
        if epoch_newer(target, previous_target) {
            // A reset into each epoch the lane passed: counted once per slot and epoch, by the first lane to reach it.
            let counted = match &state.lanes {
                Some(lanes) => {
                    lanes.open(state.lane.index, target);
                    lanes.count_reset(target)
                }
                None => target.wrapping_sub(previous_target),
            };
            state.clock_resets += u64::from(counted);
        }
        if !closed.is_empty() && state.window.is_open() {
            state.counts.overflows += closed.len() as u64;
        }

        // The window's opening, which stops the old epochs' ticks: what is due up to the reset mark's cutoff fires
        // first.
        let mut queue = VecDeque::new();
        let closing = closed.iter().map(|(epoch, _)| *epoch).collect::<Vec<_>>();
        open_window(generations, state, &closing, &mut queue);

        // The window's close: each closed epoch's instances drain at its watermark, oldest epoch first.
        let resets = queue_resets(generations, state, &closed, header.admitted_at, &mut queue);

        // The crossing: once every upstream point has sent a header of the generation retiring an instance, every
        // batch of the generation it served has arrived, and it makes its final flush before this batch is admitted.
        // An instance a reset window is closing retires when the window closes, at its drain.
        let crossed = state
            .instances
            .iter()
            .filter(|(handle, instance)| {
                state
                    .retiring_of(*handle)
                    .is_some_and(|retiring| state.tracker.switched(retiring.by.id()))
                    && !instance.retired
                    && !instance.removed
                    && !instance.reset_retiring
                    // Not while an instance beside it holds an epoch the window keeps open, as one a reset overflow
                    // left open after the bound instance continued into the newest epoch: that instance drains at
                    // the window's close under the generation it served, which the final retirement would clear.
                    && !state.instances.iter().any(|(_, other)| {
                        other.primary == Some(*handle)
                            && other.reset_retiring
                            && !other.removed
                            && state.window.holds(other.epoch)
                    })
            })
            .map(|(handle, _)| (handle, DrainKind::FinalRetire))
            .collect();
        queue.extend(in_drain_order(generations, state, crossed));

        // Gated ticks whose second the watermark has now reached fire before the batch, as natively they fired before
        // it arrived. A boundary flush whose cut the watermark has now reached runs among them, after the ticks that
        // flush by its time. A reset dropped every gated tick of the epochs it closes. A batch from a derived point
        // holds buckets closed before its stamp: its header raises the watermark only once it is admitted.
        let released = released_ticks(generations, state);
        let released = with_boundaries(generations, state, released);
        let ticks_released = !released.is_empty();
        queue.extend(released);

        if !batch.is_empty() && !state.window.holds(state.tracker.epoch_of(&header)) {
            // A batch of an epoch the lane already closed: one a window closed at once, past the most epochs it keeps
            // open. No instance holds that epoch any more.
            self.reject(Fault::NotUpstream, &header, &batch);
            batch = EventsBuffer::with_capacity(0);
        }
        let window_moved = epoch_newer(target, previous_target) || !closed.is_empty();
        let state = self.state_mut();
        if queue.is_empty() && state.drain.is_none() {
            if resets == 0 && !closing.is_empty() {
                self.continue_lineages();
            }
            return self.admit_isolated(turn, header, batch, exits);
        }
        // While a pass is pending, a batch enters at once unless it starts a drain of its own, moves the reset window,
        // finds an entry of the drain not started yet, or could need more room than the state owner has without
        // growing its table or refusing a context.
        if queue.is_empty() && !window_moved && self.admits_during_pass(&header, batch.len()) {
            return self.admit_isolated(turn, header, batch, exits);
        }
        // The batch waits: the drain runs ahead of admission until its last entry started, and the batch enters then
        // if that entry is a pass on pace it may enter during, or else once the drain completed. A header that moved
        // the reset window makes the drain complete first.
        let state = self.state_mut();
        batch.set_header(header);
        state.stashed = Some(batch);
        match state.drain.as_mut() {
            Some(drain) => {
                drain.queue.extend(queue);
                drain.resets += resets;
                if window_moved {
                    drain.block(Instant::now());
                } else if let Some(timing) = drain.timing.as_mut() {
                    timing.hold(Instant::now());
                }
            }
            None => {
                // A drain of the ticks the header released is timed for the lane's flush telemetry, as one of the
                // ticks a deadline fired is.
                let timing = ticks_released.then(|| FlushTiming::start(Instant::now()));
                let mut drain = DrainCursor::new(queue, resets, timing);
                if window_moved {
                    drain.block(Instant::now());
                }
                state.drain = Some(drain);
            }
        }
        if resets == 0 && !closing.is_empty() {
            self.continue_lineages();
        }
        self.continue_isolated(turn, exits)
    }

    /// Returns `true` if a batch with header `header` and `events` events may be admitted at once while the drain's
    /// current step is a pass: the pass is on pace, no entry of the drain waits to start, the pass's kernel admits
    /// while advancing, and the new contexts the batch can create fit in its table without growing it, strictly below
    /// its context limit and its budgets ([`RoomToAdmit`]), asked with the batch's events times the paths from its
    /// entry to the state step.
    fn admits_during_pass(&mut self, header: &BatchHeader, events: usize) -> bool {
        let live = header
            .generation_id()
            .and_then(|id| self.generations.iter().find(|live| live.id == id));
        let entry = live.and_then(|live| Some(&live.code.entries[live.code.entry_index(header)?]));
        let contexts = entry.map_or(0, |entry| entry.state_paths.saturating_mul(events));
        // The batch's own parameters for the state step, whose context limit the room is checked against.
        let params = live.zip(entry).and_then(|(live, entry)| {
            entry.ops.iter().find_map(|op| match op {
                Op::State { params, .. } => live
                    .program
                    .state_params()
                    .get(*params)
                    .map(|entry| Arc::clone(&entry.params)),
                _ => None,
            })
        });
        // The lineages the batch's generation binds, whose instances of any epoch it can reach.
        let reached = live
            .map(|live| {
                live.kernels
                    .iter()
                    .filter_map(|binding| match binding {
                        KernelBinding::Instance(handle) => Some(*handle),
                        KernelBinding::Slab(_) => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Self { state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        let Some(drain) = state.interleaved_pass() else {
            return false;
        };
        if drain
            .queue
            .iter()
            .any(|(queued, _)| reached.contains(&state.bound(*queued)))
        {
            return false;
        }
        let Some(handle) = drain.current.as_ref().map(|step| step.instance) else {
            return false;
        };
        let Some(instance) = state.instances.get_mut(handle) else {
            return false;
        };
        if !instance.kernel.kernel.admits_while_advancing() {
            return false;
        }
        if contexts == 0 {
            return true;
        }
        let mut room = RoomToAdmit { contexts, fits: false };
        ask_with(&mut instance.kernel, &mut room, params.as_deref());
        room.fits
    }

    /// Continues the drain in progress, then admits the batch that started it, if any.
    ///
    /// A pass on pace runs one step and lets batches in again: the call is done, with the pass pending. A batch that
    /// waited for the drain's entries to start, such as for the boundary flush its own header started, enters then
    /// if it may while the pass is pending, and otherwise the drain runs ahead of admission until it completes.
    pub(super) fn continue_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        loop {
            match self.run_drain(turn, exits) {
                DrainProgress::Yielded => return Progress::Yielded,
                DrainProgress::Done => break,
                DrainProgress::Paused => {
                    let Some(mut batch) = self.state_mut().stashed.take() else {
                        return Progress::Done;
                    };
                    let header = batch.take_header();
                    if self.admits_during_pass(&header, batch.len()) {
                        return self.admit_isolated(turn, header, batch, exits);
                    }
                    batch.set_header(header);
                    let state = self.state_mut();
                    state.stashed = Some(batch);
                    if let Some(drain) = state.drain.as_mut() {
                        drain.block(Instant::now());
                    }
                }
            }
        }
        match self.state_mut().stashed.take() {
            Some(mut batch) => {
                let header = batch.take_header();
                self.admit_isolated(turn, header, batch, exits)
            }
            // A finish that completed a drain first, over calls its host resumed, goes on with the shutdown drains
            // in the call that completed it: the host takes that call's end as the finish's.
            None if self.state().finishing && !self.state().shutting_down => self.finish_isolated(turn, exits),
            None => Progress::Done,
        }
    }

    /// Admits a batch into the state of the instances its generation's program binds, in the batch's epoch, forwards
    /// its point's progress, and, for a batch from a derived point, runs the gated ticks its header released.
    fn admit_isolated(
        &mut self, turn: &mut RegionTurn<'_>, header: BatchHeader, batch: EventsBuffer, exits: &mut ExitSink,
    ) -> Progress {
        self.scratch.written.fill(false);
        let header_only = batch.is_empty();
        self.forward_final_headers(&header, exits);
        if !header_only {
            self.run_isolated(turn, &header, batch, exits);
            if self.state().interleaved_pass().is_some() {
                self.deliver_touched(turn, exits);
            }
        }
        self.forward_progress(&header, header_only, turn.now(), exits);
        // A derived point's progress comes only in the header-only batch its state owner sends once a drain completed.
        let derived = header
            .admission_point
            .is_some_and(|point| self.state().derived.contains(&point));
        if !derived || !header_only {
            return Progress::Done;
        }
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        state.tracker.observe_after(&header);
        let released = released_ticks(generations, state);
        let queue = with_boundaries(generations, state, released);
        if queue.is_empty() {
            return Progress::Done;
        }
        // A pass pending runs first: the released ticks start once it completes, one pass at a time.
        if let Some(drain) = state.drain.as_mut() {
            drain.queue.extend(queue);
            return Progress::Done;
        }
        state.drain = Some(DrainCursor::new(
            queue.into_iter().collect(),
            0,
            Some(FlushTiming::start(Instant::now())),
        ));
        self.continue_isolated(turn, exits)
    }

    /// Delivers what the batch just admitted flushed on touch, the output of the pending pass's call, through the
    /// step's output entry as the step's own, with what its emission filter skipped, and counts the contexts it
    /// visited.
    fn deliver_touched(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) {
        self.deliver_step_output(turn, exits);
        let Self { generations, state, .. } = self;
        let Some(state) = state.as_deref_mut() else { return };
        let StateSlot {
            drain,
            instances,
            counts,
            ..
        } = state;
        let Some(drain) = drain.as_mut() else { return };
        let (Some(step), Some(pass)) = (drain.current.as_ref(), drain.pass.as_mut()) else {
            return;
        };
        let Some(instance) = instances.get_mut(step.instance) else {
            return;
        };
        let mut progress = PassProgress::default();
        ask(&mut instance.kernel, &mut progress);
        counts.touched += pass.progress(progress) as u64;
        let Some(live) = generations.iter_mut().find(|live| live.id == step.generation.id()) else {
            return;
        };
        if live.telemetry.has_pushed_filter(step.kernel) {
            let mut take = TakeSuppressed::default();
            ask(&mut instance.kernel, &mut take);
            for (_, suppressed) in take.counts {
                add_loss(
                    &mut live.counts.pushed[step.kernel],
                    suppressed.records,
                    suppressed.points,
                );
            }
        }
    }

    /// Runs a batch through its generation's program, admitting what its state steps reach into the instances of the
    /// region's state.
    fn run_isolated(
        &mut self, turn: &mut RegionTurn<'_>, header: &BatchHeader, batch: EventsBuffer, exits: &mut ExitSink,
    ) {
        let Some(index) = self.live_index_for(header, &batch) else {
            return;
        };
        let Some(entry) = self.generations[index].code.entry_index(header) else {
            self.reject(Fault::NoEntry, header, &batch);
            return;
        };
        let now = turn.now();
        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            state,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        let LiveGeneration {
            id,
            program,
            code,
            layout,
            kernels: kernel_slots,
            decisions,
            counts,
            ..
        } = &mut generations[index];
        cursor.generation = Some(*id);
        Run {
            layout,
            kernel_slots,
            functions: &code.decisions,
            decisions,
            counts,
            slab: kernels,
            instances: Some(StateAdmission::new(state, now)),
            keyed: &mut [],
            state_params: program.state_params(),
            scratch,
            cursor,
            header,
            turn,
            exits,
        }
        .entry(&code.entries[entry], std::iter::once(batch));
        *cursor = Cursor::default();
    }

    /// Runs time-driven work: resumes a drain a panic ended, or fires, gates, or skips every tick now due.
    pub(super) fn advance_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        if let Some(drain) = self.state_mut().drain.as_mut() {
            // Only a drain a panic ended, or a pass that lets batches in, can be left when the host calls this: a
            // yielded one is resumed. A pass runs its next step, as its pace or the host's idle time says, and its
            // instance ticks no more until it completes.
            drain.orphaned = false;
            return self.continue_isolated(turn, exits);
        }
        let now = turn.now();
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        // While a reset window is open, the lane's lineages tick no more: the older epochs' instances wait for their
        // reset drains, and the newest epoch's have no watermark until the window closes. Their deadlines wait for the
        // close, as a gated tick waits for the watermark, so that a lineage catches up the ticks the window held alike
        // whichever of its instances continues it: one the window created for the newest epoch, or the one it drained.
        if state.window.is_open() {
            return Progress::Done;
        }
        let wall = state.wall.now();
        // Ticks whose grace ended fire before the ticks due now, as the interpreter fires them.
        let (mut expired, mut fired) = (Vec::new(), Vec::new());
        for handle in state.instances.handles() {
            let bound = stamping(generations, state, handle).is_some();
            let interval = stamping_interval(generations, state, handle);
            let Some(instance) = state.instances.get(handle) else {
                continue;
            };
            if instance.retired || instance.removed || instance.reset_retiring {
                continue;
            }
            let watermark = state.watermark_of(instance);
            let lanes = instance.lanes.is_some();
            match instance.tick {
                TickState::Scheduled { at } if at <= now => {
                    let decision = match instance.lanes.as_ref() {
                        Some(lanes) => {
                            let second = lanes.record.take_second(lanes.ordinal, at, || wall.as_unix_secs());
                            if second.straggler {
                                state.counts.stragglers += 1;
                            }
                            state
                                .gate
                                .due_at_second(bound, watermark, second.second, instance.last_flush)
                        }
                        None => state.gate.due(bound, watermark, wall, instance.last_flush),
                    };
                    match decision {
                        // Skipped work moves on by its interval, so that the deadline that fired is never kept.
                        TickDecision::Skip => skip_tick(state, handle, at),
                        TickDecision::Fire { flush } => {
                            if let Some(kind) = resolve_fire(state, handle, at, flush, interval, true) {
                                fired.push((handle, kind));
                            }
                        }
                        TickDecision::Gate { second } => {
                            let until = now + state.gate.grace();
                            if let Some(instance) = state.instances.get_mut(handle) {
                                instance.tick = TickState::Gated { at, second, until };
                            }
                        }
                    }
                }
                TickState::Gated { at, until, .. } if until <= now => {
                    match state.gate.on_grace(watermark, instance.last_flush) {
                        Some(flush) if bound => {
                            let counted = match instance.lanes.as_ref() {
                                Some(lanes) => lanes.record.count_grace(lanes.ordinal),
                                None => true,
                            };
                            if let Some(kind) = resolve_fire(state, handle, at, flush, interval, false) {
                                // With lanes the grace is each lane's own: the lanes may flush the tick at different
                                // times.
                                if lanes {
                                    state.counts.divergence += 1;
                                }
                                if counted {
                                    state.grace_expired += 1;
                                }
                                expired.push((handle, kind));
                            }
                        }
                        _ => skip_tick(state, handle, at),
                    }
                }
                _ => {}
            }
        }
        let ticks = in_drain_order(generations, state, expired)
            .chain(in_drain_order(generations, state, fired))
            .collect::<Vec<_>>();
        let queue = with_boundaries(generations, state, ticks)
            .into_iter()
            .collect::<VecDeque<_>>();
        if queue.is_empty() {
            return Progress::Done;
        }
        state.drain = Some(DrainCursor::new(queue, 0, Some(FlushTiming::start(Instant::now()))));
        self.continue_isolated(turn, exits)
    }

    /// Finishes the state owner's region after its input ended: completes a drain a panic ended, closes a reset window still
    /// open, and then runs the shutdown drain of every instance.
    pub(super) fn finish_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        self.state_mut().finishing = true;
        if let Some(drain) = self.state_mut().drain.as_mut() {
            // A pass pending completes ahead of everything else once input ended.
            drain.orphaned = false;
            drain.block(Instant::now());
            if self.continue_isolated(turn, exits) == Progress::Yielded {
                return Progress::Yielded;
            }
        }
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        if std::mem::replace(&mut state.shutting_down, true) {
            return Progress::Done;
        }
        // Every upstream point's last header has arrived, so a window still open closes at exact watermarks.
        let closed = state.window.close(&state.tracker);
        if closed.is_empty() {
            let queue = shutdown_queue(generations, state);
            if queue.is_empty() {
                return Progress::Done;
            }
            state.drain = Some(DrainCursor::new(queue, 0, None));
            return self.continue_isolated(turn, exits);
        }
        let mut queue = VecDeque::new();
        let closing = closed.iter().map(|(epoch, _)| *epoch).collect::<Vec<_>>();
        open_window(generations, state, &closing, &mut queue);
        let resets = queue_resets(generations, state, &closed, None, &mut queue);
        state.shutdown_after_drain = true;
        state.drain = Some(DrainCursor::new(queue, resets, None));
        if resets == 0 {
            self.continue_lineages();
        }
        self.continue_isolated(turn, exits)
    }

    /// Takes what the state owner's region's lane did with its time since this was last called: its ticks that took effect,
    /// reset drains, crossings, and continuations past a reset window; nothing for a slot without state.
    #[cfg(any(test, feature = "test-util"))]
    pub fn take_time_events(&mut self) -> Vec<LaneTimeEvent> {
        self.state
            .as_deref_mut()
            .map(|state| std::mem::take(&mut state.time_events))
            .unwrap_or_default()
    }

    /// Returns when the state owner's region next has time-driven work: at once for a drain a panic ended or a pass
    /// that runs ahead of admission, when its next step is due for a pass that lets batches in, never while another
    /// drain is pending, and else the earliest tick deadline or end of a grace.
    pub(super) fn next_deadline_isolated(&self) -> Option<Instant> {
        let state = self.state();
        if let Some(drain) = &state.drain {
            if drain.orphaned || (drain.blocking && drain.pass.is_some()) {
                return Some(self.now.unwrap_or_else(Instant::now));
            }
            return drain.pass.as_ref().filter(|_| drain.current.is_some()).map(Pass::due);
        }
        // The lane's ticks wait for its reset window to close, which only its input does.
        if state.window.is_open() {
            return None;
        }
        state
            .instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed && !instance.reset_retiring)
            .filter_map(|(_, instance)| match instance.tick {
                TickState::Scheduled { at } => Some(at),
                TickState::Gated { until, .. } => Some(until),
                TickState::Unstarted | TickState::Off => None,
            })
            .min()
    }

    /// Runs the drain in progress until it completes, returning [`DrainProgress::Done`], or until the turn's budget
    /// runs out: a pass on pace then lets batches in again ([`DrainProgress::Paused`]), and any other step, a pass that
    /// runs ahead of admission included, yields ([`DrainProgress::Yielded`]).
    fn run_drain(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> DrainProgress {
        loop {
            let Some(drain) = self.state_mut().drain.as_mut() else {
                return DrainProgress::Done;
            };
            if drain.current.is_none() {
                let Some((handle, kind)) = drain.queue.pop_front() else {
                    // A lane whose input ended inside a reset window runs the shutdown drain once it closed it.
                    if std::mem::take(&mut self.state_mut().shutdown_after_drain) {
                        let Self { generations, state, .. } = self;
                        let state = state.as_deref_mut().expect("the drain is the state's");
                        let queue = shutdown_queue(generations, state);
                        if !queue.is_empty() {
                            state.drain.as_mut().expect("the drain is in progress").queue = queue;
                            continue;
                        }
                    }
                    let state = self.state_mut();
                    if let Some(mut timing) = state.drain.take().and_then(|drain| drain.timing) {
                        let (took, held) = timing.finish(Instant::now());
                        state.lane_telemetry.flush_seconds.record(took.as_secs_f64());
                        state.lane_telemetry.flush_blocked_seconds.record(held.as_secs_f64());
                    }
                    self.collect_instances();
                    return DrainProgress::Done;
                };
                let step = self.start_step(handle, kind);
                match step {
                    Some(step) => {
                        let now = turn.now();
                        let state = self.state_mut();
                        let pass = state.passes(&step).then(|| Pass::start(now, step.interval));
                        let drain = state.drain.as_mut().expect("the drain is in progress");
                        drain.current = Some(step);
                        drain.pass = pass;
                        // A pass on pace lets batches in from its first step on.
                        if pass.is_some() && !drain.blocking {
                            if let Some(timing) = drain.timing.as_mut() {
                                timing.release(Instant::now());
                            }
                        }
                    }
                    None => {
                        if let DrainKind::Reset { ends_epochs, .. } = kind {
                            self.reset_completed(ends_epochs);
                        }
                        continue;
                    }
                }
            }
            if self.run_step(turn, exits) == Progress::Yielded {
                return self.step_yielded(turn.now());
            }
            self.forward_drain_progress(exits);
            self.complete_step();
        }
    }

    /// Accounts for a drain step that yielded at `now`: a pass records how far it has come, and lets batches in again
    /// while it is on pace, unless a batch waits for an entry of the drain behind it to start; once behind its pace, or
    /// once a tick the drain keeps from firing is due, its own instance's next one included, the pass runs ahead of
    /// admission until it completes, as every other step does, so that no batch admitted after the tick's deadline
    /// enters before the tick is decided.
    fn step_yielded(&mut self, now: Instant) -> DrainProgress {
        let overdue = self.state().tick_due_behind_drain(now);
        let StateSlot {
            drain,
            instances,
            stashed,
            ..
        } = self.state_mut();
        let waiting = stashed.is_some();
        let Some(drain) = drain.as_mut() else {
            return DrainProgress::Yielded;
        };
        if let (Some(pass), Some(step)) = (drain.pass.as_mut(), drain.current.as_ref()) {
            pass.steps = pass.steps.saturating_add(1);
            if let Some(instance) = instances.get_mut(step.instance) {
                let mut progress = PassProgress::default();
                ask(&mut instance.kernel, &mut progress);
                pass.progress(progress);
            }
            let behind = pass.behind(now) || overdue;
            if !drain.blocking && behind {
                drain.block(Instant::now());
            }
            let waits_behind = waiting && !drain.queue.is_empty();
            if !drain.blocking && !waits_behind {
                return DrainProgress::Paused;
            }
        }
        if let Some(timing) = drain.timing.as_mut() {
            timing.hold(Instant::now());
        }
        DrainProgress::Yielded
    }

    /// Decides the drain of one instance: its generation, flush time, parameters, and stamp. Returns `None` for an
    /// instance there is nothing to drain for: gone, retired, removed, or bound by no published generation, which was
    /// created by a generation prepared and not published, so it holds nothing. A tick that does not run moves on by
    /// the instance's interval, or to the next instant of its lineage's record, so that the deadline that fired is
    /// never kept.
    fn start_step(&mut self, handle: InstanceHandle, kind: DrainKind) -> Option<DrainStep> {
        let step = self.decide_step(handle, kind);
        if step.is_none() {
            if let DrainKind::Tick {
                scheduled,
                ordinal,
                next,
                ..
            } = kind
            {
                let state = self.state_mut();
                let lane = state.lane.index;
                if let Some(instance) = state.instances.get_mut(handle).filter(|instance| !instance.retired) {
                    instance.tick = TickState::Scheduled {
                        at: next.unwrap_or(scheduled + instance.interval),
                    };
                    if let (Some(lanes), Some(ordinal)) = (instance.lanes.as_mut(), ordinal) {
                        // The kernel never resolved the tick: the lane resolves it, holding nothing to flush.
                        lanes.ordinal = lanes.ordinal.max(ordinal + 1);
                        lanes.record.share().skipped(instance.epoch, ordinal, lane);
                    }
                }
            }
        }
        step
    }

    fn decide_step(&mut self, handle: InstanceHandle, kind: DrainKind) -> Option<DrainStep> {
        let exits = self.scratch.written.len();
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        let instance = state.instances.get(handle)?;
        if instance.removed || instance.retired {
            return None;
        }
        let generation = match kind {
            // A final retirement is stamped with the generation the instance served.
            DrainKind::FinalRetire => state.retiring_of(handle).map(|retiring| Arc::clone(&retiring.owner)),
            _ => stamping(generations, state, handle),
        }?;
        let instance = state.instances.get(handle)?;
        let bound = state.bound(handle);
        let Some(live) = generations.iter().find(|live| live.id == generation.id()) else {
            debug_assert!(false, "a drain's generation is live in the region");
            return None;
        };
        let Some(kernel) = live
            .kernels
            .iter()
            .position(|binding| *binding == KernelBinding::Instance(bound))
        else {
            debug_assert!(false, "a drain's generation binds the instance");
            return None;
        };
        let code = live.code.state_kernel(kernel)?;
        let params = code
            .params
            .and_then(|params| live.program.state_param(StateParamsIndex::new(params as u16)))
            .map(|entry| Arc::clone(&entry.params));
        let interval = live
            .program
            .kernel(KernelIndex::new(kernel as u16))
            .and_then(|kernel| kernel.lineage.as_ref())
            .map_or(instance.interval, |lineage| lineage.flush_interval);

        // A retiring instance never closes or fills a bucket at or after its cut, within the cut's publication epoch.
        // With one upstream point, every stamp of that epoch before the crossing is before the cut, so its flush times
        // are too, and the clamp only guards: a flush time past the cut there is a stamping or ordering fault.
        let cut = state.retiring_of(handle).and_then(|retiring| retiring.by.cut());
        let before_cut = |flush: AdmissionTime, in_epoch: bool| cut.is_none_or(|cut| !in_epoch || flush <= cut.at);
        let (flush, stamp_at) = match kind {
            DrainKind::Tick { flush, ordinal, .. } => {
                let in_epoch = cut.is_some_and(|cut| state.tracker.is_publication_epoch(cut, instance.epoch));
                debug_assert!(
                    before_cut(flush, in_epoch),
                    "a retiring instance's tick never passes its cut in the cut's publication epoch"
                );
                // A tick queued before a reset mark was set is at or before its cutoff.
                debug_assert!(
                    ordinal
                        .zip(instance.lanes.as_ref().and_then(|lanes| lanes.record.mark()))
                        .is_none_or(|(ordinal, mark)| ordinal <= mark.cutoff),
                    "a tick after its record's reset mark fired"
                );
                let flush = retiring_flush_time(flush, cut, in_epoch);
                (Some(flush), flush)
            }
            DrainKind::Reset {
                pre,
                stamp_at,
                in_publication_epoch,
                ..
            } => {
                // With one upstream point every stamp of the publication epoch before the reset is before the cut. With
                // several, a point's routed buffer past the cut can still be held in its keyed exit when another point
                // resets: the window holds the crossing, and the epoch's final watermark passes the cut, which the
                // clamp below covers.
                debug_assert!(
                    state.tracker.upstream().nth(1).is_some()
                        || pre.is_none_or(|pre| before_cut(pre, in_publication_epoch)),
                    "a retiring instance's reset never flushes past its cut in the cut's publication epoch"
                );
                let pre = pre.map(|pre| retiring_flush_time(pre, cut, in_publication_epoch));
                (pre, pre.unwrap_or(stamp_at))
            }
            DrainKind::FinalRetire => {
                // The cut is armed before any generation it publishes is stamped, so a crossing always has one.
                let at = match cut {
                    Some(cut) => cut.at,
                    None => {
                        debug_assert!(false, "an instance retires only at an armed cut");
                        state.watermark_of(instance).unwrap_or(AdmissionTime::UNIX_EPOCH)
                    }
                };
                (Some(at), at)
            }
            DrainKind::Shutdown { at } => {
                let at = cut.map_or(at, |cut| at.min(cut.at));
                (Some(at), at)
            }
            DrainKind::Boundary { at } => (Some(at), at),
        };
        // Which shared flush of its lineage the call is: the kernel resolves it in the state owner's record, which says
        // where its idle-counter zeros start, and tells the lane when it could not ([`TakeLaneFallbacks`]).
        let lane_tick = instance.lanes.as_ref().map(|_| LaneTick {
            epoch: instance.epoch,
            flush: match kind {
                DrainKind::Tick {
                    ordinal: Some(ordinal), ..
                } => LaneFlush::Ordinal(ordinal),
                _ => LaneFlush::Closing,
            },
            lane: state.lane.index,
        });
        let mut latch = state.output;
        let stamp = latch.stamp(stamp_at, Arc::clone(&generation));
        Some(DrainStep {
            instance: handle,
            kind,
            generation,
            kernel,
            output: code.output,
            params,
            interval,
            flush,
            stamp,
            latch,
            lane_tick,
            written: vec![false; exits],
        })
    }

    /// Runs the current drain step's kernel call once, and its time-driven output through the generation's entry.
    fn run_step(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        let Self {
            generations,
            scratch,
            cursor,
            state,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        let step = state
            .drain
            .as_ref()
            .and_then(|drain| drain.current.as_ref())
            .expect("a drain step is in progress");
        let Some(index) = generations.iter().position(|live| live.id == step.generation.id()) else {
            return Progress::Done;
        };
        let Some(instance) = state.instances.get_mut(step.instance) else {
            return Progress::Done;
        };

        *cursor = Cursor {
            generation: Some(step.generation.id()),
            node: Some(NodeRef::Kernel(step.kernel)),
            instance: Some(step.instance),
        };
        let slot = &mut instance.kernel;
        let pushed = generations[index].telemetry.has_pushed_filter(step.kernel);
        let (progress, suppressed) = {
            let token = slot.context.resource_group();
            let KernelSlot { kernel, ports, .. } = slot;
            // A drain step is long and rare: its CPU time is read on entry whatever the sampling.
            let _entered = token.as_ref().map(|token| token.enter_measured());
            let mut kernel_turn = Turn::new(&step.stamp, turn.now(), turn.budget_mut(), ports, &mut scratch.counters)
                .with_watermark(step.flush);
            if let Some(params) = &step.params {
                kernel_turn = kernel_turn.with_param(params.as_ref());
            }
            if let Some(tick) = step.lane_tick {
                kernel_turn = kernel_turn.with_lane_tick(tick);
            }
            let progress = match step.kind {
                DrainKind::Tick { .. } | DrainKind::Boundary { .. } => kernel.advance_time(&mut kernel_turn),
                DrainKind::Reset { .. } => kernel.control(&mut kernel_turn, &mut ClockReset),
                DrainKind::FinalRetire => kernel.finish(&mut kernel_turn, DrainPolicy::Retire),
                DrainKind::Shutdown { .. } => kernel.finish(&mut kernel_turn, DrainPolicy::Shutdown),
            };
            // What the call skipped at emission, for the filter R5b pushed into it: a read-only message, which
            // leaves a yielded call in place. A call that panics never gets here, and its records are discarded.
            let mut take = TakeSuppressed::default();
            if pushed {
                kernel.control(&mut kernel_turn, &mut take);
            }
            (progress, take.counts)
        };
        cursor.instance = None;

        let live = &mut generations[index];
        fold_loss(&mut live.counts.kernel_loss[step.kernel], &mut scratch.counters);
        // Each suppressed record is one the filter would have received and sent to its `drop`: counted in the
        // filter's own series, under the generation whose filter judged it.
        for (_, suppressed) in suppressed {
            add_loss(
                &mut live.counts.pushed[step.kernel],
                suppressed.records,
                suppressed.points,
            );
        }
        *cursor = Cursor::default();
        self.deliver_step_output(turn, exits);
        progress
    }

    /// Runs what the current drain step's kernel wrote, in a call of the step or while it admitted a batch with the
    /// step's call yielded, through the step's generation's entry for its time-driven output, which hands it to the
    /// exit its edge leaves by, stamped with the step's header. What it writes to each exit is the step's own.
    fn deliver_step_output(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) {
        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            state,
            ..
        } = self;
        let Some(state) = state.as_deref_mut() else { return };
        let StateSlot {
            drain,
            instances,
            output: clock,
            ..
        } = state;
        let Some(step) = drain.as_mut().and_then(|drain| drain.current.as_mut()) else {
            return;
        };
        let Some(index) = generations.iter().position(|live| live.id == step.generation.id()) else {
            return;
        };
        let Some(instance) = instances.get_mut(step.instance) else {
            return;
        };
        let slot = &mut instance.kernel;
        if slot.ports.is_empty() {
            return;
        }
        let LiveGeneration {
            program,
            code,
            layout,
            kernels: kernel_slots,
            decisions,
            counts,
            ..
        } = &mut generations[index];
        let mut events = 0u64;
        let output = slot
            .ports
            .drain(PortIndex::PRIMARY)
            .inspect(|buffer| events += buffer.len() as u64);
        let entry = &code.entries[step.output];
        *cursor = Cursor {
            generation: Some(step.generation.id()),
            node: None,
            instance: None,
        };
        std::mem::swap(&mut scratch.written, &mut step.written);
        Run {
            layout,
            kernel_slots,
            functions: &code.decisions,
            decisions,
            counts,
            slab: kernels,
            instances: None,
            keyed: &mut [],
            state_params: program.state_params(),
            scratch,
            cursor,
            header: &step.stamp,
            turn,
            exits,
        }
        .entry(entry, output);
        std::mem::swap(&mut scratch.written, &mut step.written);
        slot.ports.clear();
        if events > 0 {
            // The output's admission time is the latest of what the slot emitted, never of a step that emitted nothing.
            *clock = step.latch;
        }
        let live = &mut generations[index];
        live.counts.kernel_ports[live.layout.kernel_ports[step.kernel]] += events;
        *cursor = Cursor::default();
    }

    /// Forwards the progress of the state owner's derived point after a drain step that flushed, at its stamp, as a
    /// header-only batch: on every exit toward a state owner downstream that lists the point, which counts the point's
    /// time only from such a batch, so that its watermark reaches the flush time once the drain's whole output has, and
    /// even when the flush emits nothing there. A step that sends progress moves the output
    /// clock as one that emits does, so that the point's stamps only grow.
    ///
    /// After a lineage's last drain under the generation a cut replaces, its boundary flush or its final retirement,
    /// once every upstream point switched to the cut's generation, every such exit also receives the progress of the
    /// cut's generation at the same time: the state owner stamps it from now on, and a retiring lineage downstream,
    /// which has received everything the replaced generation sends it at the point, crosses at it rather than at the
    /// state owner's next drain, which may be a flush interval away.
    fn forward_drain_progress(&mut self, exits: &mut ExitSink) {
        let Self { state, progress, .. } = self;
        let Some(state) = state.as_deref_mut() else { return };
        let Some(step) = state.drain.as_ref().and_then(|drain| drain.current.as_ref()) else {
            return;
        };
        // A reset's and a shutdown's drains forward their stamp only where they wrote, as do drains while another
        // lineage of the state owner retires before its final flush: a state owner downstream would otherwise see the
        // new generation at the point before the retiring lineage's last output.
        let closing = matches!(step.kind, DrainKind::Reset { .. } | DrainKind::Shutdown { .. });
        let retiring = state.instances.iter().any(|(handle, instance)| {
            handle != step.instance && instance.retiring.is_some() && !instance.retired && !instance.removed
        });
        let Some(mark) = SentMark::of(&step.stamp) else { return };
        let point = state.output.point();
        let mut sent = false;
        for exit in progress.iter_mut() {
            let Some(position) = exit.points.iter().position(|upstream| *upstream == point) else {
                continue;
            };
            let wrote = step.written.get(exit.exit.get()).copied().unwrap_or(false);
            if (closing || retiring) && !wrote {
                continue;
            }
            // What the drain wrote carries no progress downstream ([`WatermarkTracker::observe_after`]): the drain's
            // progress goes once, now that its whole output has, so that a state owner downstream never closes a bucket
            // the rest of the output still reaches.
            let mut header = step.stamp.clone();
            header.entry = None;
            exits.emit(exit.exit, EventsBuffer::with_capacity(0).with_header(header));
            sent = true;
            exit.last[position] = Some(mark);
        }
        if sent {
            state.output = step.latch;
        }
        if closing || retiring {
            return;
        }

        let by = match step.kind {
            DrainKind::Boundary { .. } => state
                .instances
                .get(state.bound(step.instance))
                .and_then(|instance| instance.boundary.as_ref())
                .map(|boundary| &boundary.by),
            DrainKind::FinalRetire => state.retiring_of(step.instance).map(|retiring| &retiring.by),
            _ => None,
        };
        let Some(by) = by.filter(|by| state.tracker.switched(by.id())).map(Arc::clone) else {
            return;
        };
        let mut latch = step.latch;
        let Some(at) = step.stamp.admitted_at else { return };
        let handover = latch.stamp(at, by);
        let Some(mark) = SentMark::of(&handover) else { return };
        let mut sent = false;
        for exit in progress.iter_mut() {
            let Some(position) = exit.points.iter().position(|upstream| *upstream == point) else {
                continue;
            };
            exits.emit(exit.exit, EventsBuffer::with_capacity(0).with_header(handover.clone()));
            exit.last[position] = Some(mark);
            sent = true;
        }
        if sent {
            state.output = latch;
        }
    }

    /// Completes the current drain step: moves its instance's tick on, forgets its flushes after a reset, or retires
    /// it, and, after a clock reset's last instance of an epoch, starts the output's next epoch.
    fn complete_step(&mut self) {
        let state = self.state_mut();
        let Some(drain) = state.drain.as_mut() else { return };
        let Some(step) = drain.current.take() else { return };
        drain.pass = None;
        let retiring = state.retiring_of(step.instance).is_some();
        if let Some(instance) = state.instances.get_mut(step.instance) {
            complete_instance(instance, &step, retiring);
        }
        #[cfg(any(test, feature = "test-util"))]
        if let Some(event) = state
            .instances
            .get(step.instance)
            .and_then(|instance| time_event(instance, &step))
        {
            state.time_events.push(event);
        }
        if matches!(step.kind, DrainKind::FinalRetire) {
            state.counts.cut_crossings += 1;
            // A retired instance holds nothing and never ticks again: it leaves its lineage's records.
            state.leave(step.instance);
        }
        if let DrainKind::Reset { ends_epochs, .. } = step.kind {
            self.reset_completed(ends_epochs);
        }
    }

    /// Accounts for one completed reset drain: starts the output's next epoch as many times as it ends, and once the
    /// last reset drain completed, continues every lineage past the epochs the window closed.
    fn reset_completed(&mut self, ends_epochs: u32) {
        let state = self.state_mut();
        for _ in 0..ends_epochs {
            state.output.reset();
        }
        let done = state.drain.as_mut().is_some_and(|drain| {
            drain.resets = drain.resets.saturating_sub(1);
            drain.resets == 0
        });
        if done {
            self.continue_lineages();
        }
    }

    /// Continues every lineage past the epochs a reset window closed, once their drains completed: the lineage's
    /// instance of the lane's newest epoch takes its bindings, if the window created one, and the drained instance is
    /// freed; otherwise the drained instance continues itself in the newest epoch, empty and as if it had never
    /// flushed, unless its drain was its final retirement at a cut.
    fn continue_lineages(&mut self) {
        let Self { generations, state, .. } = self;
        let Some(state) = state.as_deref_mut() else { return };
        let target = state.window.target();
        let drained = state
            .instances
            .iter()
            .filter(|(_, instance)| instance.reset_retiring && !state.window.holds(instance.epoch))
            .map(|(handle, _)| handle)
            .collect::<Vec<_>>();
        if !drained.is_empty() {
            state.counts.reset_crossings += 1;
        }
        // The bound instances first, so that an instance standing beside one finds where its lineage continues.
        let mut drained = drained;
        drained.sort_by_key(|handle| {
            state
                .instances
                .get(*handle)
                .is_some_and(|instance| instance.primary.is_some())
        });
        for handle in drained {
            let bound = state.bound(handle);
            let continuing = state.in_epoch(bound, target).filter(|continuing| *continuing != handle);
            // A lineage that continues through the drained instance keeps its schedule, as a stage 6 reset does: with
            // lanes, the successor record starts at the current deadline of the lineage's record of the newest epoch
            // the window closed, the instant of the ordinal after its reset mark's cutoff, never at this lane's own
            // deadline, which a window that opened late moved past ordinals the lane skipped, nor at an older epoch's.
            // Another lane may reach the drained epoch later, as one that hears of the reset late does: it ticks the
            // epoch on the record this lane leaves, which the slot keeps.
            let resume_at = predecessor_deadline(state, bound, target);
            state.leave(handle);
            match continuing {
                Some(continuing) => {
                    if bound == handle {
                        rebind(generations, state, handle, continuing);
                    }
                    if let Some(instance) = state.instances.get_mut(handle) {
                        instance.reset_retiring = false;
                        instance.removed = true;
                    }
                }
                None if bound == handle => {
                    let Some(instance) = state.instances.get_mut(handle) else {
                        continue;
                    };
                    instance.reset_retiring = false;
                    if std::mem::take(&mut instance.retires_at_close) {
                        // Its drain at its cut's time in the publication epoch was its final retirement.
                        instance.retired = true;
                        instance.retiring = None;
                        instance.tick = TickState::Off;
                        state.counts.cut_crossings += 1;
                        continue;
                    }
                    #[cfg(any(test, feature = "test-util"))]
                    let from = instance.epoch;
                    instance.epoch = target;
                    let next_at = state.join(handle, resume_at);
                    if let Some(at) = next_at {
                        if let Some(instance) = state.instances.get_mut(handle) {
                            instance.tick = TickState::Scheduled { at };
                        }
                    }
                    #[cfg(any(test, feature = "test-util"))]
                    if let Some(created_by) = state.instances.get(handle).map(|instance| instance.created_by) {
                        state.time_events.push(LaneTimeEvent::Continued {
                            created_by,
                            from,
                            epoch: target,
                            resume_at,
                            next_at,
                        });
                    }
                }
                None => {
                    // An instance of an epoch between: its lineage continues through the bound instance.
                    if let Some(instance) = state.instances.get_mut(handle) {
                        instance.reset_retiring = false;
                        instance.removed = true;
                    }
                }
            }
        }
        // The lane holds no epoch older than its window's oldest open one, or its newest: records of the epochs every
        // lane passed need no keeping.
        if let Some(slot_lanes) = state.lanes.as_ref() {
            let oldest = state.window.older().first().copied().unwrap_or(target);
            slot_lanes.pass(state.lane.index, oldest);
        }
        if state.drain.is_none() {
            self.collect_instances();
        }
    }

    /// Frees every instance no live generation binds and no drain refers to, counting what one still held as invalid
    /// loss of its node: an instance holds nothing once its lineage retired.
    pub(super) fn collect_instances(&mut self) {
        let Self {
            state, invalid, name, ..
        } = self;
        let Some(state) = state.as_deref_mut() else { return };
        if state.drain.is_some() {
            return;
        }
        // Instances standing beside a bound one go with it, so they are judged before any is freed.
        let unused = state
            .instances
            .handles()
            .into_iter()
            .filter(|handle| state.unused(*handle))
            .collect::<Vec<_>>();
        for handle in unused {
            state.leave(handle);
            let Some(mut instance) = state.instances.remove(handle) else {
                continue;
            };
            if state.lanes.is_some() {
                state.counts.divergence += take_fallbacks(&mut instance.kernel);
            }
            let held = instance.kernel.kernel.held();
            if held != LossCount::default() {
                warn!(
                    region = %name,
                    node = %instance.key.node,
                    contexts = held.events,
                    points = held.points,
                    "Freed an instance of a state owner's state that still held points, which only an installation \
                     that removes a state owner without a cut leaves; counting them as invalid loss."
                );
                add_loss(&mut invalid.pending, held.events, held.points);
            }
            debug!(region = %name, node = %instance.key.node, created_by = %instance.created_by, "Freed a lineage.");
        }
    }

    /// Releases a generation's uses of the instances it binds; an instance a drain refers to is marked removed and
    /// freed once the drain ends.
    pub(super) fn release_instances(&mut self, bindings: &[KernelBinding]) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for binding in bindings {
            let KernelBinding::Instance(handle) = *binding else {
                continue;
            };
            let referred = state.drain_refers_to(handle);
            if let Some(instance) = state.instances.get_mut(handle) {
                instance.users = instance.users.saturating_sub(1);
                if instance.users == 0 && referred {
                    instance.removed = true;
                }
            }
        }
        self.collect_instances();
    }

    /// Returns the marks a preparation's bindings set on the region's instances: each instance a
    /// [`StateBinding::Retire`] retires, and each a [`StateBinding::Boundary`] makes a boundary flush, with the
    /// generation it served, which the region runs.
    pub(super) fn state_marks(&self, bindings: &[StateBinding]) -> Result<StateMarks, String> {
        let mut marks = StateMarks::default();
        for binding in bindings {
            let (node, key, from, retires) = match binding {
                StateBinding::Retire { node, key, from } => (node, key, *from, true),
                StateBinding::Boundary { node, key, from } => (node, key, *from, false),
                StateBinding::Retain { .. } | StateBinding::New { .. } => continue,
            };
            let found = self.state.as_ref().and_then(|_| self.instance_of(from, key));
            let served = self
                .generations
                .iter()
                .find(|live| live.id == from)
                .and_then(|live| live.token.upgrade());
            let (Some(handle), Some(served)) = (found, served) else {
                return Err(format!(
                    "the preparation {} the instance of `{}` that generation {} uses, which the region does not hold",
                    if retires {
                        "retires"
                    } else {
                        "makes a boundary flush of"
                    },
                    node,
                    from
                ));
            };
            if retires {
                marks.retiring.push((handle, served));
            } else {
                marks.boundary.push((handle, served));
            }
        }
        Ok(marks)
    }

    /// Sets the marks of a preparation of `generation`, or of a retirement at its cut, on the region's instances.
    pub(super) fn apply_state_marks(&mut self, marks: StateMarks, generation: &GenerationRef) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for (handle, owner) in marks.retiring {
            if let Some(instance) = state.instances.get_mut(handle) {
                instance.retiring = Some(Retiring {
                    owner,
                    by: Arc::clone(generation),
                });
            }
        }
        for (handle, served) in marks.boundary {
            if let Some(instance) = state.instances.get_mut(handle) {
                instance.boundary = Some(Boundary {
                    served,
                    by: Arc::clone(generation),
                });
            }
        }
    }

    /// Undoes what an aborted generation's preparation did to the state: the retiring and boundary marks it set, and
    /// the instances it created, which no other generation can bind yet.
    pub(super) fn abort_state(&mut self, generation: GenerationId) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for handle in state.instances.handles() {
            let referred = state.drain_refers_to(handle);
            let Some(instance) = state.instances.get_mut(handle) else {
                continue;
            };
            if instance
                .retiring
                .as_ref()
                .is_some_and(|retiring| retiring.by.id() == generation)
            {
                instance.retiring = None;
            }
            if instance
                .boundary
                .as_ref()
                .is_some_and(|boundary| boundary.by.id() == generation)
            {
                instance.boundary = None;
            }
            if instance.created_by == generation && referred {
                instance.removed = true;
            }
        }
    }

    /// Resolves how a preparation binds a state owner's kernel: to the instance its explicit binding names, or else to
    /// the region's instance of its lineage that is not retiring, or else to the new instance the preparation brings.
    pub(super) fn resolve_state(
        &self, kernel: &ProgramKernel, bindings: &[StateBinding], supplied: &mut BTreeMap<KernelKey, PreparedKernel>,
    ) -> Result<StateResolution, String> {
        let lineage = kernel
            .lineage
            .as_ref()
            .ok_or_else(|| format!("kernel `{}` is no state owner's", kernel.key.node))?;
        let state = self.state();
        let binding = bindings.iter().find(|binding| match binding {
            StateBinding::Retain { node, key, .. }
            | StateBinding::Boundary { node, key, .. }
            | StateBinding::New { node, key } => node == &kernel.key.node && key == &lineage.key,
            StateBinding::Retire { .. } => false,
        });
        let new = |supplied: &mut BTreeMap<_, _>| {
            supplied
                .remove(&kernel.key)
                .map(|prepared| StateResolution::New(Box::new(prepared), kernel.ports.len()))
                .ok_or_else(|| {
                    format!(
                        "the state owner `{}` needs a new instance of its lineage, and the preparation brought no \
                         kernel for it",
                        kernel.key.node
                    )
                })
        };
        match binding {
            Some(StateBinding::Retain { from, .. } | StateBinding::Boundary { from, .. }) => self
                .instance_of(*from, &lineage.key)
                .map(StateResolution::Retain)
                .ok_or_else(|| {
                    format!(
                        "the state owner `{}` retains the instance of its lineage that generation {} uses, which \
                         the region does not hold",
                        kernel.key.node, from
                    )
                }),
            Some(StateBinding::New { .. }) => new(supplied),
            Some(StateBinding::Retire { .. }) => unreachable!("retirements are no bindings of a kernel"),
            None => {
                let shared = state.instances.iter().find(|(_, instance)| {
                    instance.key == lineage.key
                        && instance.users > 0
                        && instance.primary.is_none()
                        && instance.retiring.is_none()
                        && !instance.retired
                        && !instance.removed
                });
                match shared {
                    Some((handle, _)) => Ok(StateResolution::Retain(handle)),
                    None => new(supplied),
                }
            }
        }
    }

    /// Returns the instance of `key` the live generation `generation` binds, if any.
    pub(super) fn instance_of(&self, generation: GenerationId, key: &LineageKey) -> Option<InstanceHandle> {
        let state = self.state();
        let live = self.generations.iter().find(|live| live.id == generation)?;
        live.kernels.iter().find_map(|binding| match *binding {
            KernelBinding::Instance(handle) => state
                .instances
                .get(handle)
                .filter(|instance| &instance.key == key && !instance.retired)
                .map(|_| handle),
            KernelBinding::Slab(_) => None,
        })
    }

    /// Adds a new instance of a state owner's state, created by `generation`, holding data of the lane's newest epoch,
    /// ticking in the phase of another instance of its node, or one interval from now; with lanes, a member of its
    /// lineage's record in that epoch, whose first tick takes that phase.
    pub(super) fn insert_instance(
        &mut self, generation: GenerationId, kernel: &ProgramKernel, prepared: PreparedKernel, ports: usize,
        now: Instant,
    ) -> InstanceHandle {
        let lineage = kernel
            .lineage
            .as_ref()
            .expect("only a state owner's kernel has an instance");
        let state = self.state_mut();
        let phase = state
            .instances
            .iter()
            .filter(|(_, instance)| instance.key.node == lineage.key.node && !instance.retired)
            .find_map(|(_, instance)| match &instance.lanes {
                // The replaced record's current deadline: every lane's new instance shares it.
                Some(lanes) => lanes.record.current_deadline().or(instance.tick.at()),
                None => instance.tick.at(),
            });
        let tick = match (state.started, phase) {
            (false, _) => TickState::Unstarted,
            (true, Some(at)) => TickState::Scheduled { at },
            (true, None) => TickState::Scheduled {
                at: now + lineage.flush_interval,
            },
        };
        let mut kernel = KernelSlot::new(prepared, ports);
        state.wrap_kernel(&mut kernel);
        let handle = state.instances.insert(Instance {
            key: lineage.key.clone(),
            created_by: generation,
            kernel,
            users: 1,
            interval: lineage.flush_interval,
            tick,
            last_flush: None,
            retiring: None,
            boundary: None,
            retired: false,
            removed: false,
            epoch: state.window.target(),
            primary: None,
            reset_retiring: false,
            retires_at_close: false,
            lanes: None,
        });
        let started = state.started;
        let first = started.then(|| tick.at()).flatten();
        if let Some(at) = state.join(handle, first) {
            if let Some(instance) = state.instances.get_mut(handle).filter(|_| started) {
                instance.tick = TickState::Scheduled { at };
            }
        }
        handle
    }

    /// Delivers a control message to every live instance the target selects, then replies with it.
    pub(super) fn kernel_control_isolated(&mut self, turn: &mut RegionTurn<'_>, control: KernelControl) {
        // The slot's coordinator routes a message to the lane it names; a lane handles what reaches it.
        let KernelControl {
            target,
            lane: _,
            sub_region: _,
            mut message,
            reply,
        } = control;
        let Self {
            generations,
            state,
            scratch,
            cursor,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only a state owner's region runs its state");
        for handle in state.instances.handles() {
            let generation = stamping(generations, state, handle);
            let bound = state.bound(handle);
            let Some(instance) = state.instances.get_mut(handle) else {
                continue;
            };
            let selected = match &target {
                KernelTarget::Node(node) => &instance.key.node == node,
                KernelTarget::Stateful => true,
            };
            if !selected || instance.retired || instance.removed {
                continue;
            }
            // A panic is charged to the generation whose time-driven output the instance's state would go to, or the
            // one that created it, and to the node it runs.
            let charged = generation.map_or(instance.created_by, |generation| generation.id());
            let node = generations
                .iter()
                .find(|live| live.id == charged)
                .and_then(|live| {
                    live.kernels
                        .iter()
                        .position(|binding| *binding == KernelBinding::Instance(bound))
                })
                .map(NodeRef::Kernel);
            *cursor = Cursor {
                generation: Some(charged),
                node,
                instance: Some(handle),
            };
            let slot = &mut instance.kernel;
            let header = BatchHeader::default();
            let progress = slot.context.accounted(|| {
                let mut kernel_turn = Turn::new(
                    &header,
                    turn.now(),
                    turn.budget_mut(),
                    &mut slot.ports,
                    &mut scratch.counters,
                );
                slot.kernel.control(&mut kernel_turn, message.as_mut())
            });
            debug_assert_eq!(
                progress,
                Progress::Done,
                "a read-only control message completes at once"
            );
            if !slot.ports.is_empty() {
                // A read-only message writes nothing; anything written is dropped as invalid.
                let lost = slot
                    .ports
                    .drain(PortIndex::PRIMARY)
                    .fold(LossCount::default(), |mut lost, buffer| {
                        lost.events += buffer.len() as u64;
                        lost.points += points_in(&buffer);
                        lost
                    });
                scratch
                    .counters
                    .record_loss(LossClass::Invalid, lost.events, lost.points);
                slot.ports.clear();
            }
        }
        *cursor = Cursor::default();
        reply(message);
    }

    /// Recovers a state owner's region from a panic: charges it to the generation whose call ran, rebuilds the instance
    /// whose kernel panicked empty, and keeps the rest of a drain the panic ended for the next call to resume.
    ///
    /// Returns the instance that panicked, if a state owner's kernel did.
    pub(super) fn recover_state(&mut self, cursor: &Cursor) -> Option<InstanceHandle> {
        let state = self.state.as_deref_mut()?;
        state.stashed = None;
        let panicked = cursor.instance;
        let mut reset_ended = None;
        if let Some(drain) = state.drain.as_mut() {
            drain.orphaned = true;
            // The instance that panicked restarts empty, so its drain is complete; any other one resumes its call.
            if let Some(step) = drain.current.take_if(|step| Some(step.instance) == panicked) {
                let retiring = state.retiring_of(step.instance).is_some();
                let lane = state.lane.index;
                if let Some(instance) = state.instances.get_mut(step.instance) {
                    complete_instance(instance, &step, retiring);
                    if let (
                        Some(lanes),
                        DrainKind::Tick {
                            ordinal: Some(ordinal), ..
                        },
                    ) = (instance.lanes.as_ref(), step.kind)
                    {
                        // Whether or not the kernel recorded the tick before it panicked, the lane resolved it.
                        lanes.record.share().skipped(instance.epoch, ordinal, lane);
                    }
                }
                if matches!(step.kind, DrainKind::FinalRetire) {
                    state.leave(step.instance);
                }
                if let DrainKind::Reset { ends_epochs, .. } = step.kind {
                    reset_ended = Some(ends_epochs);
                }
            }
        }
        for (_, instance) in state.instances.iter_mut() {
            instance.kernel.ports.clear();
        }
        if let Some(ends_epochs) = reset_ended {
            self.reset_completed(ends_epochs);
        }
        panicked
    }

    /// Rebuilds an instance whose kernel panicked, empty, counting what it held as invalid loss of its node in the
    /// generation `charged`; returns why it could not be rebuilt, if it could not.
    ///
    /// With lanes, the rebuilt instance starts as if it had never flushed: its first flush adds no idle-counter zeros,
    /// whatever the other lanes flushed ([`Restarted`]).
    pub(super) fn rebuild_instance(&mut self, handle: InstanceHandle, charged: usize) -> Result<(), String> {
        let Self { generations, state, .. } = self;
        let Some(state) = state.as_deref_mut() else {
            return Ok(());
        };
        let bound = state.bound(handle);
        #[cfg(any(test, feature = "test-util"))]
        let wrap = state.wrap.clone();
        let Some(instance) = state.instances.get_mut(handle) else {
            return Ok(());
        };
        let kernel = &mut instance.kernel;
        let held = catch_unwind(AssertUnwindSafe(|| kernel.kernel.held())).unwrap_or_default();
        let live = &mut generations[charged];
        if let Some(index) = live
            .kernels
            .iter()
            .position(|binding| *binding == KernelBinding::Instance(bound))
        {
            live.counts.kernel_loss[index].record_loss(LossClass::Invalid, held.events, held.points);
        }
        kernel.rebuild()?;
        #[cfg(any(test, feature = "test-util"))]
        if let Some(wrap) = &wrap {
            wrap_kernel(wrap, kernel);
        }
        if instance.lanes.is_some() {
            let header = BatchHeader::default();
            let (mut budget, mut counters) = (Budget::unlimited(), TurnCounters::default());
            let slot = &mut instance.kernel;
            slot.context.accounted(|| {
                let mut turn = Turn::new(&header, Instant::now(), &mut budget, &mut slot.ports, &mut counters);
                slot.kernel.control(&mut turn, &mut Restarted)
            });
        }
        Ok(())
    }

    /// Returns the live generations that bind an instance.
    pub(super) fn binders(&self, handle: InstanceHandle) -> Vec<usize> {
        let bound = self.state.as_deref().map_or(handle, |state| state.bound(handle));
        (0..self.generations.len())
            .filter(|&index| {
                self.generations[index]
                    .kernels
                    .contains(&KernelBinding::Instance(bound))
            })
            .collect()
    }

    /// Publishes the state owner's region's own telemetry, and what its kernel's instances accumulated.
    pub(super) fn publish_state(&mut self) {
        let Some(state) = self.state.as_deref_mut() else { return };
        let on_lanes = state.lanes.is_some();
        for (_, instance) in state.instances.iter_mut() {
            let slot = &mut instance.kernel;
            slot.context.accounted(|| slot.kernel.publish_telemetry());
            if on_lanes {
                // The ticks its kernel resolved apart from the other lanes'.
                state.counts.divergence += take_fallbacks(&mut instance.kernel);
            }
        }
        let telemetry = &state.telemetry;
        if state.clock_resets > 0 {
            telemetry
                .clock_resets
                .increment(std::mem::take(&mut state.clock_resets));
        }
        if state.grace_expired > 0 {
            telemetry
                .tick_grace_expired
                .increment(std::mem::take(&mut state.grace_expired));
        }
        let lineages = state
            .instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed && instance.primary.is_none())
            .count();
        telemetry.lineages.set(lineages as f64);
        let lag = state.tracker.watermark_in(state.window.target()).map(|watermark| {
            let wall = state.wall.now();
            wall.as_unix_nanos().saturating_sub(watermark.as_unix_nanos()) as f64 / 1e9
        });
        if let Some(lag) = lag {
            state.lane_telemetry.watermark_lag.set(lag);
        }
        // On lanes, the slot's lag is its most lagging lane's: every lane publishes its own and sets the slot's from
        // what all of them last published.
        let slot_lag = match &state.lanes {
            Some(lanes) => lanes.publish_watermark_lag(state.lane.index, lag),
            None => lag,
        };
        if let Some(lag) = slot_lag {
            telemetry.watermark_lag.set(lag);
        }
        let contexts = state
            .instances
            .iter_mut()
            .filter(|(_, instance)| !instance.removed)
            .map(|(_, instance)| contexts_held(&mut instance.kernel))
            .sum::<u64>();
        state.lane_telemetry.contexts.set(contexts as f64);
        let overlap = if on_lanes { crossing_overlap(state) } else { 0 };
        let LaneCounts {
            misrouted,
            divergence,
            stragglers,
            overflows,
            cut_crossings,
            reset_crossings,
            touched,
        } = std::mem::take(&mut state.counts);
        let lane = &state.lane_telemetry;
        for (counter, value) in [
            (&lane.misrouted, misrouted),
            (&lane.tick_divergence, divergence),
            (&lane.tick_stragglers, stragglers),
            (&lane.reset_overflows, overflows),
            (&lane.cut_crossings, cut_crossings),
            (&lane.reset_crossings, reset_crossings),
            (&lane.flush_touched_contexts, touched),
        ] {
            if value > 0 {
                counter.increment(value);
            }
        }
        lane.crossing_overlap.set(overlap as f64);
    }

    /// Returns `true` if the state owner's region can take input: not while a drain is left to complete, unless its
    /// current step is a pass on pace, which lets batches in.
    pub(super) fn accepts_isolated(&self) -> bool {
        let state = self.state();
        state.drain.is_none() || state.interleaved_pass().is_some()
    }

    /// Returns `true` if a pass on pace is pending: its host runs its next step whenever no batch waits.
    pub(super) fn has_background_work_isolated(&self) -> bool {
        self.state().interleaved_pass().is_some()
    }

    /// Checks that a state owner's kernel was built with the host's state budgets, if the region has any, and, in a
    /// slot with more than one lane, for this lane of the slot's lanes.
    pub(super) fn check_budget(&self, prepared: &PreparedKernel) -> Result<(), String> {
        let state = self.state();
        if state.lanes.is_some() {
            let lane = state.lane;
            let built_for = prepared.context().lanes().map(|lanes| (lanes.lane, lanes.lanes));
            if built_for != Some((lane.index, lane.count)) {
                return Err(format!(
                    "the kernel of state owner `{}` was not built for {}; build state owners' kernels on lanes with \
                     `KernelBuildContext::with_lanes`, every lane's instance of a lineage against one `LaneCell`",
                    prepared.key().node,
                    lane
                ));
            }
        }
        let Some(budgets) = &state.budgets else {
            return Ok(());
        };
        let shares = prepared
            .context()
            .state_budget()
            .is_some_and(|budget| budget.global.shares_with(budgets.global()));
        if shares {
            Ok(())
        } else {
            Err(format!(
                "the kernel of state owner `{}` was not built with the host's state budget; build state owners' \
                 kernels with `KernelBuildContext::with_state_budget(StateBudgets::context_budget(node))`",
                prepared.key().node
            ))
        }
    }

    /// Logs a generation a region keeps running after a state owner's kernel panicked under it.
    pub(super) fn log_rolling_back(&self, index: usize, node: &str, reason: &str) {
        let generation = &self.generations[index];
        error!(
            region = %self.name,
            generation = %generation.id,
            node,
            reason,
            "A state owner's kernel panicked under an artifact's generation; its state restarted empty, the \
             generation keeps processing its input, and a rollback is requested."
        );
    }
}

/// Opens the lane's reset window over every instance holding data of an epoch the window now holds open as older, or
/// `closing`: each becomes reset-retiring, and ticks no more. A tick gated in the timeline that ended is dropped, as
/// stage 6 drops it at a reset. With lanes, the first lane to open marks the lineage's record, so that no lane fires
/// an ordinal after the newest any lane resolved, and this lane resolves its own ordinals up to that cutoff first:
/// their deadlines have passed, since another lane resolved them; their flushes join `queue`.
///
/// A gated tick is dropped before the record is marked, so that the cutoff covers it: with one point, every lane is
/// gated at the same ordinal, and the lineage continues one interval later, as on one lane.
fn open_window(
    generations: &[LiveGeneration], state: &mut StateSlot, closing: &[u32],
    queue: &mut VecDeque<(InstanceHandle, DrainKind)>,
) {
    if !state.window.is_open() && closing.is_empty() {
        return;
    }
    let target = state.window.target();
    let opening = state
        .instances
        .iter()
        .filter(|(_, instance)| {
            !instance.retired
                && !instance.removed
                && !instance.reset_retiring
                && (state.window.older().contains(&instance.epoch) || closing.contains(&instance.epoch))
        })
        .map(|(handle, _)| handle)
        .collect::<Vec<_>>();
    if opening.is_empty() {
        return;
    }
    let wall = state.wall.now();
    // With one point every lane sees the same headers, so every lane gates a tick alike, and a tick dropped at the
    // opening is one no lane fires; with several, another lane may have fired it.
    let several_points = state.tracker.upstream().nth(1).is_some();
    let mut fired = Vec::new();
    for handle in opening {
        let bound = stamping(generations, state, handle).is_some();
        let interval = stamping_interval(generations, state, handle);
        let lane = state.lane.index;
        let Some(instance) = state.instances.get_mut(handle) else {
            continue;
        };
        instance.reset_retiring = true;
        let Some(lanes) = instance.lanes.as_mut() else {
            if let TickState::Gated { at, .. } = instance.tick {
                instance.tick = TickState::Scheduled {
                    at: at + instance.interval,
                };
            }
            continue;
        };
        let gated = match instance.tick {
            TickState::Gated { at, .. } => {
                let ordinal = lanes.ordinal;
                let resolved = lanes.record.resolve(ordinal, at, instance.interval, false);
                lanes.ordinal = ordinal + 1;
                instance.tick = TickState::Scheduled { at: resolved.next_at };
                Some(ordinal)
            }
            _ => None,
        };
        let mark = lanes.record.mark_reset(target);
        if let Some(ordinal) = gated.filter(|ordinal| *ordinal <= mark.cutoff) {
            // Dropped at the window's opening: another lane may have fired it. An ordinal after the cutoff is not the
            // epoch's any more, and the state owner's record never hears of it.
            if several_points {
                state.counts.divergence += 1;
            }
            lanes.record.share().skipped(instance.epoch, ordinal, lane);
        }
        let watermark = state.tracker.watermark_in(instance.epoch);
        while lanes.ordinal <= mark.cutoff {
            let ordinal = lanes.ordinal;
            let Some(at) = instance.tick.at() else { break };
            let second = lanes.record.take_second(ordinal, at, || wall.as_unix_secs());
            let decision = state
                .gate
                .due_at_second(bound, watermark, second.second, instance.last_flush);
            let fire = match decision {
                TickDecision::Fire { flush } => Some(flush),
                TickDecision::Skip | TickDecision::Gate { .. } => None,
            };
            let interval = if fire.is_some() { interval } else { instance.interval };
            let resolved = lanes.record.resolve(ordinal, at, interval, fire.is_some());
            lanes.ordinal = ordinal + 1;
            instance.tick = TickState::Scheduled { at: resolved.next_at };
            match fire.filter(|_| resolved.fired) {
                Some(flush) => {
                    check_flush_agreed(lanes, ordinal, flush, instance.last_flush);
                    fired.push((
                        handle,
                        DrainKind::Tick {
                            scheduled: at,
                            flush,
                            ordinal: Some(ordinal),
                            next: Some(resolved.next_at),
                        },
                    ));
                }
                None => {
                    // Dropped, gated at the old watermark: another lane may have fired it. A skip, for want of a
                    // binding or a watermark, is one every lane makes.
                    if several_points && matches!(decision, TickDecision::Gate { .. }) {
                        state.counts.divergence += 1;
                    }
                    lanes.record.share().skipped(instance.epoch, ordinal, lane);
                }
            }
        }
    }
    queue.extend(in_drain_order(generations, state, fired));
}

/// Queues the reset drain of every reset-retiring instance of each closed epoch, oldest epoch first, at the epoch's
/// watermark, and returns how many it queued. The last drain of each epoch ends an output epoch, and an epoch with no
/// instance to drain ends one at once, or with the drain before it.
fn queue_resets(
    generations: &[LiveGeneration], state: &mut StateSlot, closed: &[(u32, Option<AdmissionTime>)],
    admitted_at: Option<AdmissionTime>, queue: &mut VecDeque<(InstanceHandle, DrainKind)>,
) -> usize {
    let mut resets = 0;
    for &(epoch, pre) in closed {
        // Without a watermark, as when a point never stamped in the epoch, at the epoch's latest admission time.
        let stamp_at = pre
            .or_else(|| state.tracker.max_latest_in(epoch))
            .or(state.output.latest())
            .or(admitted_at)
            .unwrap_or(AdmissionTime::UNIX_EPOCH);
        let drains = state
            .instances
            .iter()
            .filter(|(_, instance)| {
                instance.epoch == epoch && instance.reset_retiring && !instance.retired && !instance.removed
            })
            .map(|(handle, _)| {
                // Whether `pre` is in the publication epoch of the cut retiring the instance, if any.
                let in_publication_epoch = state
                    .retiring_of(handle)
                    .and_then(|retiring| retiring.by.cut())
                    .is_some_and(|cut| state.tracker.is_publication_epoch(cut, epoch));
                (
                    handle,
                    DrainKind::Reset {
                        pre,
                        stamp_at,
                        in_publication_epoch,
                        ends_epochs: 0,
                    },
                )
            })
            .collect::<Vec<_>>();
        resets += drains.len();
        queue.extend(in_drain_order(generations, state, drains));
        // The output's epoch ends with the epoch's last drain; an epoch with no instance to drain ends it with the
        // drain before it, of an older epoch, or at once.
        let last = queue.iter_mut().rev().find_map(|(_, kind)| match kind {
            DrainKind::Reset { ends_epochs, .. } => Some(ends_epochs),
            _ => None,
        });
        match last {
            Some(ends_epochs) => *ends_epochs += 1,
            None => state.output.reset(),
        }
    }
    resets
}

/// Returns the shutdown drains of every instance, in drain order, each closing at its record's shared close time, or,
/// without lanes, at the later of the latest admission time of its epoch and the wall clock.
fn shutdown_queue(generations: &[LiveGeneration], state: &mut StateSlot) -> VecDeque<(InstanceHandle, DrainKind)> {
    let wall = state.wall.now();
    let drains = state
        .instances
        .iter()
        .filter(|(_, instance)| !instance.retired && !instance.removed)
        .map(|(handle, instance)| {
            // Every upstream point's data has arrived: close at the later of the latest admission time and the wall
            // clock, of the instance's epoch only.
            let latest = state.tracker.max_latest_in(instance.epoch);
            let at = match &instance.lanes {
                Some(lanes) => {
                    let shared = lanes.record.shutdown_at(|| shutdown_time(latest, wall));
                    latest.map_or(shared, |latest| shared.max(latest))
                }
                None => shutdown_time(latest, wall),
            };
            (handle, DrainKind::Shutdown { at })
        })
        .collect();
    in_drain_order(generations, state, drains).collect()
}

/// Skips the tick due at `at` of an instance: it moves on by its interval, or with lanes to the next instant of its
/// lineage's record, which it resolves the ordinal in.
fn skip_tick(state: &mut StateSlot, handle: InstanceHandle, at: Instant) {
    let lane = state.lane.index;
    let window_open = state.window.is_open();
    let Some(instance) = state.instances.get_mut(handle) else {
        return;
    };
    let Some(lanes) = instance.lanes.as_mut() else {
        instance.tick = TickState::Scheduled {
            at: at + instance.interval,
        };
        return;
    };
    let ordinal = lanes.ordinal;
    let resolved = lanes.record.resolve(ordinal, at, instance.interval, false);
    lanes.ordinal = ordinal + 1;
    instance.tick = TickState::Scheduled { at: resolved.next_at };
    // An ordinal after a reset mark's cutoff is not the epoch's any more: the state owner's record never hears of it.
    if lanes.record.mark().is_none_or(|mark| ordinal <= mark.cutoff) {
        lanes.record.share().skipped(instance.epoch, ordinal, lane);
    }
    // A tick skipped while a window is open, by an instance holding contexts, is one the other lanes may fire.
    if window_open && contexts_held(&mut instance.kernel) > 0 {
        state.counts.divergence += 1;
    }
}

/// Resolves the tick due at `at` of an instance as a fire at `flush`, under the stamping generation's `interval`:
/// returns the drain to queue, or `None` when the lanes' reset mark forbids it, which skips it. `at_second` is whether
/// `flush` is the tick's second, which every lane fires it at, rather than the lane's own watermark at a grace expiry.
fn resolve_fire(
    state: &mut StateSlot, handle: InstanceHandle, at: Instant, flush: AdmissionTime, interval: Duration,
    at_second: bool,
) -> Option<DrainKind> {
    let instance = state.instances.get_mut(handle)?;
    let Some(lanes) = instance.lanes.as_mut() else {
        return Some(DrainKind::Tick {
            scheduled: at,
            flush,
            ordinal: None,
            next: None,
        });
    };
    let ordinal = lanes.ordinal;
    let resolved = lanes.record.resolve(ordinal, at, interval, true);
    if resolved.fired {
        if at_second {
            check_flush_agreed(lanes, ordinal, flush, instance.last_flush);
        }
        return Some(DrainKind::Tick {
            scheduled: at,
            flush,
            ordinal: Some(ordinal),
            next: Some(resolved.next_at),
        });
    }
    // Past the reset mark's cutoff: the lane's window is not open yet, and the tick is skipped.
    lanes.ordinal = ordinal + 1;
    instance.tick = TickState::Scheduled { at: resolved.next_at };
    None
}

/// Checks, in debug builds, that a lane fires an ordinal at its second at the flush time the first lane to fire it so
/// did: the lanes flush one ordinal at one time, except at a grace expiry, which flushes at the lane's own watermark, a
/// straggler, which reads its own second, and a flush the lane's own previous one held back, as after the wall clock
/// stepped back.
fn check_flush_agreed(lanes: &InstanceLanes, ordinal: u64, flush: AdmissionTime, last_flush: Option<AdmissionTime>) {
    debug_assert!(
        last_flush == Some(flush)
            || lanes
                .record
                .agree_flush(ordinal, flush)
                .is_none_or(|agreed| agreed == flush),
        "the lanes fired ordinal {} at different flush times",
        ordinal
    );
}

/// Moves every binding of `from` to `to`, its lineage's instance of a newer epoch, with its users and its cut.
fn rebind(generations: &mut [LiveGeneration], state: &mut StateSlot, from: InstanceHandle, to: InstanceHandle) {
    for live in generations.iter_mut() {
        for binding in &mut live.kernels {
            if *binding == KernelBinding::Instance(from) {
                *binding = KernelBinding::Instance(to);
            }
        }
    }
    let Some(old) = state.instances.get_mut(from) else {
        return;
    };
    let (users, interval, mut retiring) = (std::mem::take(&mut old.users), old.interval, old.retiring.take());
    for (handle, instance) in state.instances.iter_mut() {
        if handle == to {
            instance.primary = None;
            instance.users += users;
            instance.interval = interval;
            if !instance.retired {
                instance.retiring = retiring.take();
            }
        } else if instance.primary == Some(from) {
            instance.primary = Some(to);
        }
    }
}

/// Returns the contexts a context limit holds twice at this lane: in an instance retiring at a cut and in its node's
/// instance created by the cut's generation, between the first and the last point's switch, or inside a reset window.
///
/// Only a slot on several lanes counts them: with one point the two lineages never hold contexts at once. A pair is
/// looked at only while both instances hold contexts, from the new one's first batch to the crossing: the new
/// instance's contexts are listed, and each is looked up once in the retiring instance.
fn crossing_overlap(state: &mut StateSlot) -> u64 {
    let pairs = state
        .instances
        .iter()
        .filter(|(_, instance)| !instance.retired && !instance.removed && instance.primary.is_none())
        .filter_map(|(handle, instance)| {
            let by = instance.retiring.as_ref()?.by.id();
            let new = state.instances.iter().find(|(_, other)| {
                other.key.node == instance.key.node
                    && other.created_by == by
                    && !other.retired
                    && !other.removed
                    && other.primary.is_none()
            })?;
            Some((handle, new.0))
        })
        .collect::<Vec<_>>();
    let mut overlap = 0;
    for (retiring, new) in pairs {
        let held = |state: &mut StateSlot, handle| {
            state
                .instances
                .get_mut(handle)
                .map_or(0, |instance| contexts_held(&mut instance.kernel))
        };
        if held(state, new) == 0 || held(state, retiring) == 0 {
            continue;
        }
        let mut snapshot = ContextSnapshot::default();
        if let Some(instance) = state.instances.get_mut(new) {
            ask(&mut instance.kernel, &mut snapshot);
        }
        let mut count = CountHeld {
            entries: snapshot.entries,
            held: 0,
        };
        if let Some(instance) = state.instances.get_mut(retiring) {
            ask(&mut instance.kernel, &mut count);
        }
        overlap += count.held;
    }
    overlap
}

/// Wraps an instance's kernel in what `wrap` returns for it.
#[cfg(any(test, feature = "test-util"))]
fn wrap_kernel(wrap: &StateKernelWrap, slot: &mut KernelSlot) {
    /// What an instance holds for the moment its kernel is being wrapped.
    struct Unwrapped;

    impl saluki_core::execution::Kernel for Unwrapped {
        fn consume(&mut self, _: &mut Turn<'_>, _: &mut EventsBuffer) -> Progress {
            Progress::Done
        }
    }

    let kernel = std::mem::replace(&mut slot.kernel, Box::new(Unwrapped));
    slot.kernel = wrap(kernel);
}

/// Asks an instance's kernel a read-only control message, in the kernel's resource group: answered at once, it leaves
/// a call of the kernel that yielded in place, and writes nothing.
fn ask(slot: &mut KernelSlot, message: &mut dyn Any) {
    ask_with(slot, message, None);
}

/// Asks an instance's kernel a read-only control message as [`ask`] does, with per-call parameters, if given.
fn ask_with(slot: &mut KernelSlot, message: &mut dyn Any, param: Option<&(dyn Any + Send + Sync)>) {
    let header = BatchHeader::default();
    let (mut budget, mut counters) = (Budget::unlimited(), TurnCounters::default());
    slot.context.accounted(|| {
        let mut turn = Turn::new(&header, Instant::now(), &mut budget, &mut slot.ports, &mut counters);
        if let Some(param) = param {
            turn = turn.with_param(param);
        }
        slot.kernel.control(&mut turn, message)
    });
}

/// Returns how many contexts an instance's kernel holds: in constant time for a kernel that answers [`ContextCount`],
/// and otherwise the contexts it holds points of.
fn contexts_held(slot: &mut KernelSlot) -> u64 {
    let mut count = ContextCount::default();
    ask(slot, &mut count);
    count
        .contexts
        .map_or_else(|| slot.kernel.held().events, |contexts| contexts as u64)
}

/// Takes, from an instance's kernel on lanes, the flushes that started their idle-counter zeros from its own last
/// flush because the state owner's record could not say where the lanes left them ([`TakeLaneFallbacks`]).
fn take_fallbacks(slot: &mut KernelSlot) -> u64 {
    let mut take = TakeLaneFallbacks::default();
    ask(slot, &mut take);
    take.flushes
}

/// Returns what a completed drain step did with the lane's time, for a harness: a tick that took effect, a reset drain,
/// or a crossing.
#[cfg(any(test, feature = "test-util"))]
fn time_event(instance: &Instance, step: &DrainStep) -> Option<LaneTimeEvent> {
    let (created_by, epoch) = (instance.created_by, instance.epoch);
    match step.kind {
        DrainKind::Tick {
            scheduled,
            flush,
            ordinal,
            ..
        } => Some(LaneTimeEvent::Ticked {
            created_by,
            epoch,
            ordinal,
            scheduled,
            flush: step.flush.unwrap_or(flush),
        }),
        DrainKind::Reset { .. } => Some(LaneTimeEvent::ResetDrained {
            created_by,
            epoch,
            pre: step.flush,
        }),
        DrainKind::FinalRetire => step.flush.map(|at| LaneTimeEvent::Crossed { created_by, epoch, at }),
        DrainKind::Shutdown { .. } | DrainKind::Boundary { .. } => None,
    }
}

/// Completes the drain of one instance: a tick moves its deadline on by the interval of the generation it ran under,
/// keeping its phase, or to the next instant of its lineage's record; a reset forgets its flushes, and marks a
/// retiring instance whose drain clamped to its cut as retiring at the window's close; a final retirement retires it.
fn complete_instance(instance: &mut Instance, step: &DrainStep, retiring: bool) {
    instance.interval = step.interval;
    match step.kind {
        DrainKind::Tick {
            scheduled,
            ordinal,
            next,
            ..
        } => {
            if !instance.retired {
                instance.tick = TickState::Scheduled {
                    at: next.unwrap_or(scheduled + step.interval),
                };
            }
            if let (Some(lanes), Some(ordinal)) = (instance.lanes.as_mut(), ordinal) {
                lanes.ordinal = lanes.ordinal.max(ordinal + 1);
            }
            instance.last_flush = step.flush;
        }
        DrainKind::Reset {
            in_publication_epoch, ..
        } => {
            instance.last_flush = None;
            // A reset drain empties the lineage, so it owes no boundary flush any more.
            instance.boundary = None;
            // Its reset drain flushed at its cut's time at the latest: nothing after it belongs to the lineage it
            // served, so the drain was its final retirement.
            instance.retires_at_close = retiring && in_publication_epoch;
        }
        DrainKind::FinalRetire => {
            instance.retired = true;
            instance.retiring = None;
            instance.tick = TickState::Off;
        }
        DrainKind::Boundary { at } => {
            instance.boundary = None;
            instance.last_flush = Some(instance.last_flush.map_or(at, |last| last.max(at)));
            // A tick gated on a second the boundary flush covered has nothing left to close.
            if let TickState::Gated { at: due, second, .. } = instance.tick {
                if second <= at.as_unix_secs() {
                    instance.tick = TickState::Scheduled {
                        at: due + instance.interval,
                    };
                }
            }
        }
        DrainKind::Shutdown { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flush_holds_admission_from_its_start_except_while_a_pass_lets_batches_in() {
        let ms = |millis: u64| Duration::from_millis(millis);
        let started = Instant::now();
        // A flush that never interleaves holds admission throughout.
        let mut timing = FlushTiming::start(started);
        assert_eq!(timing.finish(started + ms(30)), (ms(30), ms(30)));
        // A pass lets batches in from 2 ms, falls behind at 20 ms, and completes at 30 ms; holding again is idempotent.
        let mut timing = FlushTiming::start(started);
        timing.release(started + ms(2));
        timing.release(started + ms(5));
        timing.hold(started + ms(20));
        timing.hold(started + ms(25));
        assert_eq!(timing.finish(started + ms(30)), (ms(30), ms(12)));
    }

    #[test]
    fn a_pass_is_due_when_either_pace_reaches_what_it_did_and_behind_a_step_period_later() {
        let ms = |millis: u64| Duration::from_millis(millis);
        let started = Instant::now();
        // A flush interval of 15 s spreads a pass over 5 s.
        let mut pass = Pass::start(started, Duration::from_secs(15));
        // Before its kernel says how many contexts it visits, the step pace alone: one step done, the next due at
        // 100 ms, and behind from 200 ms.
        pass.steps = 1;
        assert_eq!(pass.due(), started + ms(100));
        assert!(!pass.behind(started + ms(199)));
        assert!(pass.behind(started + ms(200)));
        // 100,000 contexts, 4,096 of them visited: the step pace is still the faster.
        pass.progress(PassProgress {
            visited: 4_096,
            total: 100_000,
        });
        assert_eq!(pass.due(), started + ms(100));
        // Contexts visited on touch move the contexts pace past the step pace, which then decides alone.
        pass.steps = 30;
        assert_eq!(
            pass.progress(PassProgress {
                visited: 10_000,
                total: 100_000
            }),
            5_904
        );
        assert_eq!(pass.due(), started + ms(500));
        assert!(pass.behind(started + ms(600)));
    }
}

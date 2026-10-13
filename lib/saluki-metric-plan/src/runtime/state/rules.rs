//! The time rules of state owners, shared by the region driver of an isolation slot and the reference interpreter.
//!
//! Everything here is pure: no clock is read, nothing is awaited, and nothing is shared. Both hosts of a state owner,
//! the region that runs it in production and the interpreter that is its oracle, decide with these functions, so that
//! the two agree by construction on what a state owner's watermark is, when a flush tick fires and at what flush time,
//! when a clock reset happens, when a cut takes effect, and which generation time-driven output belongs to.
//!
//! # Time domains
//!
//! A state owner reads one time: the admission time `t_a` of what it receives. A flush runs at a **flush time** `F`,
//! which a host passes as the kernel's turn watermark and which never exceeds the state owner's watermark `W`, so that
//! no bucket closes before everything stamped within it has arrived. Ticks keep their native cadence on a monotonic
//! timer, which decides *when* a flush runs; `F` decides *what* it closes.
//!
//! - [`WatermarkTracker`] keeps the progress of every admission point upstream of a state owner, from the headers that
//!   reach it, and derives `W`, clock resets, and whether every upstream point has switched to a generation.
//! - [`TickGate`] decides each flush tick: fire at the tick's wall-clock second once `W` has reached it, or wait for
//!   it at most a grace, then fire at `W`.
//! - [`lcm_secs`] and [`cut_time`] give the time a cut between bucket widths takes effect at;
//!   [`retiring_flush_time`] keeps a retiring lineage's flushes before it.
//! - [`stamping_generation`] decides the generation of a drain's time-driven output, and [`OutputClock`] stamps it.
//! - [`shutdown_time`] is the time a state owner closes at when its input ends.

use std::{num::NonZeroU64, time::Duration};

use saluki_core::execution::{
    AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, CutBoundary, GenerationId, GenerationRef,
};
use snafu::Snafu;

use crate::limits::values::{MAX_OPEN_EPOCHS, MAX_TRANSITION_LCM, TICK_GRACE};

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// What a state owner knows about one admission point upstream of it, from the headers of that point that reached it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointProgress {
    /// The epoch of the point's clock in the latest header: it changes only when the clock resets.
    pub epoch: u32,

    /// The latest admission time the point stamped in the current epoch.
    pub latest: AdmissionTime,

    /// The newest generation any header of the point carried, in any epoch.
    pub max_generation: Option<GenerationId>,
}

/// What observing one header told a [`WatermarkTracker`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    /// The header advanced, or kept, its point's progress.
    Ok,

    /// The header's point reset its clock: its header is in a newer epoch than its previous one. Before handling the
    /// header's batch, the state owner closes everything it holds at `pre`, in the old timeline.
    ///
    /// The observation carries what the tracker knew of the point before it switched, so that a host can evaluate a
    /// rule as it stood before the header without keeping a copy of the tracker, such as
    /// [`in_publication_epoch_with`][WatermarkTracker::in_publication_epoch_with].
    EpochSwitched {
        /// The point that reset.
        point: AdmissionPointId,

        /// The epoch the point was in before this header.
        previous: u32,

        /// The latest admission time the point stamped in its previous epoch, at which that epoch closed.
        final_latest: AdmissionTime,

        /// The watermark before the reset, or `None` if another upstream point had sent nothing yet.
        pre: Option<AdmissionTime>,
    },

    /// The header is unstamped, stamped by a point that is not upstream of the state owner, or stamped in an epoch
    /// older than one its point already sent: a wiring fault. The tracker ignored it; its batch is invalid.
    Fault,
}

/// The progress of every admission point upstream of one state owner, and the watermark it makes.
///
/// A tracker observes every header that reaches its state owner, data or progress, in arrival order. Batches from one
/// admission point arrive in the order it stamped them, so within one epoch of its clock, each point's admission times
/// only grow:
///
/// 1. an unstamped header, or one stamped by a point outside the upstream set, is a [fault][Observation::Fault];
/// 2. the first header of a point records its progress;
/// 3. a header of a [newer][epoch_newer] epoch than the point's last [switches its epoch][Observation::EpochSwitched]:
///    a reset of the point, whose progress then restarts from the header;
/// 4. a header of an older epoch than the point's last is a fault too, which one path in stamping order never
///    delivers: debug builds assert it, and the tracker ignores it;
/// 5. otherwise the point's latest admission time grows to the header's;
/// 6. in every case but a fault, the point's newest generation grows to the header's.
///
/// The **watermark** is the earliest latest admission time over the upstream points, and unknown while any of them has
/// sent nothing: everything stamped before it has arrived, so a bucket ending by it can close.
///
/// When a point switches epochs, the tracker keeps the final latest time it recorded in the epoch it left, for the
/// last [`MAX_OPEN_EPOCHS`] epochs it left, so that the instances of an epoch some points have left and others not yet
/// have a watermark of their own ([`watermark_in`][Self::watermark_in]): what a [reset window][ResetWindow] drains
/// them at.
///
/// A point is found by its ID in constant time when the upstream points' IDs follow one another, as the lane points of
/// one source do, and by binary search otherwise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatermarkTracker {
    points: Vec<(AdmissionPointId, Option<PointProgress>)>,
    /// The ID of the first point, when every point's ID follows the one before it: a point's index is then its ID
    /// minus this.
    first: Option<u32>,
    /// Per point, in the order of `points`, the final latest admission time it recorded in each earlier epoch it left,
    /// oldest first.
    finals: Vec<Vec<(u32, AdmissionTime)>>,
}

/// Where one point stands toward one epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InEpoch {
    /// The point's latest admission time in the epoch: its current one, or its final one there.
    At(AdmissionTime),

    /// The point went past the epoch without the tracker knowing a stamp of it there.
    Passed,

    /// The point has not reached the epoch: it sent nothing yet, or is in an older one.
    NotReached,
}

impl WatermarkTracker {
    /// Creates a tracker of the given upstream admission points, none of which has sent anything.
    ///
    /// A tracker of no point has no watermark, and has trivially switched to every generation.
    pub fn new<I>(upstream: I) -> Self
    where
        I: IntoIterator<Item = AdmissionPointId>,
    {
        let mut points = upstream.into_iter().map(|point| (point, None)).collect::<Vec<_>>();
        points.sort_by_key(|(point, _)| *point);
        points.dedup_by_key(|(point, _)| *point);
        let first = points.first().map(|(point, _)| point.get()).filter(|first| {
            points
                .iter()
                .enumerate()
                .all(|(index, (point, _))| u64::from(point.get()) == u64::from(*first) + index as u64)
        });
        let finals = vec![Vec::new(); points.len()];
        Self { points, first, finals }
    }

    /// Returns the index of an upstream point, if it is one.
    fn index_of(&self, point: AdmissionPointId) -> Option<usize> {
        match self.first {
            Some(first) => point
                .get()
                .checked_sub(first)
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < self.points.len()),
            None => self.points.binary_search_by_key(&point, |(upstream, _)| *upstream).ok(),
        }
    }

    /// Returns the upstream admission points, in ID order.
    pub fn upstream(&self) -> impl Iterator<Item = AdmissionPointId> + '_ {
        self.points.iter().map(|(point, _)| *point)
    }

    /// Returns what the tracker knows about an upstream point, if the point has sent anything.
    pub fn progress(&self, point: AdmissionPointId) -> Option<&PointProgress> {
        self.index_of(point).and_then(|index| self.points[index].1.as_ref())
    }

    /// Observes one header that reached the state owner, in arrival order.
    pub fn observe(&mut self, header: &BatchHeader) -> Observation {
        let (Some(admitted_at), Some(point)) = (header.admitted_at, header.admission_point) else {
            return Observation::Fault;
        };
        let Some(index) = self.index_of(point) else {
            return Observation::Fault;
        };
        let generation = header.generation_id();
        let known = match self.points[index].1 {
            None => {
                self.points[index].1 = Some(PointProgress {
                    epoch: header.epoch,
                    latest: admitted_at,
                    max_generation: generation,
                });
                return Observation::Ok;
            }
            Some(known) => known,
        };
        let observation = if known.epoch == header.epoch {
            debug_assert!(
                admitted_at >= known.latest,
                "admission point {} stamped {} after {} in one epoch, which one path in stamping order rules out",
                point.get(),
                admitted_at,
                known.latest
            );
            Observation::Ok
        } else if epoch_newer(header.epoch, known.epoch) {
            // A newer epoch is a reset; the point's timeline restarts from this header. The watermark before it is
            // what the state owner knew before the header.
            let pre = self.watermark();
            let finals = &mut self.finals[index];
            finals.push((known.epoch, known.latest));
            if finals.len() > MAX_OPEN_EPOCHS as usize {
                finals.remove(0);
            }
            Observation::EpochSwitched {
                point,
                previous: known.epoch,
                final_latest: known.latest,
                pre,
            }
        } else {
            debug_assert!(
                false,
                "admission point {} stamped epoch {} after epoch {}, which one path in stamping order rules out",
                point.get(),
                header.epoch,
                known.epoch
            );
            // Release builds drop the batch: its timeline is one this point already closed.
            return Observation::Fault;
        };
        let progress = self.points[index].1.as_mut().expect("the point has sent a header");
        if observation == Observation::Ok {
            progress.latest = progress.latest.max(admitted_at);
        } else {
            progress.epoch = header.epoch;
            progress.latest = admitted_at;
        }
        progress.max_generation = progress.max_generation.max(generation);
        observation
    }

    /// Returns the watermark: the earliest latest admission time over the upstream points, or `None` while any of them
    /// has sent nothing, or there is none.
    pub fn watermark(&self) -> Option<AdmissionTime> {
        if self.points.is_empty() {
            return None;
        }
        self.points
            .iter()
            .map(|(_, progress)| progress.map(|progress| progress.latest))
            .try_fold(None, |earliest: Option<AdmissionTime>, latest| {
                let latest = latest?;
                Some(Some(earliest.map_or(latest, |earliest| earliest.min(latest))))
            })
            .flatten()
    }

    /// Returns the latest admission time over the upstream points that have sent anything: the time a state owner
    /// closes at when its input ends, together with the wall clock ([`shutdown_time`]).
    pub fn max_latest(&self) -> Option<AdmissionTime> {
        self.points
            .iter()
            .filter_map(|(_, progress)| progress.map(|progress| progress.latest))
            .max()
    }

    /// Returns `true` if every upstream point has sent a header of `new` or of a later generation.
    ///
    /// Batches from one point arrive in stamping order, and once a cut to `new` is published, every generation a
    /// schedule holds afterwards is `new` or later. So once a point has sent a header of `new`, every batch it stamped
    /// for the generation `new` replaces has arrived.
    pub fn switched(&self, new: GenerationId) -> bool {
        self.points.iter().all(|(_, progress)| {
            progress
                .and_then(|progress| progress.max_generation)
                .is_some_and(|generation| generation >= new)
        })
    }

    /// Returns `true` if every upstream point the cut records is in the epoch it had when the cut was published, as
    /// the latest header from it says.
    ///
    /// Headers of an earlier epoch belong to the timeline before publication, whose admission times can exceed the
    /// cut's time without being past it, so a retiring lineage's flush times are clamped to the cut only in the
    /// publication epoch. A point the cut records that has sent nothing is not in it; a point it does not record does
    /// not count.
    pub fn in_publication_epoch(&self, cut: &CutBoundary) -> bool {
        self.points.iter().all(|(point, progress)| match cut.epoch_of(*point) {
            None => true,
            Some(epoch) => progress.is_some_and(|progress| progress.epoch == epoch),
        })
    }

    /// Returns `true` if `epoch` is the epoch every upstream point the cut records had when the cut was published: the
    /// rule [`in_publication_epoch`][Self::in_publication_epoch] applies to the instances holding data of `epoch`,
    /// whatever epoch the points are in now, as a reset window draining them needs.
    pub fn is_publication_epoch(&self, cut: &CutBoundary, epoch: u32) -> bool {
        self.points
            .iter()
            .all(|(point, _)| cut.epoch_of(*point).is_none_or(|published| published == epoch))
    }

    /// Returns where the point at `index` stands toward `epoch`.
    fn in_epoch(&self, index: usize, epoch: u32) -> InEpoch {
        let Some(progress) = self.points[index].1 else {
            return InEpoch::NotReached;
        };
        if progress.epoch == epoch {
            return InEpoch::At(progress.latest);
        }
        if let Some((_, latest)) = self.finals[index].iter().find(|(left, _)| *left == epoch) {
            return InEpoch::At(*latest);
        }
        if epoch_newer(progress.epoch, epoch) {
            InEpoch::Passed
        } else {
            InEpoch::NotReached
        }
    }

    /// Returns the watermark of the instances holding data of `epoch`: per point, its latest admission time in `epoch`,
    /// its current one or the final one it recorded when it left `epoch`, and `None` while some point has not reached
    /// `epoch` yet.
    ///
    /// A point that went past `epoch` without stamping in it, or whose final time there the tracker no longer keeps, is
    /// left out of the minimum; with no point left, there is no watermark. With one point, this is the watermark of its
    /// current epoch.
    pub fn watermark_in(&self, epoch: u32) -> Option<AdmissionTime> {
        let mut earliest = None;
        for index in 0..self.points.len() {
            match self.in_epoch(index, epoch) {
                InEpoch::At(latest) => {
                    earliest = Some(earliest.map_or(latest, |earliest: AdmissionTime| earliest.min(latest)))
                }
                InEpoch::Passed => {}
                InEpoch::NotReached => return None,
            }
        }
        earliest
    }

    /// Returns the latest admission time any upstream point stamped in `epoch`, as far as the tracker knows: the close
    /// time of the instances holding data of `epoch` at shutdown, with the wall clock.
    pub fn max_latest_in(&self, epoch: u32) -> Option<AdmissionTime> {
        (0..self.points.len())
            .filter_map(|index| match self.in_epoch(index, epoch) {
                InEpoch::At(latest) => Some(latest),
                InEpoch::Passed | InEpoch::NotReached => None,
            })
            .max()
    }

    /// Returns `true` if every upstream point's current epoch is `epoch` or newer: every point has switched to it, so a
    /// reset window into it closes.
    pub fn switched_epoch(&self, epoch: u32) -> bool {
        self.points.iter().all(|(_, progress)| {
            progress.is_some_and(|progress| progress.epoch == epoch || epoch_newer(progress.epoch, epoch))
        })
    }

    /// Returns what [`in_publication_epoch`][Self::in_publication_epoch] returned before the tracker observed a header
    /// of `point` that switched it from the epoch `previous`, as an [`Observation::EpochSwitched`] reports.
    ///
    /// Only the observed point's epoch changed, so the rule is evaluated with that point in `previous`, and every other
    /// point as it is.
    pub fn in_publication_epoch_with(&self, cut: &CutBoundary, point: AdmissionPointId, previous: u32) -> bool {
        self.points
            .iter()
            .all(|(upstream, progress)| match cut.epoch_of(*upstream) {
                None => true,
                Some(epoch) if *upstream == point => previous == epoch,
                Some(epoch) => progress.is_some_and(|progress| progress.epoch == epoch),
            })
    }
}

/// Returns `true` if the clock epoch `a` is newer than `b`.
///
/// An admission clock moves to the next epoch at each reset, and epochs wrap around, so `a` is newer than `b` when it
/// is less than half the epoch range ahead of it: `(a - b) mod 2^32` read as a signed number is positive.
pub fn epoch_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// The reset window of one state lane: the lane's epoch, and the clock epochs not yet closed at the lane, because some
/// upstream point has not switched past them.
///
/// With several upstream points of one source clock, a state lane receives each point's switch to a newer epoch at a
/// different time. The window opens at the first header of a newer epoch from any point, routes each batch to the
/// instances of its own epoch while it is open, and closes once every point switched to the newest epoch
/// ([`WatermarkTracker::switched_epoch`]), when each older epoch's instances drain at that epoch's watermark
/// ([`WatermarkTracker::watermark_in`]), oldest first. A later reset inside an open window moves its target, and
/// leaves the older epochs open. A header several epochs newer than the lane's newest holds every epoch in between
/// open as well: each is one reset of the clock, whose batches may still arrive from points that have not switched past
/// it. With one point the window opens and closes on one header. The driver and the interpreter share this rule.
///
/// A reset that would leave more than [`MAX_OPEN_EPOCHS`] older epochs open closes the oldest at once, at its
/// watermark so far: the step reports them closed while the window stays open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResetWindow {
    older: Vec<u32>,
    target: u32,
}

/// What one header did to a state lane's reset window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowStep {
    /// No window is open, and the header opens none.
    None,

    /// The header opened a window from `older` into `target`, or moved an open window's target to `target`.
    Opened {
        /// The epoch the lane was in: the newest it had reached before the header.
        older: u32,

        /// The epoch the header is in.
        target: u32,
    },

    /// A window is open: the header's batch goes to the instances of this side of it.
    Route(EpochSide),

    /// The header closed epochs of the window: each closed epoch, oldest first, with its watermark, which its instances
    /// drain at. The window is closed afterwards unless it closed only its oldest epochs, past the most it keeps open.
    Closed {
        /// The closed epochs, oldest first, each with its watermark.
        epochs: Vec<(u32, Option<AdmissionTime>)>,
    },
}

/// Which side of an open reset window a batch belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochSide {
    /// An older epoch, still open at the lane.
    Older(u32),

    /// The window's target epoch.
    Target,
}

impl ResetWindow {
    /// Creates the window of a lane in `epoch`, with none open: `0` for a lane whose upstream clock has not reset.
    pub const fn new(epoch: u32) -> Self {
        Self {
            older: Vec::new(),
            target: epoch,
        }
    }

    /// Steps the lane's window with one header, which `tracker` has observed, and says what the header did.
    ///
    /// Epochs follow one another, one per reset of the clock, so a header more than one epoch newer than the lane's
    /// newest holds every epoch in between open too: a point that skipped one, because the clock reset twice while it
    /// was blocked, says nothing of the others, whose batches of that epoch may still be on their way.
    pub fn step(&mut self, tracker: &WatermarkTracker, header: &BatchHeader) -> WindowStep {
        let epoch = header.epoch;
        let mut overflowed = Vec::new();
        let opened = epoch_newer(epoch, self.target).then(|| {
            let older = self.target;
            let resets = epoch.wrapping_sub(older);
            self.older.extend((0..resets).map(|offset| older.wrapping_add(offset)));
            self.target = epoch;
            let excess = self.older.len().saturating_sub(MAX_OPEN_EPOCHS as usize);
            overflowed.extend(self.older.drain(..excess));
            older
        });
        if !self.is_open() {
            return WindowStep::None;
        }
        if tracker.switched_epoch(self.target) {
            let epochs = overflowed
                .into_iter()
                .chain(std::mem::take(&mut self.older))
                .map(|older| (older, tracker.watermark_in(older)))
                .collect();
            return WindowStep::Closed { epochs };
        }
        if !overflowed.is_empty() {
            return WindowStep::Closed {
                epochs: overflowed
                    .into_iter()
                    .map(|older| (older, tracker.watermark_in(older)))
                    .collect(),
            };
        }
        match opened {
            Some(older) => WindowStep::Opened {
                older,
                target: self.target,
            },
            None if epoch == self.target => WindowStep::Route(EpochSide::Target),
            None => WindowStep::Route(EpochSide::Older(epoch)),
        }
    }

    /// Returns `true` if a window is open: some older epoch is not closed at the lane yet.
    pub fn is_open(&self) -> bool {
        !self.older.is_empty()
    }

    /// Returns `true` if the lane holds `epoch` open: it is the lane's epoch, the window's target, or an older epoch
    /// the window has not closed.
    pub fn holds(&self, epoch: u32) -> bool {
        epoch == self.target || self.older.contains(&epoch)
    }

    /// Returns the epochs not yet closed at the lane, oldest first: none while no window is open.
    pub fn older(&self) -> &[u32] {
        &self.older
    }

    /// Returns the newest epoch a reset leads to: the lane's epoch, which instances bound now hold data of.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Closes every older epoch at once, as a lane whose input ended does: returns each, oldest first, with its
    /// watermark so far, which is exact once every point's last header reached the lane.
    pub fn close(&mut self, tracker: &WatermarkTracker) -> Vec<(u32, Option<AdmissionTime>)> {
        std::mem::take(&mut self.older)
            .into_iter()
            .map(|older| (older, tracker.watermark_in(older)))
            .collect()
    }
}

impl Default for ResetWindow {
    /// Returns the window of a lane in epoch 0, the first epoch of every admission clock.
    fn default() -> Self {
        Self::new(0)
    }
}

/// What the [tick gate][TickGate] decides at a flush tick's deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickDecision {
    /// No flush: no published generation binds the lineage, or its watermark is unknown. The next deadline is the
    /// skipped one plus the lineage's flush interval, never a deadline already passed.
    Skip,

    /// Flush now, at this flush time.
    Fire {
        /// The flush time: the kernel's turn watermark.
        flush: AdmissionTime,
    },

    /// Wait until the watermark reaches the tick's wall-clock second, or the grace ends.
    Gate {
        /// The tick's wall-clock second, in whole seconds since the Unix epoch.
        second: u64,
    },
}

/// The tick gate: when a flush tick fires, and at what flush time.
///
/// A state owner's watermark lags the wall clock at a tick, by queueing and batching, and while its sources are idle
/// by up to a heartbeat period. Flushing at the watermark would make a tick that falls within that lag of a bucket's
/// end close the bucket one tick late, and change the zeros idle counters gain. The gate waits instead: a tick fires at
/// its wall-clock second `s`, native aggregation's flush time, once the watermark has reached `s`, and otherwise waits
/// for it at most the grace, while input raises the watermark; once the grace ends it fires at the watermark. A flush
/// time never moves back from the lineage's previous one, so a wall clock that steps back closes nothing new.
///
/// | When | Decision |
/// |---|---|
/// | A deadline, and no published generation binds the lineage, or `W` is unknown | [skip][TickDecision::Skip] |
/// | A deadline, and `floor(W) >= s` | [fire][TickDecision::Fire] at `max(last, s)` |
/// | A deadline, otherwise | [gate][TickDecision::Gate] at `s`, until now plus the [grace][Self::grace] |
/// | Gated, and a header raises `floor(W)` to `s` | fire at `max(last, s)` before its batch ([`Self::on_watermark`]) |
/// | Gated, and the grace ends | fire at `max(last, floor(W))` ([`Self::on_grace`]) |
///
/// After a tick fires, the next deadline is the scheduled one plus the flush interval of the generation the flush ran
/// under, which keeps native phase, and bursts when it has already passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickGate {
    grace: Duration,
}

impl TickGate {
    /// Creates a gate that waits at most `grace` for a lagging watermark.
    pub const fn new(grace: Duration) -> Self {
        Self { grace }
    }

    /// Returns the longest a tick waits for its watermark.
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Decides a scheduled tick of a lineage.
    ///
    /// `bound` is whether a published generation binds the lineage, `watermark` the state owner's watermark, `wall`
    /// the wall clock at the tick, and `last` the flush time of the lineage's previous tick that fired, in this epoch.
    pub fn due(
        &self, bound: bool, watermark: Option<AdmissionTime>, wall: AdmissionTime, last: Option<AdmissionTime>,
    ) -> TickDecision {
        self.due_at_second(bound, watermark, wall.as_unix_secs(), last)
    }

    /// Decides a scheduled tick of a lineage whose tick's wall-clock second is `second`, as [`due`][Self::due] does:
    /// for a lane of a lineage on several lanes, the second the first lane to reach the tick read, which every lane
    /// shares.
    pub fn due_at_second(
        &self, bound: bool, watermark: Option<AdmissionTime>, second: u64, last: Option<AdmissionTime>,
    ) -> TickDecision {
        let Some(watermark) = watermark.filter(|_| bound) else {
            return TickDecision::Skip;
        };
        if watermark.as_unix_secs() >= second {
            TickDecision::Fire {
                flush: later(last, AdmissionTime::from_unix_secs(second)),
            }
        } else {
            TickDecision::Gate { second }
        }
    }

    /// Decides a gated tick when a header raises the watermark: returns the flush time to fire at, before the header's
    /// batch is admitted, if the watermark has reached the tick's wall-clock second.
    pub fn on_watermark(
        &self, second: u64, watermark: Option<AdmissionTime>, last: Option<AdmissionTime>,
    ) -> Option<AdmissionTime> {
        let watermark = watermark?;
        (watermark.as_unix_secs() >= second).then(|| later(last, AdmissionTime::from_unix_secs(second)))
    }

    /// Decides a gated tick whose grace ended: returns the flush time to fire at, the watermark's whole second, or
    /// `None` if the watermark is unknown.
    pub fn on_grace(&self, watermark: Option<AdmissionTime>, last: Option<AdmissionTime>) -> Option<AdmissionTime> {
        let watermark = watermark?;
        Some(later(last, AdmissionTime::from_unix_secs(watermark.as_unix_secs())))
    }
}

impl Default for TickGate {
    /// Returns the gate with the limits table's [`TICK_GRACE`].
    fn default() -> Self {
        Self::new(TICK_GRACE)
    }
}

/// Returns `flush`, or `last` if that is later: flush times never move back within an epoch.
fn later(last: Option<AdmissionTime>, flush: AdmissionTime) -> AdmissionTime {
    last.map_or(flush, |last| last.max(flush))
}

/// Why a cut between bucket widths cannot be made.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum CutError {
    /// No width was given.
    #[snafu(display("A cut needs the bucket widths it changes between, and none was given."))]
    NoWidths,

    /// A width is not a positive whole number of seconds.
    #[snafu(display(
        "The bucket width {:?} is not a positive whole number of seconds; bucket widths are allowed widths, such as 10 \
         seconds.",
        width
    ))]
    NotWholeSeconds {
        /// The width.
        width: Duration,
    },

    /// The widths line up less often than a cut may wait for.
    #[snafu(display(
        "Bucket widths {} line up only every {} seconds, more than the {} seconds a cut may wait for its boundary. \
         Change through an intermediate width of at most 60 seconds.",
        describe_widths(widths),
        lcm,
        MAX_TRANSITION_LCM.as_secs()
    ))]
    TooLong {
        /// The widths, as given.
        widths: Vec<Duration>,

        /// Their least common multiple, in seconds, saturating at `u64::MAX`.
        lcm: u64,
    },
}

fn describe_widths(widths: &[Duration]) -> String {
    widths
        .iter()
        .map(|width| format!("{} s", width.as_secs()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the least common multiple of bucket widths, in seconds: how often they share a boundary.
///
/// # Errors
///
/// Returns an error if no width is given, a width is not a positive whole number of seconds, or the least common
/// multiple exceeds [`MAX_TRANSITION_LCM`].
pub fn lcm_secs(widths: &[Duration]) -> Result<NonZeroU64, CutError> {
    if widths.is_empty() {
        return Err(CutError::NoWidths);
    }
    let mut lcm: u64 = 1;
    for &width in widths {
        if width.subsec_nanos() != 0 || width.is_zero() {
            return Err(CutError::NotWholeSeconds { width });
        }
        let secs = width.as_secs();
        lcm = (lcm / gcd(lcm, secs)).saturating_mul(secs);
    }
    if lcm > MAX_TRANSITION_LCM.as_secs() {
        return Err(CutError::TooLong {
            widths: widths.to_vec(),
            lcm,
        });
    }
    Ok(NonZeroU64::new(lcm).expect("the least common multiple of positive widths is positive"))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Returns the time a cut takes effect at: the first multiple of `lcm` seconds since the Unix epoch at least `margin`
/// after `now`, where `now` is the latest of the affected admission points' latest stamps and wall clocks.
///
/// Bucket widths divide `lcm`, so no bucket of any of them straddles the cut. Saturates at the largest admission time.
pub fn cut_time(now: AdmissionTime, lcm: NonZeroU64, margin: Duration) -> AdmissionTime {
    let earliest = u128::from(now.saturating_add(margin).as_unix_nanos());
    let step = u128::from(lcm.get()) * u128::from(NANOS_PER_SEC);
    let at = earliest.div_ceil(step) * step;
    AdmissionTime::from_unix_nanos(u64::try_from(at).unwrap_or(u64::MAX))
}

/// Returns the flush time of a retiring lineage: `flush`, clamped to the cut's time while the state owner's upstream
/// points are in the cut's publication epoch, so that the lineage never closes or fills a bucket at or after the cut.
///
/// Before the cut is armed (`cut` is `None`), or in an earlier epoch, whose admission times can exceed the cut's time
/// without being past it, the flush time is unchanged.
pub fn retiring_flush_time(
    flush: AdmissionTime, cut: Option<&CutBoundary>, in_publication_epoch: bool,
) -> AdmissionTime {
    match cut {
        Some(cut) if in_publication_epoch => flush.min(cut.at),
        _ => flush,
    }
}

/// The generations of a lineage that is retiring at a cut: the one it served, and the one replacing it.
#[derive(Clone, Copy, Debug)]
pub struct RetiringRefs<'a> {
    /// The generation the lineage served, which its output keeps until its final flush.
    pub owner: &'a GenerationRef,

    /// The generation replacing it, whose cut boundary, once armed, says when the lineage retires.
    pub by: &'a GenerationRef,
}

/// Returns the generation a drain of one lineage is stamped with, decided when the drain starts.
///
/// 1. A retiring lineage whose replacement's cut is armed drains under the generation it served.
/// 2. Otherwise the candidates come from the schedule: its generation when steady; during a cut, the new generation
///    first once every upstream point has switched to it (`switched`), and the old one first before. The first
///    candidate whose program binds the lineage (`binds`) is the drain's.
///
/// Returns `None` if no candidate binds the lineage: it was created by a generation prepared but not yet published,
/// so it holds nothing, and its tick is skipped.
pub fn stamping_generation<B, S>(
    schedule: &AdmissionSchedule, binds: B, switched: S, retiring: Option<RetiringRefs<'_>>,
) -> Option<GenerationRef>
where
    B: Fn(GenerationId) -> bool,
    S: Fn(GenerationId) -> bool,
{
    if let Some(retiring) = retiring.filter(|retiring| retiring.by.cut().is_some()) {
        return Some(retiring.owner.clone());
    }
    let candidates: [Option<&GenerationRef>; 2] = match schedule {
        AdmissionSchedule::Steady(generation) => [Some(generation), None],
        AdmissionSchedule::Cut { old, new, .. } if switched(new.id()) => [Some(new), Some(old)],
        AdmissionSchedule::Cut { old, new, .. } => [Some(old), Some(new)],
    };
    candidates
        .into_iter()
        .flatten()
        .find(|generation| binds(generation.id()))
        .cloned()
}

/// Returns the time a state owner closes at when its input ends: the later of the latest admission time it saw and
/// the wall clock.
///
/// Every upstream point's data has arrived when the input ends, so nothing stamped before this time can still come.
pub fn shutdown_time(max_latest: Option<AdmissionTime>, wall: AdmissionTime) -> AdmissionTime {
    max_latest.map_or(wall, |latest| latest.max(wall))
}

/// Stamps the time-driven output of an isolation slot: what its state owners emit at flush ticks, clock resets,
/// retirements, and shutdown.
///
/// The output is stamped as if the slot's output were an admission point of its own: its admission time is the
/// flush time, never less than the previous stamp of the same epoch, so that a state owner downstream sees admission
/// times that only grow within an epoch; its epoch counts the slot's clock resets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputClock {
    point: AdmissionPointId,
    epoch: u32,
    latest: Option<AdmissionTime>,
}

impl OutputClock {
    /// Creates the clock of a slot whose output point is `point`, in epoch 0, having stamped nothing.
    pub const fn new(point: AdmissionPointId) -> Self {
        Self {
            point,
            epoch: 0,
            latest: None,
        }
    }

    /// Returns the slot's output point.
    pub const fn point(&self) -> AdmissionPointId {
        self.point
    }

    /// Returns the current epoch: the number of clock resets the slot ran, wrapping.
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Returns the latest admission time stamped in the current epoch, if any.
    pub const fn latest(&self) -> Option<AdmissionTime> {
        self.latest
    }

    /// Returns the header of output flushed at `flush` under `generation`: admitted at the later of `flush` and the
    /// previous stamp of this epoch, with no entry, which the output's program sets.
    pub fn stamp(&mut self, flush: AdmissionTime, generation: GenerationRef) -> BatchHeader {
        let admitted_at = self.latest.map_or(flush, |latest| latest.max(flush));
        self.latest = Some(admitted_at);
        BatchHeader {
            admitted_at: Some(admitted_at),
            admission_point: Some(self.point),
            epoch: self.epoch,
            generation: Some(generation),
            entry: None,
        }
    }

    /// Starts a new epoch, after a clock reset's drain: the next stamp may be earlier than the last.
    pub fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.latest = None;
    }
}

#[cfg(test)]
mod tests;

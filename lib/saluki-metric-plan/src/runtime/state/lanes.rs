//! What the lanes of one slot share: the panic ledger of the generations they run and, for an isolation slot, one
//! record per logical lineage instance.
//!
//! A state owner partitioned over lanes holds a disjoint share of its contexts in each lane's instance. A few native
//! rules couple every context of one lineage instance: the instants of its ticks and each tick's flush time, the reset
//! that stops its old-epoch ticks, and its close time at shutdown. The lanes keep them in one [`LineageLanes`] record
//! per logical instance ([`LogicalInstance`]: the lineage, its creator, and the clock epoch), shared through the slot's
//! [`SlotLanes`], and the rules that are the state owner's own, such as where idle counters start from, in the kernel's
//! record, which the lineage's record carries as [`LaneShare`]. Records are touched a few times per tick, never per
//! event.
//!
//! # The tick record
//!
//! Ticks are identified by their **ordinal**, the `k`-th tick of the logical instance, counted alike in every lane, and
//! never by an instant, which a skipped tick or an interval change would move apart across lanes. The record keeps, for
//! the last [`TICK_RECORD_DEPTH`] ordinals, each one's instant, its flush second, and the instant of the next one, with
//! the newest ordinal any lane resolved:
//!
//! - **Instants.** Ordinal 1 is at the record's first instant; ordinal `k + 1` at the instant the first lane to resolve
//!   `k` set, `k`'s instant plus the interval its resolution moved its deadline by. Every lane takes its next deadline
//!   from the record, so a skip, an interval change, or a late lane never moves a lane off the record's instants.
//! - **The flush second.** The first lane at an ordinal's deadline reads the wall clock's second, and every later lane
//!   takes it ([`take_second`][LineageLanes::take_second]). A lane further behind than the record keeps is a
//!   straggler: it reads its own.
//! - **Resolution.** A lane resolves an ordinal in one critical section ([`resolve`][LineageLanes::resolve]): when it
//!   queues the tick's flush, or skips or drops it. Once a lane opened a reset window into a newer epoch, the record
//!   holds a [`ResetMark`]: no ordinal after its cutoff, the newest ordinal any lane had resolved, fires, so every lane
//!   fires the same old-epoch ordinals, and a flush already queued is always at or before the cutoff.
//! - **Joining.** A lane that binds an instance to the record starts at the ordinal after the newest any lane resolved,
//!   as if it had resolved every one before, and takes its instant, never a burst of past ones. Which lanes hold an
//!   instance in the record, and so can hold another lane's back-fill start back, the state owner's record keeps.
//!
//! A lane fires one tick in two critical sections at most: one to take its second, one to resolve it. Where its
//! idle-counter zeros start is the state owner's record's to say, which tells its kernel ([`LaneShare`]).
//!
//! A region without lanes keeps every rule itself; a region with one lane is the same.

#[cfg(not(all(test, feature = "loom")))]
use std::sync::{Mutex as RecordMutex, MutexGuard as RecordGuard};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, PoisonError, Weak,
    },
    time::{Duration, Instant},
};

// The record's lock is `loom`'s in the `loom` models, which run the record's own code.
#[cfg(all(test, feature = "loom"))]
use loom::sync::{Mutex as RecordMutex, MutexGuard as RecordGuard};
use saluki_core::execution::{AdmissionTime, GenerationId, LaneShare};

use super::rules::epoch_newer;
use crate::{compile::LineageKey, limits::values::TICK_RECORD_DEPTH, runtime::supervision::PanicLedger};

/// What the lanes of one slot share.
///
/// The host creates one per slot with lanes and gives it to every lane's region ([`RegionConfig::with_lane`]).
///
/// [`RegionConfig::with_lane`]: crate::runtime::RegionConfig::with_lane
pub struct SlotLanes {
    lanes: u16,
    ledger: Arc<PanicLedger>,
    lineages: Mutex<HashMap<LogicalInstance, Weak<LineageLanes>>>,
    /// The newest epoch whose clock reset the slot counted, so that a reset is counted once, by the first lane to
    /// reach it.
    resets_counted: AtomicU32,
    /// The watermark lag each lane last published, in seconds, as an `f64`'s bits, or [`LAG_UNKNOWN`] while the lane
    /// has no watermark.
    lags: Box<[AtomicU64]>,
    /// The oldest epoch each lane still holds open: every older epoch, the lane has continued its lineages past.
    reached: Box<[AtomicU32]>,
    /// The newest epoch each lane holds: its reset window's target, which is also `reached` while no window is open.
    newest: Box<[AtomicU32]>,
    /// The records their last lane left, with their epoch, kept while another lane may still reach it.
    kept: Mutex<Vec<(u32, Arc<LineageLanes>)>>,
}

/// The watermark lag of a lane that has no watermark: the bits of a NaN, which no known lag has.
const LAG_UNKNOWN: u64 = u64::MAX;

impl SlotLanes {
    /// Creates what the `lanes` lanes of one slot share.
    pub fn new(lanes: u16) -> Arc<Self> {
        Arc::new(Self {
            lanes,
            ledger: PanicLedger::new(),
            lineages: Mutex::new(HashMap::new()),
            resets_counted: AtomicU32::new(0),
            lags: (0..lanes).map(|_| AtomicU64::new(LAG_UNKNOWN)).collect(),
            reached: (0..lanes).map(|_| AtomicU32::new(0)).collect(),
            newest: (0..lanes).map(|_| AtomicU32::new(0)).collect(),
            kept: Mutex::new(Vec::new()),
        })
    }

    /// Returns the number of lanes of the slot.
    pub fn lanes(&self) -> u16 {
        self.lanes
    }

    /// Returns the panic ledger of the generations the slot's lanes run.
    pub(crate) fn panic_ledger(&self) -> &Arc<PanicLedger> {
        &self.ledger
    }

    /// Returns the record of a logical lineage instance, creating it with `make` if no lane holds it.
    ///
    /// Each lane whose instance is bound to the logical instance holds the record, and the record goes with the last of
    /// them, unless another lane may still reach its epoch when the last leaves it
    /// ([`keep_while_reachable`][Self::keep_while_reachable]).
    pub fn lineage<F>(&self, instance: &LogicalInstance, make: F) -> Arc<LineageLanes>
    where
        F: FnOnce() -> LineageLanes,
    {
        let mut lineages = self.lineages.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = lineages.get(instance).and_then(Weak::upgrade) {
            return record;
        }
        lineages.retain(|_, record| record.strong_count() > 0);
        let record = Arc::new(make());
        lineages.insert(instance.clone(), Arc::downgrade(&record));
        record
    }

    /// Records that a lane reached the clock reset into `epoch`, and returns how many resets it counts: one for each
    /// epoch up to `epoch` no lane reached yet, so that every reset is counted once per slot, by the first lane to
    /// reach its epoch, however many epochs a lane passes at once.
    pub fn count_reset(&self, epoch: u32) -> u32 {
        self.resets_counted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |counted| {
                epoch_newer(epoch, counted).then_some(epoch)
            })
            .map_or(0, |counted| epoch.wrapping_sub(counted))
    }

    /// Returns the record of a logical lineage instance if a lane holds it, or the slot keeps it, without creating one.
    pub fn existing(&self, instance: &LogicalInstance) -> Option<Arc<LineageLanes>> {
        let lineages = self.lineages.lock().unwrap_or_else(PoisonError::into_inner);
        lineages.get(instance).and_then(Weak::upgrade)
    }

    /// Keeps `record`, of a logical instance in `epoch`, which a lane leaves, for as long as another lane of the slot
    /// may still reach `epoch`: one that holds an older epoch open, has not reached `epoch` yet, or holds it open as
    /// the oldest epoch of its reset window. The record goes with its last holder otherwise.
    ///
    /// A lane that hears of a reset late reaches an epoch after the others continued past it or retired their
    /// lineages in it, and must tick the epoch on the record they shared, at its instants and flush seconds, and on
    /// the state owner's record of the epoch, never on records of its own numbered afresh.
    pub fn keep_while_reachable(&self, epoch: u32, record: Arc<LineageLanes>) {
        if !self.may_reach(epoch) {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if !kept.iter().any(|(_, kept)| Arc::ptr_eq(kept, &record)) {
            kept.push((epoch, record));
        }
    }

    /// Records that `lane`'s reset window opened, or moved, into `target`: the lane holds every epoch from the oldest
    /// it reached through `target` open.
    pub fn open(&self, lane: u16, target: u32) {
        if let Some(newest) = self.newest.get(usize::from(lane)) {
            let _ = newest.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                epoch_newer(target, current).then_some(target)
            });
        }
    }

    /// Records that `lane` holds no epoch older than `oldest` open any more, having continued every lineage past them,
    /// and lets go of the records of the epochs no lane may still reach.
    pub fn pass(&self, lane: u16, oldest: u32) {
        let index = usize::from(lane);
        for epochs in [&self.reached, &self.newest] {
            if let Some(epoch) = epochs.get(index) {
                // A lane's epochs only move on: an older value is a lane that has moved further already.
                let _ = epoch.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    epoch_newer(oldest, current).then_some(oldest)
                });
            }
        }
        let released = {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            let (reachable, released): (Vec<_>, Vec<_>) = std::mem::take(&mut *kept)
                .into_iter()
                .partition(|(epoch, _)| self.may_reach(*epoch));
            *kept = reachable;
            released
        };
        // Records dropped outside the lock, since a record's last drop tells its state owner's record.
        drop(released);
    }

    /// Returns `true` if a lane of the slot may still bind an instance to a record of `epoch` that no lane holds: a
    /// lane that holds an older epoch open, or has not reached `epoch` yet, or whose reset window holds `epoch` open as
    /// its oldest. A lane that holds `epoch` alone, its window closed, binds an instance of it only to a record it
    /// holds already, and one past it never comes back.
    fn may_reach(&self, epoch: u32) -> bool {
        self.reached.iter().zip(self.newest.iter()).any(|(reached, newest)| {
            let (reached, newest) = (reached.load(Ordering::Acquire), newest.load(Ordering::Acquire));
            epoch_newer(epoch, reached) || (reached == epoch && newest != epoch)
        })
    }

    /// Records the watermark lag `lane` publishes, in seconds, or `None` while it has no watermark, and returns the
    /// largest lag any lane last published, the slot's: the lag of its most lagging lane, `None` while no lane has a
    /// watermark.
    ///
    /// Every lane publishes about once a second, so the slot's lag is at most about a second older than its lanes'.
    pub fn publish_watermark_lag(&self, lane: u16, lag: Option<f64>) -> Option<f64> {
        if let Some(slot) = self.lags.get(usize::from(lane)) {
            slot.store(lag.map_or(LAG_UNKNOWN, f64::to_bits), Ordering::Relaxed);
        }
        self.lags
            .iter()
            .map(|slot| slot.load(Ordering::Relaxed))
            .filter(|&bits| bits != LAG_UNKNOWN)
            .map(f64::from_bits)
            .reduce(f64::max)
    }
}

impl fmt::Debug for SlotLanes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotLanes")
            .field("lanes", &self.lanes)
            .field("resets_counted", &self.resets_counted.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// One logical instance of a lineage, across lanes: the lineage, the generation that created it, and the epoch of the
/// upstream clock it holds data of.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LogicalInstance {
    /// The lineage.
    pub key: LineageKey,

    /// The generation that created the lineage's instances: the lineage's creator, never a batch's generation.
    pub created_by: GenerationId,

    /// The epoch of the source clock the instances hold data of.
    pub epoch: u32,
}

/// The record that every lane's instance of one logical lineage instance shares.
///
/// It holds the tick record (see the [module documentation][self]), the close time at shutdown, set by the first lane
/// to finish, and the state owner's own record of the rules that couple its contexts, which forgets the record's
/// epoch when the record goes ([`LaneShare::forget`]).
pub struct LineageLanes {
    epoch: u32,
    ticks: RecordMutex<TickRecord>,
    share: Arc<dyn LaneShare>,
}

/// The tick record of one logical instance.
#[derive(Default)]
struct TickRecord {
    /// The instant of ordinal 1.
    first_at: Option<Instant>,
    /// The last ordinals the record keeps, oldest first.
    recent: VecDeque<SharedTick>,
    /// The newest ordinal any lane resolved.
    newest: u64,
    /// The mark of the first lane to open a reset window, and the instant of the ordinal after its cutoff.
    reset: Option<(ResetMark, Option<Instant>)>,
    /// The time the instance closes at when its input ends.
    shutdown_at: Option<AdmissionTime>,
}

/// One ordinal of a tick record.
struct SharedTick {
    ordinal: u64,
    /// Its instant.
    at: Instant,
    /// Its flush second, in whole Unix seconds, once the first lane at its deadline read it.
    second: Option<u64>,
    /// The instant of the next ordinal, once the first lane resolved this one.
    next_at: Option<Instant>,
    /// Whether a lane whose grace expired at the ordinal counted it.
    grace_counted: bool,
    /// The flush time of the first lane to fire the ordinal at its second, which debug builds check every other lane
    /// fires it at ([`agree_flush`][LineageLanes::agree_flush]).
    flushed: Option<AdmissionTime>,
}

/// What a lane joining a record takes from it: where it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Joined {
    /// The newest ordinal any lane resolved, or the reset mark's cutoff if that is older: the lane joins as if it
    /// resolved every ordinal up to it, and tells the state owner's record so ([`LaneShare::join`]).
    pub through: u64,

    /// The ordinal the lane resolves next.
    pub ordinal: u64,

    /// The instant of that ordinal, if the record knows it yet: before the first lane started, it does not.
    pub at: Option<Instant>,
}

/// The flush second of one ordinal, as a lane at its deadline takes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickSecond {
    /// The second, in whole Unix seconds.
    pub second: u64,

    /// Whether the lane is a straggler: the ordinal is older than the record keeps, so it read its own second.
    pub straggler: bool,
}

/// What resolving one ordinal decided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolved {
    /// Whether the lane's flush takes effect: `false` for a skip, a drop, and for a flush the reset mark forbids.
    pub fired: bool,

    /// The instant of the next ordinal, the lane's next deadline.
    pub next_at: Instant,
}

impl LineageLanes {
    /// Creates the record of a logical instance in `epoch` whose state owner keeps its own rules in `share`.
    pub fn new(share: Arc<dyn LaneShare>, epoch: u32) -> Self {
        Self {
            epoch,
            ticks: RecordMutex::new(TickRecord::default()),
            share,
        }
    }

    fn lock(&self) -> RecordGuard<'_, TickRecord> {
        // Every update completes before the lock is released, and none can panic midway: a poisoned lock holds a
        // consistent record.
        self.ticks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the state owner's own record of the rules that couple the instance's contexts.
    pub fn share(&self) -> &Arc<dyn LaneShare> {
        &self.share
    }

    /// Returns the instant of the instance's first tick, setting it to what `first` returns if no lane has.
    pub fn first_at<F>(&self, first: F) -> Instant
    where
        F: FnOnce() -> Instant,
    {
        *self.lock().first_at.get_or_insert_with(first)
    }

    /// Returns the time the instance closes at when its input ends, setting it to what `close` returns if no lane has:
    /// the first lane to finish sets it.
    pub fn shutdown_at<F>(&self, close: F) -> AdmissionTime
    where
        F: FnOnce() -> AdmissionTime,
    {
        *self.lock().shutdown_at.get_or_insert_with(close)
    }

    /// Returns where a lane binding an instance to the record starts: after the newest ordinal any lane resolved, as if
    /// it had resolved every one before. The caller makes the lane a member of the state owner's record from there
    /// ([`LaneShare::join`]).
    ///
    /// Once a reset window marked the record, the state owner's record never hears of an ordinal after the mark's
    /// cutoff, so the lane joins it through the cutoff at the latest, and never holds the others' closing flushes back.
    pub fn join(&self) -> Joined {
        let record = self.lock();
        let ordinal = record.newest + 1;
        let through = record
            .reset
            .map_or(record.newest, |(mark, _)| record.newest.min(mark.cutoff));
        Joined {
            through,
            ordinal,
            at: record.at_of(ordinal),
        }
    }

    /// Returns the instant of `ordinal`, if the record knows it.
    pub fn at_of(&self, ordinal: u64) -> Option<Instant> {
        self.lock().at_of(ordinal)
    }

    /// Returns the record's current deadline: the instant of the newest ordinal no lane has resolved, or, once a reset
    /// window marked the record, of the ordinal after the mark's cutoff. A cut's new lineage and a reset's successor
    /// start their first tick there, as a stage 6 instance takes the phase of the one it replaces.
    pub fn current_deadline(&self) -> Option<Instant> {
        let record = self.lock();
        match record.reset {
            Some((_, resume_at)) => resume_at,
            None => record.at_of(record.newest + 1),
        }
    }

    /// Returns the flush second of `ordinal`, at the deadline `at` of a lane reaching it: the second the first lane at
    /// it read, or, for the first lane, what `now_second` reads, which every later lane takes.
    ///
    /// A lane at an ordinal older than the record keeps is a straggler, and reads its own second.
    pub fn take_second<F>(&self, ordinal: u64, at: Instant, now_second: F) -> TickSecond
    where
        F: FnOnce() -> u64,
    {
        let mut record = self.lock();
        match record.tick_mut(ordinal, at) {
            Some(tick) => TickSecond {
                second: *tick.second.get_or_insert_with(now_second),
                straggler: false,
            },
            None => TickSecond {
                second: now_second(),
                straggler: true,
            },
        }
    }

    /// Resolves `ordinal` for a lane whose deadline for it was `at`, in the critical section a flush takes effect in:
    /// with `fire`, the lane queues the tick's flush, and otherwise it skips or drops it. The first lane to resolve an
    /// ordinal sets the instant of the next one, `at` plus `interval`, the interval its resolution moves its deadline
    /// by; every lane returns with that instant.
    ///
    /// A flush of an ordinal after the reset mark's cutoff does not take effect: the lane resolves it as a skip.
    pub fn resolve(&self, ordinal: u64, at: Instant, interval: Duration, fire: bool) -> Resolved {
        let mut record = self.lock();
        let forbidden = record.reset.is_some_and(|(mark, _)| ordinal > mark.cutoff);
        record.newest = record.newest.max(ordinal);
        let next_at = match record.tick_mut(ordinal, at) {
            Some(tick) => *tick.next_at.get_or_insert(at + interval),
            None => at + interval,
        };
        Resolved {
            fired: fire && !forbidden,
            next_at,
        }
    }

    /// Records that `lane`'s grace expired at `ordinal`, and returns `true` for the first lane whose did, which counts
    /// the expiry, once per ordinal. A straggler never counts.
    pub fn count_grace(&self, ordinal: u64) -> bool {
        let mut record = self.lock();
        let Some(index) = record.index_of(ordinal) else {
            return false;
        };
        !std::mem::replace(&mut record.recent[index].grace_counted, true)
    }

    /// Marks the record at the opening of a reset window into `target`, if no lane has: no ordinal after the newest
    /// any lane resolved, the cutoff, fires from now on. A later reset moves the mark's target, never its cutoff.
    /// Returns the mark.
    pub fn mark_reset(&self, target: u32) -> ResetMark {
        let mut record = self.lock();
        let newest = record.newest;
        let resume_at = record.at_of(newest + 1);
        let (mark, _) = record
            .reset
            .get_or_insert((ResetMark { target, cutoff: newest }, resume_at));
        if epoch_newer(target, mark.target) {
            mark.target = target;
        }
        *mark
    }

    /// Returns the record's reset mark, if a lane opened a reset window.
    pub fn mark(&self) -> Option<ResetMark> {
        self.lock().reset.map(|(mark, _)| mark)
    }

    /// Records that a lane fired `ordinal` at `flush`, its second, and returns the flush time of the first lane to fire
    /// it so, which every lane's flush of one ordinal equals; or `None` once the record no longer keeps the ordinal, as
    /// for a straggler, which reads its own second.
    ///
    /// Only debug builds call it, to check that the lanes agree. A flush at a grace expiry, at the lane's own
    /// watermark, or one a lane's own previous flush time held back, is not a flush at the ordinal's second.
    pub fn agree_flush(&self, ordinal: u64, flush: AdmissionTime) -> Option<AdmissionTime> {
        let mut record = self.lock();
        let index = record.index_of(ordinal)?;
        Some(*record.recent[index].flushed.get_or_insert(flush))
    }
}

impl TickRecord {
    fn index_of(&self, ordinal: u64) -> Option<usize> {
        let front = self.recent.front()?.ordinal;
        let index = usize::try_from(ordinal.checked_sub(front)?).ok()?;
        self.recent
            .get(index)
            .filter(|tick| tick.ordinal == ordinal)
            .map(|_| index)
    }

    /// Returns the instant of `ordinal`, if the record knows it.
    fn at_of(&self, ordinal: u64) -> Option<Instant> {
        if ordinal <= 1 {
            return self.first_at;
        }
        if let Some(index) = self.index_of(ordinal) {
            return Some(self.recent[index].at);
        }
        self.index_of(ordinal - 1).and_then(|index| self.recent[index].next_at)
    }

    /// Returns the record's tick of `ordinal`, whose instant is `at`, adding it if it is newer than every kept one,
    /// and dropping the oldest past the record's depth; or `None` if it is older than the record keeps.
    fn tick_mut(&mut self, ordinal: u64, at: Instant) -> Option<&mut SharedTick> {
        if let Some(index) = self.index_of(ordinal) {
            return Some(&mut self.recent[index]);
        }
        if self.recent.back().is_some_and(|newest| newest.ordinal > ordinal) {
            // Older than the newest kept and not kept: older than the record keeps, or a gap no lane left.
            return None;
        }
        // Ordinals are kept contiguous, so that a position is an offset from the oldest: a lane reaches an ordinal only
        // after it resolved the one before, so a gap means every kept ordinal is older than any lane still needs.
        if self.recent.back().is_some_and(|newest| newest.ordinal + 1 != ordinal) {
            self.recent.clear();
        }
        self.recent.push_back(SharedTick {
            ordinal,
            at,
            second: None,
            next_at: None,
            grace_counted: false,
            flushed: None,
        });
        while self.recent.len() > TICK_RECORD_DEPTH as usize {
            self.recent.pop_front();
        }
        self.recent.back_mut()
    }
}

impl Drop for LineageLanes {
    fn drop(&mut self) {
        // No lane holds the record, and the slot no longer keeps it: no lane binds an instance to the epoch again.
        self.share.forget(self.epoch);
    }
}

impl fmt::Debug for LineageLanes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.lock();
        f.debug_struct("LineageLanes")
            .field("epoch", &self.epoch)
            .field("first_at", &record.first_at)
            .field("newest", &record.newest)
            .field("reset", &record.reset.map(|(mark, _)| mark))
            .field("shutdown_at", &record.shutdown_at)
            .finish_non_exhaustive()
    }
}

/// The mark the first lane to open a reset window sets on the old epoch's record: no ordinal after `cutoff` fires on an
/// instance that holds the old epoch's data.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResetMark {
    /// The newest epoch the reset leads to: a later reset moves it, never the cutoff.
    pub target: u32,

    /// The newest ordinal any lane had resolved when the mark was set.
    pub cutoff: u64,
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use saluki_core::execution::GenerationId;

    use super::*;
    use crate::{canonical::PlanHash, ir::NodeId};

    struct NoShare;

    impl LaneShare for NoShare {
        fn join(&self, _: u32, _: u16, _: u64) {}
        fn leave(&self, _: u32, _: u16) {}
        fn forget(&self, _: u32) {}
        fn skipped(&self, _: u32, _: u64, _: u16) {}
    }

    fn instance(epoch: u32) -> LogicalInstance {
        LogicalInstance {
            key: LineageKey {
                node: "agg".parse::<NodeId>().unwrap(),
                contract: PlanHash::of_bytes(b"lineage"),
            },
            created_by: GenerationId::new(1),
            epoch,
        }
    }

    fn record() -> LineageLanes {
        LineageLanes::new(Arc::new(NoShare), 0)
    }

    const INTERVAL: Duration = Duration::from_secs(15);

    #[test]
    fn lanes_share_one_record_per_logical_instance_while_any_holds_it() {
        let slot = SlotLanes::new(2);
        assert_eq!(slot.lanes(), 2);
        let first = slot.lineage(&instance(0), record);
        let again = slot.lineage(&instance(0), || panic!("the record exists"));
        assert!(Arc::ptr_eq(&first, &again));
        let other_epoch = slot.lineage(&instance(1), record);
        assert!(!Arc::ptr_eq(&first, &other_epoch));

        let start = Instant::now();
        assert_eq!(first.first_at(|| start), start);
        assert_eq!(
            again.first_at(|| start + Duration::from_secs(1)),
            start,
            "the first lane sets it"
        );
        let close = AdmissionTime::from_unix_secs(100);
        assert_eq!(first.shutdown_at(|| close), close);
        assert_eq!(again.shutdown_at(|| AdmissionTime::from_unix_secs(200)), close);

        drop((first, again));
        let fresh = slot.lineage(&instance(0), record);
        assert_eq!(
            fresh.first_at(|| start + Duration::from_secs(2)),
            start + Duration::from_secs(2)
        );
    }

    #[test]
    fn a_record_a_lane_left_for_a_newer_epoch_is_kept_until_every_lane_passed_its_epoch() {
        let slot = SlotLanes::new(2);
        let left = slot.lineage(&instance(1), record);
        let start = Instant::now();
        left.first_at(|| start);
        // Lane 1 continues past epoch 1, leaving the record; lane 0 has not reached it yet.
        slot.open(1, 1);
        slot.pass(1, 1);
        slot.open(1, 2);
        slot.keep_while_reachable(1, Arc::clone(&left));
        slot.pass(1, 2);
        let weak = Arc::downgrade(&left);
        drop(left);
        let again = slot.lineage(&instance(1), || panic!("the record is kept"));
        assert_eq!(
            again.first_at(|| start + INTERVAL),
            start,
            "lane 0 ticks on the record lane 1 left"
        );
        drop(again);
        // Once lane 0 passed epoch 1 too, the record goes with its last holder.
        slot.pass(0, 2);
        assert!(weak.upgrade().is_none());
        // A record of an epoch every lane has passed is not kept.
        let late = slot.lineage(&instance(1), record);
        let weak = Arc::downgrade(&late);
        slot.keep_while_reachable(1, late);
        assert!(weak.upgrade().is_none());
        // The slot's watermark lag is its most lagging lane's.
        assert_eq!(slot.publish_watermark_lag(0, Some(1.5)), Some(1.5));
        assert_eq!(slot.publish_watermark_lag(1, Some(4.0)), Some(4.0));
        assert_eq!(slot.publish_watermark_lag(1, None), Some(1.5));
        assert_eq!(slot.publish_watermark_lag(0, None), None);
    }

    #[test]
    fn a_record_its_last_lane_left_is_kept_only_while_another_lane_may_still_reach_its_epoch() {
        /// A share that counts the epochs it was told to forget.
        #[derive(Default)]
        struct Forgets(std::sync::Mutex<Vec<u32>>);
        impl LaneShare for Forgets {
            fn join(&self, _: u32, _: u16, _: u64) {}
            fn leave(&self, _: u32, _: u16) {}
            fn forget(&self, epoch: u32) {
                self.0.lock().unwrap().push(epoch);
            }
            fn skipped(&self, _: u32, _: u64, _: u16) {}
        }
        let share = Arc::new(Forgets::default());
        let slot = SlotLanes::new(3);

        // Every lane holds epoch 0 alone, its window closed: a record left there, as at a cut's crossing, goes at once,
        // and its state owner's record forgets the epoch.
        let left = Arc::new(LineageLanes::new(Arc::clone(&share) as Arc<dyn LaneShare>, 0));
        slot.keep_while_reachable(0, left);
        assert_eq!(*share.0.lock().unwrap(), vec![0]);

        // Lane 2 opens a window into epoch 1, holding epoch 0 open as its oldest: a record of epoch 0 is kept until the
        // window closes, and one of epoch 1 while lanes 0 and 1 have not reached it.
        slot.open(2, 1);
        let zero = Arc::new(LineageLanes::new(Arc::clone(&share) as Arc<dyn LaneShare>, 0));
        let one = Arc::new(LineageLanes::new(Arc::clone(&share) as Arc<dyn LaneShare>, 1));
        let (zero_weak, one_weak) = (Arc::downgrade(&zero), Arc::downgrade(&one));
        slot.keep_while_reachable(0, zero);
        slot.keep_while_reachable(1, one);
        assert!(zero_weak.upgrade().is_some() && one_weak.upgrade().is_some());
        slot.pass(2, 1);
        assert!(zero_weak.upgrade().is_none(), "no lane holds epoch 0 open any more");
        assert!(one_weak.upgrade().is_some(), "lanes 0 and 1 have not reached epoch 1");
        slot.pass(0, 1);
        slot.pass(1, 1);
        assert!(one_weak.upgrade().is_none(), "every lane holds epoch 1 alone");
        assert_eq!(*share.0.lock().unwrap(), vec![0, 0, 1]);
    }

    #[test]
    fn a_reset_is_counted_once_by_the_first_lane_to_reach_it() {
        let slot = SlotLanes::new(3);
        assert_eq!(
            slot.count_reset(0),
            0,
            "epoch 0 is the clock's first, reached by no reset"
        );
        assert_eq!(slot.count_reset(1), 1);
        assert_eq!(slot.count_reset(1), 0);
        assert_eq!(
            slot.count_reset(3),
            2,
            "a lane that passes two epochs at once counts both resets"
        );
        assert_eq!(
            slot.count_reset(2),
            0,
            "an epoch counted with a newer one is not counted again"
        );
    }

    #[test]
    fn every_lane_takes_each_ordinals_second_and_next_instant_from_the_first_lane() {
        let record = record();
        let start = Instant::now();
        let first = record.first_at(|| start + INTERVAL);
        assert_eq!(
            record.join(),
            Joined {
                through: 0,
                ordinal: 1,
                at: Some(first)
            }
        );

        // Lane 0 reaches ordinal 1 first and reads second 100; lane 1, a second later on the wall clock, takes 100.
        assert_eq!(
            record.take_second(1, first, || 100),
            TickSecond {
                second: 100,
                straggler: false
            }
        );
        assert_eq!(record.take_second(1, first, || 101).second, 100);
        // Lane 1 resolves first, with a skip: its interval moves the record's next instant, which lane 0 takes too,
        // even though its flush ran under another interval.
        let skipped = record.resolve(1, first, INTERVAL, false);
        assert_eq!(
            skipped,
            Resolved {
                fired: false,
                next_at: first + INTERVAL
            }
        );
        let fired = record.resolve(1, first, Duration::from_secs(30), true);
        assert_eq!(
            fired,
            Resolved {
                fired: true,
                next_at: first + INTERVAL
            }
        );
        assert_eq!(record.at_of(2), Some(first + INTERVAL));
        assert_eq!(record.current_deadline(), Some(first + INTERVAL));

        // A lane joining now starts at ordinal 2, at its instant.
        assert_eq!(
            record.join(),
            Joined {
                through: 1,
                ordinal: 2,
                at: Some(first + INTERVAL)
            }
        );
    }

    #[test]
    fn a_grace_expiry_is_counted_once_per_ordinal_and_a_straggler_reads_its_own_second() {
        let record = record();
        let first = record.first_at(Instant::now);
        let mut at = first;
        for ordinal in 1..=6 {
            record.take_second(ordinal, at, || 100 + ordinal);
            at = record.resolve(ordinal, at, INTERVAL, true).next_at;
        }
        assert!(record.count_grace(6));
        assert!(!record.count_grace(6));
        assert!(!record.count_grace(1), "ordinal 1 is no longer kept");
        // Lane 1, gated at ordinal 2 all along, is a straggler there.
        assert_eq!(
            record.take_second(2, first + INTERVAL, || 999),
            TickSecond {
                second: 999,
                straggler: true
            }
        );
        assert_eq!(
            record.take_second(3, first + 2 * INTERVAL, || 999),
            TickSecond {
                second: 103,
                straggler: false
            },
            "ordinal 3 is the oldest kept"
        );
        let resolved = record.resolve(2, first + INTERVAL, INTERVAL, true);
        assert_eq!(
            resolved,
            Resolved {
                fired: true,
                next_at: first + 2 * INTERVAL
            }
        );
    }

    #[test]
    fn no_ordinal_after_the_reset_marks_cutoff_fires_and_successors_start_at_the_ordinal_after_it() {
        let record = record();
        let first = record.first_at(Instant::now);
        record.take_second(1, first, || 100);
        let next = record.resolve(1, first, INTERVAL, true).next_at;
        // Lane 1 opens a reset window before it resolved ordinal 1: the cutoff is lane 0's, ordinal 1, which lane 1
        // still fires.
        let mark = record.mark_reset(1);
        assert_eq!(mark, ResetMark { target: 1, cutoff: 1 });
        assert!(record.resolve(1, first, INTERVAL, true).fired);
        // Ordinal 2 fires in no lane, and the record's current deadline stays the ordinal after the cutoff.
        assert!(!record.resolve(2, next, INTERVAL, true).fired);
        assert_eq!(record.current_deadline(), Some(next));
        // A later reset moves the target, never the cutoff.
        assert_eq!(record.mark_reset(2), ResetMark { target: 2, cutoff: 1 });
        assert_eq!(record.mark_reset(1), ResetMark { target: 2, cutoff: 1 });
        assert_eq!(record.mark(), Some(ResetMark { target: 2, cutoff: 1 }));
    }

    #[test]
    fn a_lane_joining_starts_after_the_newest_ordinal_and_through_the_reset_marks_cutoff_at_most() {
        let record = record();
        let first = record.first_at(Instant::now);
        let mut at = first;
        for ordinal in 1..=3 {
            at = record.resolve(ordinal, at, INTERVAL, false).next_at;
        }
        assert_eq!(
            record.join(),
            Joined {
                through: 3,
                ordinal: 4,
                at: Some(at)
            }
        );
        // A lane opens a reset window at ordinal 3, and another, whose window is not open yet, resolves ordinal 4 as a
        // skip: a lane joining now starts at ordinal 5, but the state owner's record hears of ordinal 3 at most.
        assert_eq!(record.mark_reset(1).cutoff, 3);
        let forbidden = record.resolve(4, at, INTERVAL, true);
        assert!(!forbidden.fired);
        assert_eq!(
            record.join(),
            Joined {
                through: 3,
                ordinal: 5,
                at: Some(forbidden.next_at)
            }
        );
    }

    #[test]
    fn every_lane_checks_its_flush_of_an_ordinal_against_the_first_lanes() {
        let record = record();
        let first = record.first_at(Instant::now);
        let flush = AdmissionTime::from_unix_secs(15);
        assert_eq!(record.agree_flush(1, flush), None, "no lane reached ordinal 1 yet");
        record.take_second(1, first, || 15);
        assert_eq!(record.agree_flush(1, flush), Some(flush));
        assert_eq!(
            record.agree_flush(1, AdmissionTime::from_unix_secs(14)),
            Some(flush),
            "a later lane's flush is checked against the first one's"
        );
    }

    #[test]
    fn the_flush_log_of_the_state_owner_keeps_as_many_ordinals_as_the_tick_record() {
        assert_eq!(
            saluki_metric_kernels::aggregate::lanes::FLUSH_LOG_DEPTH,
            TICK_RECORD_DEPTH,
            "a lane the tick record counts as a straggler is one the flush log does"
        );
    }
}

// Loom models of the tick record's protocol across lanes: the record itself, with `loom`'s mutex.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{sync::Arc, thread};

    use super::*;

    struct NoShare;

    impl LaneShare for NoShare {
        fn join(&self, _: u32, _: u16, _: u64) {}
        fn leave(&self, _: u32, _: u16) {}
        fn forget(&self, _: u32) {}
        fn skipped(&self, _: u32, _: u64, _: u16) {}
    }

    const INTERVAL: Duration = Duration::from_secs(15);

    /// What one lane does at the deadline of an ordinal: takes its second, and fires, skips, or gates and fires later,
    /// returning the second it flushes at, if it fires, and its next deadline.
    #[derive(Clone, Copy)]
    enum Lane {
        Fire,
        Skip,
        /// Opens a reset window after resolving the ordinal.
        FireThenOpen,
        /// Takes the second, gates, and fires later.
        GateThenFire,
    }

    /// Three lanes at ordinal 1 and then 2, one of them perhaps opening a reset window, each moving its deadline by an
    /// interval of its own: every lane flushes an ordinal at one second, takes one next instant, the one the first lane
    /// to resolve the ordinal set, and no ordinal after the cutoff fires, while every ordinal a lane fired before the
    /// mark was set is at or before the cutoff.
    ///
    /// Three threads taking a lock four or five times each interleave in too many ways to explore every one, so the
    /// model bounds how often an execution preempts a thread, as `loom` recommends: every interleaving with at most
    /// three preemption points is explored, which covers each pair of lanes racing on each ordinal, and the mark set
    /// between any two of another lane's steps.
    fn model(lanes: [Lane; 3]) {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(move || {
            let record = Arc::new(LineageLanes::new(std::sync::Arc::new(NoShare), 0));
            let first = Instant::now();
            record.first_at(|| first);
            let threads = lanes
                .into_iter()
                .enumerate()
                .map(|(lane, behavior)| {
                    let record = Arc::clone(&record);
                    thread::spawn(move || {
                        let lane = lane as u16;
                        // Lanes whose generations flush at different intervals.
                        let interval = INTERVAL + Duration::from_secs(u64::from(lane));
                        let mut at = first;
                        let mut fired = Vec::new();
                        for ordinal in 1..=2u64 {
                            let second = record.take_second(ordinal, at, || 100 * ordinal + u64::from(lane));
                            let resolved = match behavior {
                                Lane::Skip => record.resolve(ordinal, at, interval, false),
                                Lane::Fire | Lane::FireThenOpen | Lane::GateThenFire => {
                                    if matches!(behavior, Lane::GateThenFire) {
                                        thread::yield_now();
                                    }
                                    record.resolve(ordinal, at, interval, true)
                                }
                            };
                            if resolved.fired {
                                fired.push((ordinal, second.second));
                            }
                            if matches!(behavior, Lane::FireThenOpen) && ordinal == 1 {
                                record.mark_reset(1);
                            }
                            at = resolved.next_at;
                        }
                        (fired, at)
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = threads.into_iter().map(|lane| lane.join().unwrap()).collect::<Vec<_>>();
            let mark = record.mark();
            let mut seconds = std::collections::BTreeMap::new();
            let next = outcomes[0].1;
            assert_eq!(record.at_of(3), Some(next));
            for (fired, lane_next) in &outcomes {
                assert_eq!(*lane_next, next, "every lane's next deadline is the record's");
                for (ordinal, second) in fired {
                    let agreed = *seconds.entry(*ordinal).or_insert(*second);
                    assert_eq!(agreed, *second, "one second per ordinal");
                    if let Some(mark) = mark {
                        assert!(*ordinal <= mark.cutoff, "a fire after the cutoff took effect");
                    }
                }
            }
            if lanes.iter().any(|lane| matches!(lane, Lane::FireThenOpen)) {
                let mark = mark.expect("a lane opened a window");
                assert!(mark.cutoff >= 1, "the opening lane had resolved ordinal 1");
            }
        });
    }

    #[test]
    fn lanes_firing_and_skipping_one_ordinal_agree_on_its_second_and_next_instant() {
        model([Lane::Fire, Lane::Skip, Lane::Fire]);
    }

    #[test]
    fn no_ordinal_after_a_reset_marks_cutoff_fires_in_any_lane() {
        model([Lane::FireThenOpen, Lane::GateThenFire, Lane::Fire]);
    }

    /// A lane joins the record while two others resolve its ordinals: it starts after the newest ordinal resolved when
    /// it joined, at that ordinal's instant once known, never at a deadline already resolved.
    #[test]
    fn a_lane_joining_while_others_tick_starts_at_the_newest_ordinal() {
        loom::model(|| {
            let record = Arc::new(LineageLanes::new(std::sync::Arc::new(NoShare), 0));
            let first = Instant::now();
            record.first_at(|| first);
            let ticking = (0..2)
                .map(|_| {
                    let record = Arc::clone(&record);
                    thread::spawn(move || {
                        let next = record.resolve(1, first, INTERVAL, true).next_at;
                        record.resolve(2, next, INTERVAL, true);
                    })
                })
                .collect::<Vec<_>>();
            let joined = record.join();
            for lane in ticking {
                lane.join().unwrap();
            }
            assert!(joined.through <= 2);
            assert_eq!(joined.ordinal, joined.through + 1);
            if joined.through >= 1 {
                assert_eq!(
                    joined.at,
                    Some(first + u32::try_from(joined.through).unwrap() * INTERVAL)
                );
            }
            assert_eq!(
                record.join().through,
                2,
                "a lane joining once both resolved ordinal 2 starts after it"
            );
        });
    }

    /// Three lanes finish at once: they close at one time, the first one's.
    #[test]
    fn lanes_finishing_at_once_close_at_one_time() {
        loom::model(|| {
            let record = Arc::new(LineageLanes::new(std::sync::Arc::new(NoShare), 0));
            let lanes = (0..3u64)
                .map(|lane| {
                    let record = Arc::clone(&record);
                    thread::spawn(move || record.shutdown_at(|| AdmissionTime::from_unix_secs(100 + lane)))
                })
                .collect::<Vec<_>>();
            let closes = lanes.into_iter().map(|lane| lane.join().unwrap()).collect::<Vec<_>>();
            assert!(closes.windows(2).all(|pair| pair[0] == pair[1]));
        });
    }
}

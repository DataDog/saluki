//! The record that the instances of one aggregation lineage share when it runs on several lanes.
//!
//! An aggregation partitioned over lanes holds a disjoint share of its contexts in each lane's instance, so every rule
//! that concerns one context is exact in the lane that holds it. A few native rules couple every context of an
//! aggregation, and the lanes' instances keep them together in one [`LaneShared`]:
//!
//! - **The idle-counter back-fill start.** A flush adds zeros for idle counters from the previous flush that held any
//!   context, so a lane flushing at a tick starts them at the latest earlier flush at which *any* lane held contexts,
//!   in its epoch, once every member lane resolved the tick before ([`LaneShared::resolve`], [`Previous`]).
//! - **The flush count.** A tick at which any lane held contexts counts one flush, by the first lane to record it
//!   ([`FlushRecord::first`]), and so does an instance's closing flush.
//! - **The context limit warning.** It is logged once per episode, by the lane whose rejection starts it
//!   ([`LaneShared::try_breach`]), and its recovery once, by the lane whose flush ends it
//!   ([`LaneShared::clear_breach`]).
//!
//! The host of the lanes keeps membership ([`LaneShare`]): which lanes hold an instance in each epoch, and which
//! ordinals each resolved without a flush. A lane that is not a member counts as holding nothing at every ordinal, so
//! a lane without an instance never holds the others back. An epoch's log outlives its last member until the host
//! forgets the epoch ([`LaneShare::forget`]), so that a lane that reaches the epoch after the others left it, as one
//! that hears of a clock reset late does, counts and starts its zeros from the flushes the others made in it.
//!
//! # The flush log
//!
//! Per epoch of the upstream clock, the record keeps each member lane's newest resolved ordinal and, for the last
//! [`FLUSH_LOG_DEPTH`] ordinals, the latest flush time at which a lane held contexts and whether a lane counted it.
//! Older ordinals fold into one latest flush time. A lane that resolves an ordinal older than the log keeps is a
//! straggler: it never counts the flush, since it cannot know whether another lane did, and it starts its zeros from
//! its own last flush.
//!
//! With one lane, nothing needs this record: an instance without lanes keeps every rule itself.

use std::{collections::VecDeque, fmt};

use saluki_core::execution::{LaneFlush, LaneShare, LaneTick};

use crate::sync::{AtomicBool, Mutex, MutexGuard, Ordering};

/// The number of recent ordinals of an epoch whose flushes the record keeps one by one: the tick record depth of the
/// lanes' host.
pub const FLUSH_LOG_DEPTH: u64 = 4;

/// How one lane resolved one tick of its instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    /// The lane flushed at this flush time, in whole Unix seconds, holding contexts.
    Flushed(u64),

    /// The lane held no contexts at the tick, or did not flush at it.
    Empty,
}

/// The latest flush time before a tick at which any lane held contexts, as the record knows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Previous {
    /// Every member lane resolved the tick before, so the time is known: `None` if no lane held contexts at any earlier
    /// flush of the epoch.
    Known(Option<u64>),

    /// Some member lane has not resolved the tick before yet: the lane falls back to its own last flush, and its host
    /// counts the divergence.
    Unresolved,
}

/// What recording one lane's outcome of a tick returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushRecord {
    /// Whether this lane is the first to record a flush holding contexts at the tick, and so counts it.
    pub first: bool,

    /// Where the lane's idle-counter zeros start from.
    pub previous: Previous,
}

/// A control message that tells one lane's instance of a lineage that it was rebuilt, empty, after a panic.
///
/// A rebuilt instance starts as if it had never flushed, as a restarted native aggregator does: its first flush adds
/// no idle-counter zeros, so it starts them from its own last flush, which it has none of, and it starts them from the
/// lanes' record only from its second flush on. Its flushes still count, and still tell the other lanes where theirs
/// start. A host sends it to the instance right after rebuilding it; an instance without lanes ignores it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Restarted;

/// A read-only control message that takes how many of one lane's flushes started their idle-counter zeros from the
/// instance's own last flush since the last take, because the lanes' record could not say where the lanes left them
/// ([`Previous::Unresolved`]): a lane behind the others, or a straggler.
///
/// Such a flush may add zeros another lane's flush of the same tick does not, so the host counts it as a tick the
/// lanes resolved apart. A rebuilt instance's first flush ([`Restarted`]) is not one. An instance without lanes leaves
/// the count at zero, as does a kernel that does not know the message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TakeLaneFallbacks {
    /// The flushes taken, added to what the message held.
    pub flushes: u64,
}

/// A read-only control message that asks an aggregation instance how many contexts it holds, answered in constant
/// time; a kernel that does not know the message leaves it `None`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextCount {
    /// The contexts the instance holds, idle ones included.
    pub contexts: Option<usize>,
}

/// A read-only control message that counts how many of a list of contexts an aggregation instance holds, one lookup
/// each: the contexts two instances of one aggregation hold at once, given the contexts of one ([`ContextSnapshot`]).
///
/// [`ContextSnapshot`]: super::ContextSnapshot
#[derive(Clone, Debug, Default)]
pub struct CountHeld {
    /// The contexts to look up.
    pub entries: Vec<super::AggregateContextSnapshotEntry>,

    /// How many of them the instance holds, added to what the message held.
    pub held: u64,
}

/// The record that every lane's instance of one aggregation lineage shares.
///
/// Every lane's instance is built against one record ([`AggregateKernel::with_lanes`][super::AggregateKernel]), which
/// the host creates once per lineage and keys by epoch inside. Nothing in it is touched per metric: a lane takes its
/// lock a few times per tick, and reads the breach flag only on a rejection or at the end of a pass.
pub struct LaneShared {
    lanes: u16,
    log: Mutex<FlushLog>,
    breached: AtomicBool,
}

impl LaneShared {
    /// Creates the record of a lineage that runs on `lanes` lanes.
    pub fn new(lanes: u16) -> Self {
        Self {
            lanes,
            log: Mutex::new(FlushLog::default()),
            breached: AtomicBool::new(false),
        }
    }

    /// Returns the number of lanes the lineage runs on.
    pub fn lanes(&self) -> u16 {
        self.lanes
    }

    /// Records this lane's outcome of `tick`, and returns whether the lane counts the flush and where its idle-counter
    /// zeros start from.
    ///
    /// `first` is true for the first lane to record a flush holding contexts at the tick: once per ordinal, and once
    /// for the closing flushes of an epoch's instances. `previous`, the latest flush time before the tick at which any
    /// lane held contexts in the tick's epoch, is known once every member lane resolved the ordinal before it; for a
    /// closing flush, once every member resolved the newest ordinal any member resolved. Resolving an ordinal makes the
    /// lane a member of its epoch, if it was not.
    pub fn resolve(&self, tick: LaneTick, outcome: Resolution) -> FlushRecord {
        let mut log = self.lock();
        let epoch = log.epoch_mut(tick.epoch);
        let flushed = match outcome {
            Resolution::Flushed(at) => Some(at),
            Resolution::Empty => None,
        };
        match tick.flush {
            LaneFlush::Ordinal(ordinal) => epoch.resolve_ordinal(tick.lane, ordinal, flushed),
            LaneFlush::Closing => epoch.resolve_closing(flushed),
        }
    }

    /// Marks the lineage as over its context limit, returning `true` if it was not already: the caller logs the
    /// warning, once per episode across lanes.
    pub fn try_breach(&self) -> bool {
        // Rejections at the limit come in bursts from every lane: only the first writes the flag.
        !self.breached.load(Ordering::Acquire) && !self.breached.swap(true, Ordering::AcqRel)
    }

    /// Marks the lineage as below its context limit again, returning `true` if it was over it: the caller logs the
    /// recovery, once per episode across lanes.
    pub fn clear_breach(&self) -> bool {
        self.breached.load(Ordering::Acquire) && self.breached.swap(false, Ordering::AcqRel)
    }

    fn lock(&self) -> MutexGuard<'_, FlushLog> {
        // Every update of the log completes before its lock is released, and none can panic midway through: a poisoned
        // lock holds a consistent log.
        self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl LaneShare for LaneShared {
    fn join(&self, epoch: u32, lane: u16, through: u64) {
        let mut log = self.lock();
        let member = log.epoch_mut(epoch).member_mut(lane);
        *member = Some(member.map_or(through, |resolved| resolved.max(through)));
    }

    fn leave(&self, epoch: u32, lane: u16) {
        let mut log = self.lock();
        let Some(log) = log.epochs.iter_mut().find(|log| log.epoch == epoch) else {
            return;
        };
        if let Some(member) = log.members.get_mut(usize::from(lane)) {
            *member = None;
        }
    }

    fn forget(&self, epoch: u32) {
        let mut log = self.lock();
        // A lane that joined the epoch again holds it: its log stays.
        log.epochs
            .retain(|log| log.epoch != epoch || log.members.iter().any(Option::is_some));
    }

    fn skipped(&self, epoch: u32, ordinal: u64, lane: u16) {
        let mut log = self.lock();
        let Some(log) = log.epochs.iter_mut().find(|log| log.epoch == epoch) else {
            return;
        };
        // Only a member resolves ordinals; a lane that left holds nothing, at every ordinal.
        if let Some(Some(resolved)) = log.members.get_mut(usize::from(lane)) {
            *resolved = (*resolved).max(ordinal);
        }
    }
}

impl fmt::Debug for LaneShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaneShared")
            .field("lanes", &self.lanes)
            .field("breached", &self.breached.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// The flushes of a lineage on lanes, per epoch of the upstream clock: at most a few epochs at once, inside a reset
/// window.
#[derive(Default)]
struct FlushLog {
    epochs: Vec<EpochLog>,
}

impl FlushLog {
    /// Returns the log of `epoch`, creating it if no lane joined it yet.
    fn epoch_mut(&mut self, epoch: u32) -> &mut EpochLog {
        let index = match self.epochs.iter().position(|log| log.epoch == epoch) {
            Some(index) => index,
            None => {
                self.epochs.push(EpochLog::new(epoch));
                self.epochs.len() - 1
            }
        };
        &mut self.epochs[index]
    }
}

/// The flushes of one epoch.
struct EpochLog {
    epoch: u32,
    /// Per lane, the newest ordinal it resolved, or `None` if it holds no instance in the epoch.
    members: Vec<Option<u64>>,
    /// The ordinals kept one by one, oldest first, all newer than `folded_through`.
    recent: VecDeque<OrdinalLog>,
    /// The latest flush time holding contexts at an ordinal no longer kept.
    folded: Option<u64>,
    /// The newest ordinal folded into `folded`.
    folded_through: u64,
    /// Whether a closing flush holding contexts was counted in the epoch.
    closing_counted: bool,
}

/// What the lanes recorded at one ordinal.
struct OrdinalLog {
    ordinal: u64,
    /// The latest flush time at which a lane held contexts at the ordinal.
    latest: Option<u64>,
    /// Whether a lane counted the flush.
    counted: bool,
}

impl EpochLog {
    fn new(epoch: u32) -> Self {
        Self {
            epoch,
            members: Vec::new(),
            recent: VecDeque::new(),
            folded: None,
            folded_through: 0,
            closing_counted: false,
        }
    }

    fn member_mut(&mut self, lane: u16) -> &mut Option<u64> {
        let lane = usize::from(lane);
        if self.members.len() <= lane {
            self.members.resize(lane + 1, None);
        }
        &mut self.members[lane]
    }

    /// Returns `true` if every member lane resolved `ordinal`.
    fn resolved_by_every_member(&self, ordinal: u64) -> bool {
        self.members.iter().flatten().all(|resolved| *resolved >= ordinal)
    }

    /// Returns the latest flush time holding contexts at an ordinal up to `through`.
    fn latest_through(&self, through: u64) -> Option<u64> {
        self.recent
            .iter()
            .filter(|log| log.ordinal <= through)
            .filter_map(|log| log.latest)
            .chain(self.folded)
            .max()
    }

    /// Returns the log of `ordinal`, newer than every ordinal folded, creating it and folding the oldest ones it pushes
    /// out of the depth.
    fn ordinal_mut(&mut self, ordinal: u64) -> &mut OrdinalLog {
        debug_assert!(ordinal > self.folded_through, "folded ordinals are never logged again");
        let position = self.recent.partition_point(|log| log.ordinal < ordinal);
        if self.recent.get(position).is_none_or(|log| log.ordinal != ordinal) {
            self.recent.insert(
                position,
                OrdinalLog {
                    ordinal,
                    latest: None,
                    counted: false,
                },
            );
        }
        let mut position = position;
        let newest = self.recent.back().map_or(ordinal, |log| log.ordinal);
        while let Some(oldest) = self.recent.front() {
            if newest - oldest.ordinal < FLUSH_LOG_DEPTH {
                break;
            }
            let oldest = self.recent.pop_front().expect("the oldest ordinal is logged");
            self.folded = self.folded.max(oldest.latest);
            self.folded_through = oldest.ordinal;
            position -= 1;
        }
        &mut self.recent[position]
    }

    fn resolve_ordinal(&mut self, lane: u16, ordinal: u64, flushed: Option<u64>) -> FlushRecord {
        let member = self.member_mut(lane);
        *member = Some(member.map_or(ordinal, |resolved| resolved.max(ordinal)));
        let behind = self
            .recent
            .back()
            .is_some_and(|newest| newest.ordinal.saturating_sub(ordinal) >= FLUSH_LOG_DEPTH);
        if ordinal <= self.folded_through || behind {
            // A straggler: whether its ordinal was counted, and what came before it, are folded away, or would be the
            // moment the ordinal were logged, further behind the newest than the log keeps.
            return FlushRecord {
                first: false,
                previous: Previous::Unresolved,
            };
        }
        let log = self.ordinal_mut(ordinal);
        let first = match flushed {
            Some(at) => {
                log.latest = log.latest.max(Some(at));
                !std::mem::replace(&mut log.counted, true)
            }
            None => false,
        };
        let previous = if ordinal <= 1 || self.resolved_by_every_member(ordinal - 1) {
            Previous::Known(self.latest_through(ordinal - 1))
        } else {
            Previous::Unresolved
        };
        FlushRecord { first, previous }
    }

    fn resolve_closing(&mut self, flushed: Option<u64>) -> FlushRecord {
        let newest = self.members.iter().flatten().copied().max().unwrap_or(0);
        let first = flushed.is_some() && !std::mem::replace(&mut self.closing_counted, true);
        let previous = if self.resolved_by_every_member(newest) {
            Previous::Known(self.latest_through(newest))
        } else {
            Previous::Unresolved
        };
        FlushRecord { first, previous }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    fn tick(epoch: u32, ordinal: u64, lane: u16) -> LaneTick {
        LaneTick {
            epoch,
            flush: LaneFlush::Ordinal(ordinal),
            lane,
        }
    }

    fn closing(epoch: u32, lane: u16) -> LaneTick {
        LaneTick {
            epoch,
            flush: LaneFlush::Closing,
            lane,
        }
    }

    fn record(first: bool, previous: Previous) -> FlushRecord {
        FlushRecord { first, previous }
    }

    #[test]
    fn an_ordinal_further_behind_the_newest_than_the_log_keeps_is_a_straggler_even_when_never_logged() {
        // Lane 0 resolves ordinal 8 before any other is logged; lane 1 then resolves ordinal 3, which the log would
        // fold away the moment it logged it.
        let shared = LaneShared::new(2);
        shared.join(0, 0, 0);
        shared.join(0, 1, 0);
        assert!(shared.resolve(tick(0, 8, 0), Resolution::Flushed(80)).first);
        assert_eq!(
            shared.resolve(tick(0, 3, 1), Resolution::Flushed(30)),
            record(false, Previous::Unresolved)
        );
        // The newer ordinals are as they were.
        assert_eq!(
            shared.resolve(tick(0, 9, 0), Resolution::Flushed(90)),
            record(true, Previous::Unresolved),
            "lane 1 has not resolved ordinal 8"
        );
    }

    #[test]
    fn a_breach_episode_is_started_and_ended_once_across_lanes() {
        let shared = LaneShared::new(4);
        assert_eq!(shared.lanes(), 4);
        assert!(!shared.clear_breach(), "no episode is open");
        assert!(shared.try_breach(), "the first rejection starts the episode");
        assert!(!shared.try_breach(), "a later rejection, in any lane, does not");
        assert!(shared.clear_breach(), "the first pass below the limit ends it");
        assert!(!shared.clear_breach());
        assert!(shared.try_breach(), "a new episode warns again");
    }

    #[test]
    fn the_first_lane_to_flush_contexts_at_an_ordinal_counts_it_and_empty_lanes_count_nothing() {
        let shared = LaneShared::new(3);
        for lane in 0..3 {
            shared.join(0, lane, 0);
        }
        assert_eq!(
            shared.resolve(tick(0, 1, 2), Resolution::Empty),
            record(false, Previous::Known(None))
        );
        assert_eq!(
            shared.resolve(tick(0, 1, 0), Resolution::Flushed(15)),
            record(true, Previous::Known(None))
        );
        assert_eq!(
            shared.resolve(tick(0, 1, 1), Resolution::Flushed(15)),
            record(false, Previous::Known(None)),
            "a later lane flushing the same ordinal does not count it"
        );
        // Ordinal 2: nobody held contexts at it, so nobody counts it.
        for lane in 0..3 {
            assert!(!shared.resolve(tick(0, 2, lane), Resolution::Empty).first);
        }
    }

    #[test]
    fn idle_counters_start_from_the_latest_flush_any_lane_held_contexts_at() {
        // Fixture X1: lane 0 holds `a` and flushes at 15 and 30; lane 1 is empty at both; at the tick at 45, lane 1
        // (which now holds `b`) starts its zeros at 30, not at its own never.
        let shared = LaneShared::new(2);
        shared.join(0, 0, 0);
        shared.join(0, 1, 0);
        shared.resolve(tick(0, 1, 0), Resolution::Flushed(15));
        shared.skipped(0, 1, 1);
        shared.resolve(tick(0, 2, 0), Resolution::Flushed(30));
        shared.resolve(tick(0, 2, 1), Resolution::Empty);
        assert_eq!(
            shared.resolve(tick(0, 3, 1), Resolution::Flushed(45)),
            record(true, Previous::Known(Some(30)))
        );
        // Lane 0's flush at 45 starts from 30 too, whichever lane flushed first.
        assert_eq!(
            shared.resolve(tick(0, 3, 0), Resolution::Flushed(45)),
            record(false, Previous::Known(Some(30)))
        );
    }

    #[test]
    fn where_zeros_start_is_unknown_until_every_member_resolved_the_ordinal_before() {
        let shared = LaneShared::new(2);
        shared.join(0, 0, 0);
        shared.join(0, 1, 0);
        shared.resolve(tick(0, 1, 0), Resolution::Flushed(15));
        // Lane 1 is still gated at ordinal 1: lane 0's flush at ordinal 2 cannot know whether lane 1 holds contexts
        // there.
        assert_eq!(
            shared.resolve(tick(0, 2, 0), Resolution::Flushed(30)),
            record(true, Previous::Unresolved)
        );
        // A lane that left holds nothing, and holds nobody back.
        shared.leave(0, 1);
        assert_eq!(
            shared.resolve(tick(0, 3, 0), Resolution::Flushed(45)),
            record(true, Previous::Known(Some(30)))
        );
        // A lane that joins at the newest ordinal resolved everything before it.
        shared.join(0, 1, 3);
        assert_eq!(
            shared.resolve(tick(0, 4, 1), Resolution::Flushed(60)),
            record(true, Previous::Known(Some(45)))
        );
    }

    #[test]
    fn a_closing_flush_starts_from_the_newest_ordinal_resolved_and_counts_once_per_epoch() {
        let shared = LaneShared::new(2);
        shared.join(5, 0, 0);
        shared.join(5, 1, 0);
        shared.resolve(tick(5, 1, 0), Resolution::Flushed(30));
        shared.resolve(tick(5, 1, 1), Resolution::Empty);
        shared.resolve(tick(5, 2, 0), Resolution::Flushed(45));
        // Lane 1 has not resolved ordinal 2: a closing flush now cannot know where zeros start.
        assert_eq!(
            shared.resolve(closing(5, 1), Resolution::Flushed(50)),
            record(true, Previous::Unresolved)
        );
        shared.skipped(5, 2, 1);
        assert_eq!(
            shared.resolve(closing(5, 0), Resolution::Flushed(50)),
            record(false, Previous::Known(Some(45))),
            "the epoch's closing flush was counted by lane 1"
        );
        // Another epoch has its own log.
        assert_eq!(
            shared.resolve(closing(6, 0), Resolution::Flushed(10)),
            record(true, Previous::Known(None))
        );
    }

    #[test]
    fn a_lane_further_behind_than_the_log_keeps_is_a_straggler_that_never_counts() {
        let shared = LaneShared::new(2);
        shared.join(0, 0, 0);
        shared.join(0, 1, 0);
        for ordinal in 1..=6 {
            shared.resolve(tick(0, ordinal, 0), Resolution::Flushed(ordinal * 10));
        }
        // Ordinals 1 and 2 were folded away: lane 1 is a straggler at both, even though nobody counted it there.
        assert_eq!(
            shared.resolve(tick(0, 2, 1), Resolution::Flushed(20)),
            record(false, Previous::Unresolved)
        );
        // Ordinal 3 is still kept: lane 1 resolved 2, so it knows where its zeros start, and lane 0 counted it.
        assert_eq!(
            shared.resolve(tick(0, 3, 1), Resolution::Flushed(30)),
            record(false, Previous::Known(Some(20)))
        );
    }

    #[test]
    fn an_epoch_whose_members_all_left_keeps_its_flushes_until_it_is_forgotten() {
        // Lane 1 flushed ordinal 1 and the epoch's closing flush, and left; lane 0, which reaches the epoch late, joins
        // it after the last ordinal any lane resolved: its closing flush is not counted again, and starts its zeros
        // from lane 1's flushes.
        let shared = LaneShared::new(2);
        shared.join(1, 1, 0);
        assert!(shared.resolve(tick(1, 1, 1), Resolution::Flushed(15)).first);
        assert!(shared.resolve(closing(1, 1), Resolution::Flushed(20)).first);
        shared.leave(1, 1);
        shared.join(1, 0, 1);
        assert_eq!(
            shared.resolve(closing(1, 0), Resolution::Flushed(20)),
            record(false, Previous::Known(Some(15)))
        );
        // Forgetting an epoch a lane is a member of keeps its log; once its last member left, the log goes.
        shared.forget(1);
        assert!(!shared.resolve(closing(1, 0), Resolution::Flushed(20)).first);
        shared.leave(1, 0);
        shared.forget(1);
        assert_eq!(
            shared.resolve(closing(1, 0), Resolution::Flushed(20)),
            record(true, Previous::Known(None)),
            "a forgotten epoch starts afresh"
        );
    }

    #[test]
    fn a_skip_by_a_lane_that_is_not_a_member_changes_nothing() {
        let shared = LaneShared::new(2);
        shared.join(0, 0, 0);
        shared.skipped(0, 1, 1);
        shared.resolve(tick(0, 1, 0), Resolution::Flushed(15));
        assert_eq!(
            shared.resolve(tick(0, 2, 0), Resolution::Flushed(30)),
            record(true, Previous::Known(Some(15)))
        );
    }
}

// Loom models of the record's protocol across lanes: the record itself, with `loom`'s mutex and atomics.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{sync::Arc, thread};

    use super::*;

    fn tick(ordinal: u64, lane: u16) -> LaneTick {
        LaneTick {
            epoch: 0,
            flush: LaneFlush::Ordinal(ordinal),
            lane,
        }
    }

    /// Three lanes resolve two consecutive ordinals, each holding contexts at both, in any interleaving: exactly one
    /// counts each ordinal, and `previous` at ordinal 2 is known only to a lane that resolves it after every member
    /// resolved ordinal 1, and is then the flush at ordinal 1.
    #[test]
    fn one_lane_counts_each_ordinal_and_previous_is_known_only_once_every_member_resolved_the_one_before() {
        loom::model(|| {
            let shared = Arc::new(LaneShared::new(3));
            for lane in 0..3 {
                shared.join(0, lane, 0);
            }
            let lanes = (0..3u16)
                .map(|lane| {
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || {
                        let one = shared.resolve(tick(1, lane), Resolution::Flushed(10));
                        let two = shared.resolve(tick(2, lane), Resolution::Flushed(20));
                        (one, two)
                    })
                })
                .collect::<Vec<_>>();
            let records = lanes.into_iter().map(|lane| lane.join().unwrap()).collect::<Vec<_>>();
            assert_eq!(records.iter().filter(|(one, _)| one.first).count(), 1);
            assert_eq!(records.iter().filter(|(_, two)| two.first).count(), 1);
            for (one, two) in &records {
                assert_eq!(one.previous, Previous::Known(None));
                assert!(matches!(two.previous, Previous::Known(Some(10)) | Previous::Unresolved));
            }
            // The last lane to resolve ordinal 2 resolved it after every lane resolved ordinal 1.
            assert!(records.iter().any(|(_, two)| two.previous == Previous::Known(Some(10))));
        });
    }

    /// One lane lags at ordinal 1 while another leaves and a third joins between ordinals, and a closing flush runs:
    /// the closing flush counts once, and a joining lane never holds `previous` back.
    #[test]
    fn a_lane_that_leaves_or_joins_never_holds_previous_back_and_a_closing_flush_counts_once() {
        loom::model(|| {
            let shared = Arc::new(LaneShared::new(3));
            shared.join(0, 0, 0);
            shared.join(0, 1, 0);
            let lagging = {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    shared.resolve(tick(1, 0), Resolution::Flushed(10));
                    shared.resolve(
                        LaneTick {
                            epoch: 0,
                            flush: LaneFlush::Closing,
                            lane: 0,
                        },
                        Resolution::Flushed(12),
                    )
                })
            };
            let leaving = {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    shared.leave(0, 1);
                    shared.join(0, 2, 1);
                    shared.resolve(tick(2, 2), Resolution::Flushed(20))
                })
            };
            let closing = lagging.join().unwrap();
            let joined = leaving.join().unwrap();
            assert!(closing.first, "the only closing flush counts");
            // The joined lane resolved ordinal 1 by joining at it, and lane 1 left: only lane 0, which flushed contexts
            // at ordinal 1, can hold `previous` back, so it is unknown before lane 0 resolved ordinal 1 and its flush
            // there afterwards.
            assert!(matches!(
                joined.previous,
                Previous::Known(Some(10)) | Previous::Unresolved
            ));
            let after = shared.resolve(tick(2, 0), Resolution::Flushed(20));
            assert_eq!(after.previous, Previous::Known(Some(10)));
            assert_eq!(u8::from(after.first) + u8::from(joined.first), 1);
        });
    }

    /// Rejections and pass ends interleave across two lanes: each episode warns once and recovers once.
    #[test]
    fn a_breach_episode_warns_once_and_recovers_once() {
        loom::model(|| {
            let shared = Arc::new(LaneShared::new(2));
            let lanes = (0..2)
                .map(|_| {
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || {
                        let warned = shared.try_breach();
                        let recovered = shared.clear_breach();
                        (warned, recovered)
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = lanes.into_iter().map(|lane| lane.join().unwrap()).collect::<Vec<_>>();
            let warnings = outcomes.iter().filter(|(warned, _)| *warned).count();
            let recoveries = outcomes.iter().filter(|(_, recovered)| *recovered).count();
            // Every episode that warned ended, since every lane ended one after its rejection.
            assert!(warnings >= 1);
            assert_eq!(warnings, recoveries);
            assert!(!shared.clear_breach());
        });
    }
}

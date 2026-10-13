//! Budgets of state shared between kernels.

use std::fmt;
// The `loom` models below run this module's own code: under the feature, its count is a `loom` atomic, which only a
// `loom::model` can run.
#[cfg(not(all(test, feature = "loom")))]
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[cfg(all(test, feature = "loom"))]
use loom::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// A budget of state units, such as tracked contexts, shared by every kernel that draws on it.
///
/// A budget has a fixed capacity and counts the units in use. A kernel acquires units before it creates state and
/// releases them when it removes that state, so that several kernels, or several instances of one kernel, never hold
/// more than the capacity together. Clones share the same count.
///
/// Acquiring is a compare-and-swap loop, so concurrent holders never overshoot the capacity, and the count needs no
/// lock. A budget made with [`unlimited`][Self::unlimited] never refuses, and still counts what is in use. Holders that
/// acquire many units one at a time, from several threads, take them in blocks through a [`BudgetReservation`]
/// instead, which acquires a block with one compare-and-swap.
#[derive(Clone)]
pub struct StateBudget {
    inner: Arc<Inner>,
}

struct Inner {
    used: AtomicU64,
    capacity: u64,
    /// Where the budget records its operations, if it was created to.
    #[cfg(any(test, feature = "test-util"))]
    recording: Option<BudgetRecording>,
}

impl StateBudget {
    /// Creates a budget of `capacity` units, none of them in use.
    ///
    /// A capacity of `0` refuses every acquisition.
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                used: AtomicU64::new(0),
                capacity,
                #[cfg(any(test, feature = "test-util"))]
                recording: None,
            }),
        }
    }

    /// Creates a budget of `capacity` units, as [`new`][Self::new] does, that records every acquisition and release
    /// made on it, with the units in use each one observed, in the returned recording.
    ///
    /// A test checks from the recording that every refusal was justified when it happened, and counts what holders
    /// held between their acquisitions and releases, however they interleaved.
    #[cfg(any(test, feature = "test-util"))]
    pub fn recorded(capacity: u64) -> (Self, BudgetRecording) {
        let recording = BudgetRecording::default();
        let budget = Self {
            inner: Arc::new(Inner {
                used: AtomicU64::new(0),
                capacity,
                recording: Some(recording.clone()),
            }),
        };
        (budget, recording)
    }

    /// Creates a budget that never refuses an acquisition.
    pub fn unlimited() -> Self {
        Self::new(u64::MAX)
    }

    /// Acquires `units` units if they fit: returns `true`, and counts them as in use, if the units in use plus `units`
    /// are at most the capacity, and `false`, changing nothing, otherwise.
    pub fn try_acquire(&self, units: u64) -> bool {
        self.try_acquire_below(units, self.inner.capacity)
    }

    /// Acquires `units` units if they fit below `limit`: returns `true`, and counts them as in use, if the units in use
    /// plus `units` are at most the smaller of `limit` and the capacity, and `false`, changing nothing, otherwise.
    ///
    /// The check and the acquisition are one compare-and-swap loop, so holders that share the budget and pass the
    /// same limit never hold more than it together, however their calls interleave. A check of
    /// [`used`][Self::used] followed by [`try_acquire`][Self::try_acquire] gives no such bound: several holders can
    /// each see room for one more unit, and all take it. A limit below the units in use refuses everything, and
    /// releases nothing: units acquired under a higher limit stay held.
    pub fn try_acquire_below(&self, units: u64, limit: u64) -> bool {
        let bound = limit.min(self.inner.capacity);
        let outcome = self
            .inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(units).filter(|total| *total <= bound)
            });
        #[cfg(any(test, feature = "test-util"))]
        self.record(|| BudgetOperation::Acquire {
            units,
            bound,
            observed: outcome.unwrap_or_else(|used| used),
            granted: outcome.is_ok(),
        });
        outcome.is_ok()
    }

    /// Acquires as many units as fit below `limit`, up to `units`, and returns how many it acquired.
    ///
    /// The acquisition takes the smaller of `units` and the room left below the smaller of `limit` and the capacity:
    /// at least one unit if one fits, and none, changing nothing, otherwise. The check and the acquisition are one
    /// compare-and-swap loop, as in [`try_acquire_below`][Self::try_acquire_below]. Asking for no units acquires none.
    pub fn try_acquire_up_to(&self, units: u64, limit: u64) -> u64 {
        if units == 0 {
            return 0;
        }
        let bound = limit.min(self.inner.capacity);
        let mut taken = 0;
        let outcome = self
            .inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                taken = units.min(bound.saturating_sub(used));
                (taken > 0).then(|| used + taken)
            });
        #[cfg(any(test, feature = "test-util"))]
        self.record(|| BudgetOperation::Acquire {
            // A refusal is recorded as asking for the one unit it would have taken, which did not fit.
            units: taken.max(1),
            bound,
            observed: outcome.unwrap_or_else(|used| used),
            granted: outcome.is_ok(),
        });
        match outcome {
            Ok(_) => taken,
            Err(_) => 0,
        }
    }

    /// Releases `units` units that were acquired earlier.
    ///
    /// Releasing more units than are in use is a bug in the holder. It leaves nothing in use rather than wrapping
    /// around, and fails a debug assertion.
    pub fn release(&self, units: u64) {
        let previous = self
            .inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(units))
            })
            .unwrap_or_else(|used| used);
        #[cfg(any(test, feature = "test-util"))]
        self.record(|| BudgetOperation::Release {
            units,
            observed: previous,
        });
        debug_assert!(
            previous >= units,
            "released {} state units but only {} were in use",
            units,
            previous
        );
    }

    /// Returns the number of units in use.
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    /// Returns the capacity: the most units that can be in use at once.
    pub fn capacity(&self) -> u64 {
        self.inner.capacity
    }

    /// Returns `true` if this budget never refuses an acquisition.
    pub fn is_unlimited(&self) -> bool {
        self.inner.capacity == u64::MAX
    }

    /// Returns `true` if `self` and `other` share one count: one is a clone of the other.
    pub fn shares_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Records an operation, if the budget records them.
    #[cfg(any(test, feature = "test-util"))]
    fn record(&self, operation: impl FnOnce() -> BudgetOperation) {
        if let Some(recording) = &self.inner.recording {
            recording.push(operation());
        }
    }
}

/// Units of two budgets that one holder takes ahead of their use, in blocks, for one batch of work: units of a group's
/// budget, bounded by the group's limit, and of the global budget every group draws on.
///
/// A holder that creates state one unit at a time, such as an aggregation admitting new contexts, can take each unit
/// with a compare-and-swap on each budget. Holders that share the budgets from several threads then contend on both
/// counts for every unit. A reservation instead takes a block of units with one compare-and-swap on each budget, and
/// hands them out one at a time, with no atomic operation, until the block is used up and it takes the next one.
/// Dropping the reservation, at the end of the batch, returns the units it took and did not hand out, so that between
/// batches the budgets count only the units holders handed out.
///
/// # Blocks
///
/// Each unit is one unit of both budgets. A block is taken from the group's budget first, as many units as fit below
/// the limit, then as many of those as fit in the global budget; the group's units the global budget has no room for
/// are given back at once, so that the group keeps no unit without its global unit. Each block is as many units as the
/// reservation handed out before it, but at least its first block's size, one unit unless
/// [`with_first_block`][Self::with_first_block] sets more, and at most its largest, [`MAX_BLOCK`][Self::MAX_BLOCK]
/// unless [`with_max_block`][Self::with_max_block] sets fewer. So a batch that needs one or two units takes exactly
/// those, as unit-at-a-time acquisition does; a batch that needs many takes one block for every `MAX_BLOCK` units once
/// its blocks have grown; and a holder whose previous batch needed many can start the next one with blocks of that
/// size. Whenever [`take`][Self::take] returns, the units a reservation holds and has not handed out are fewer than its
/// last block, so fewer than `MAX_BLOCK`; fewer than the units it handed out unless its first block was set larger; and
/// none if its largest block is one unit.
///
/// # Limits
///
/// The budgets count the units a reservation holds as in use, so that neither the group's limit nor the global
/// capacity is ever exceeded, however reservations on several threads interleave. A unit is refused only when the
/// group's budget is at its limit, or the global budget at its capacity, counting the units every reservation holds:
/// while units sit in another holder's reservation, a holder can be refused a unit that the other reservation returns
/// at the end of its batch, fewer than `MAX_BLOCK` units for each other reservation that holds units at that moment.
/// Holders whose reservations take one unit per block hold none unused, and holders that never hold reservations at
/// once, such as several holders on one thread that each drop their reservation before the next one takes a unit: both
/// are refused exactly when unit-at-a-time acquisition would refuse them.
///
/// A reservation lives for one batch of work: one kept between batches would hold units that no state uses.
pub struct BudgetReservation<'a> {
    group: &'a StateBudget,
    limit: u64,
    global: &'a StateBudget,
    /// Units of both budgets the reservation holds and has not handed out.
    unused: u64,
    /// Units the reservation has handed out.
    taken: u64,
    /// The fewest units a block takes, if that many fit: the first block's size.
    first_block: u64,
    /// The most units a block takes.
    max_block: u64,
}

impl<'a> BudgetReservation<'a> {
    /// The most units a reservation takes in one block.
    pub const MAX_BLOCK: u64 = 64;

    /// Creates a reservation of units of `group`, bounded by `limit`, and of `global`, bounded by its capacity, which
    /// holds no units yet. Its blocks start at one unit and grow up to [`MAX_BLOCK`][Self::MAX_BLOCK].
    pub fn new(group: &'a StateBudget, limit: u64, global: &'a StateBudget) -> Self {
        Self {
            group,
            limit,
            global,
            unused: 0,
            taken: 0,
            first_block: 1,
            max_block: Self::MAX_BLOCK,
        }
    }

    /// Caps the reservation's blocks at `units`, between one and [`MAX_BLOCK`][Self::MAX_BLOCK].
    ///
    /// A cap of one takes each unit when it is handed out, with one acquisition of each budget, so that no unit ever
    /// sits in the reservation: the holder is refused exactly as by unit-at-a-time acquisition, whatever other holders
    /// do at once, at the cost of contending on both counts for every unit.
    pub fn with_max_block(mut self, units: u64) -> Self {
        self.max_block = units.clamp(1, Self::MAX_BLOCK);
        self
    }

    /// Starts the reservation's blocks at `units` rather than one unit, at most its largest block: for a holder whose
    /// previous batch took that many units, so that a batch as large takes no smaller blocks before reaching that size.
    ///
    /// A first block larger than what the batch uses holds the rest until the reservation is dropped, fewer than the
    /// first block's size.
    pub fn with_first_block(mut self, units: u64) -> Self {
        self.first_block = units.max(1);
        self
    }

    /// Takes one unit of both budgets for the holder: from the block the reservation holds, or, once that is used up,
    /// from a new block. Returns `false`, taking nothing, if not one unit fits in both budgets.
    ///
    /// The unit stays in use once the reservation is dropped: the holder releases it from both budgets when it removes
    /// the state the unit was taken for.
    pub fn take(&mut self) -> bool {
        if self.unused == 0 && !self.take_block() {
            return false;
        }
        self.unused -= 1;
        self.taken += 1;
        true
    }

    /// Takes the next block, as large as the units handed out so far but at least the first block's size, at most the
    /// largest block's, and smaller if fewer fit. Returns `false`, holding nothing, if not one unit fits in both
    /// budgets.
    fn take_block(&mut self) -> bool {
        debug_assert_eq!(
            self.unused, 0,
            "a reservation takes a block once it has handed out the last one"
        );
        let wanted = self.taken.max(self.first_block).clamp(1, self.max_block);
        // The group's units first, bounded by its limit, so that a group at its limit never holds a global unit,
        // even for a moment, that another group could be refused for.
        let group = self.group.try_acquire_up_to(wanted, self.limit);
        if group == 0 {
            return false;
        }
        let global = self.global.try_acquire_up_to(group, u64::MAX);
        if global < group {
            self.group.release(group - global);
        }
        self.unused = global;
        global > 0
    }

    /// Returns the units the reservation has handed out.
    pub fn taken(&self) -> u64 {
        self.taken
    }

    /// Returns the units of both budgets the reservation holds and has not handed out, which dropping it returns.
    pub fn unused(&self) -> u64 {
        self.unused
    }
}

impl Drop for BudgetReservation<'_> {
    fn drop(&mut self) {
        // The global units first, as a holder releases what it removes, so that the group holds no global unit it is
        // giving back while another group could be refused for it.
        let unused = std::mem::take(&mut self.unused);
        if unused > 0 {
            self.global.release(unused);
            self.group.release(unused);
        }
    }
}

impl fmt::Debug for BudgetReservation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BudgetReservation")
            .field("limit", &self.limit)
            .field("taken", &self.taken)
            .field("unused", &self.unused)
            .field("first_block", &self.first_block)
            .field("max_block", &self.max_block)
            .finish()
    }
}

/// One operation on a [recorded][StateBudget::recorded] budget, with the units in use it observed.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetOperation {
    /// An acquisition of `units`, bounded by `bound`, the smaller of its limit and the capacity.
    ///
    /// An acquisition of as many units as fit ([`try_acquire_up_to`][StateBudget::try_acquire_up_to]) is recorded with
    /// the units it took, or, when not one fitted, as a refused acquisition of one unit. Either way, a granted
    /// acquisition observed at most `bound - units` units in use, and a refused one more.
    Acquire {
        /// The units asked for, or, for an acquisition of as many units as fit, the units it took.
        units: u64,

        /// The most units in use the acquisition allowed.
        bound: u64,

        /// The units in use when its compare-and-swap took them, or when it found that they did not fit.
        observed: u64,

        /// Whether it took the units.
        granted: bool,
    },

    /// A release of `units`.
    Release {
        /// The units released.
        units: u64,

        /// The units in use just before the release.
        observed: u64,
    },
}

/// The operations a [recorded][StateBudget::recorded] budget made, which clones of the budget share.
///
/// Operations are listed in the order they were recorded, just after each took effect: operations racing on several
/// threads may be listed in another order than the one their compare-and-swaps took effect in, but each records the
/// units in use it observed itself.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
pub struct BudgetRecording {
    operations: std::sync::Arc<std::sync::Mutex<Vec<BudgetOperation>>>,
}

#[cfg(any(test, feature = "test-util"))]
impl BudgetRecording {
    fn push(&self, operation: BudgetOperation) {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(operation);
    }

    /// Returns the operations recorded so far.
    pub fn operations(&self) -> Vec<BudgetOperation> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl fmt::Debug for StateBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("StateBudget");
        debug.field("used", &self.used());
        if self.is_unlimited() {
            debug.field("capacity", &"unlimited");
        } else {
            debug.field("capacity", &self.capacity());
        }
        debug.finish()
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use std::thread;

    use proptest::{collection::vec, prelude::*};

    use super::*;

    #[test]
    fn acquisitions_below_a_limit_fit_within_the_smaller_of_the_limit_and_the_capacity() {
        let budget = StateBudget::new(5);
        assert!(budget.try_acquire_below(2, 3));
        assert!(!budget.try_acquire_below(2, 3), "2 + 2 exceeds the limit of 3");
        assert_eq!(budget.used(), 2, "a refused acquisition changes nothing");
        assert!(budget.try_acquire_below(1, 3));
        assert!(!budget.try_acquire_below(1, 3));

        // A limit above the capacity is bounded by the capacity.
        assert!(budget.try_acquire_below(2, 100));
        assert!(!budget.try_acquire_below(1, 100));
        assert_eq!(budget.used(), 5);

        // A limit below the units in use refuses everything, and keeps what is held.
        budget.release(3);
        assert!(!budget.try_acquire_below(1, 1));
        assert_eq!(budget.used(), 2);

        // An unlimited budget is bounded by the limit alone.
        let unlimited = StateBudget::unlimited();
        assert!(unlimited.try_acquire_below(4, 4));
        assert!(!unlimited.try_acquire_below(1, 4));
        assert!(!unlimited.try_acquire_below(1, 0));
    }

    #[test]
    fn a_recorded_budget_records_each_operation_with_the_units_in_use_it_observed() {
        let (budget, recording) = StateBudget::recorded(3);
        let clone = budget.clone();
        assert!(budget.try_acquire_below(2, 5));
        assert!(!clone.try_acquire(2), "2 + 2 exceeds the capacity of 3");
        assert!(budget.try_acquire_below(1, 3));
        budget.release(1);
        assert!(
            !clone.try_acquire_below(1, 1),
            "2 in use is already over the limit of 1"
        );
        assert_eq!(
            recording.operations(),
            vec![
                BudgetOperation::Acquire {
                    units: 2,
                    bound: 3,
                    observed: 0,
                    granted: true
                },
                BudgetOperation::Acquire {
                    units: 2,
                    bound: 3,
                    observed: 2,
                    granted: false
                },
                BudgetOperation::Acquire {
                    units: 1,
                    bound: 3,
                    observed: 2,
                    granted: true
                },
                BudgetOperation::Release { units: 1, observed: 3 },
                BudgetOperation::Acquire {
                    units: 1,
                    bound: 1,
                    observed: 2,
                    granted: false
                },
            ]
        );
        // A budget created without a recording records nothing, and costs nothing to record.
        assert!(StateBudget::new(1).inner.recording.is_none());
    }

    #[test]
    fn concurrent_holders_never_exceed_a_limit_below_the_capacity() {
        const LIMIT: u64 = 100;
        const THREADS: usize = 8;
        let budget = StateBudget::new(1_000);

        let acquired: u64 = thread::scope(|scope| {
            let workers = (0..THREADS)
                .map(|_| {
                    let budget = budget.clone();
                    scope.spawn(move || (0..LIMIT).filter(|_| budget.try_acquire_below(1, LIMIT)).count() as u64)
                })
                .collect::<Vec<_>>();
            workers.into_iter().map(|worker| worker.join().unwrap()).sum()
        });

        assert_eq!(acquired, LIMIT);
        assert_eq!(budget.used(), LIMIT);
    }

    #[test]
    fn acquisitions_up_to_a_number_take_as_many_units_as_fit() {
        let (budget, recording) = StateBudget::recorded(5);
        assert_eq!(budget.try_acquire_up_to(2, 100), 2);
        assert_eq!(
            budget.try_acquire_up_to(4, 4),
            2,
            "two of the four fit below the limit of 4"
        );
        assert_eq!(budget.try_acquire_up_to(1, 4), 0, "none fits once the limit is reached");
        assert_eq!(budget.try_acquire_up_to(9, 100), 1, "one fits below the capacity of 5");
        assert_eq!(budget.try_acquire_up_to(0, 100), 0, "asking for nothing takes nothing");
        assert_eq!(budget.used(), 5);

        // A refusal is recorded as a refused acquisition of one unit, and asking for nothing is not recorded.
        let acquire = |units, bound, observed, granted| BudgetOperation::Acquire {
            units,
            bound,
            observed,
            granted,
        };
        assert_eq!(
            recording.operations(),
            vec![
                acquire(2, 5, 0, true),
                acquire(2, 4, 2, true),
                acquire(1, 4, 4, false),
                acquire(1, 5, 4, true),
            ]
        );
    }

    #[test]
    fn a_reservation_takes_blocks_as_large_as_what_it_handed_out_and_returns_the_rest() {
        let (group, group_ops) = StateBudget::recorded(u64::MAX);
        let (global, global_ops) = StateBudget::recorded(1_000);
        let mut reservation = BudgetReservation::new(&group, 100, &global);
        for _ in 0..5 {
            assert!(reservation.take());
        }
        // Blocks of 1, 1, 2, and 4: the last one handed out one unit and holds three.
        assert_eq!((reservation.taken(), reservation.unused()), (5, 3));
        assert_eq!((group.used(), global.used()), (8, 8));
        drop(reservation);
        assert_eq!(
            (group.used(), global.used()),
            (5, 5),
            "dropping it returns what it did not hand out"
        );

        let acquire = |units, bound, observed| BudgetOperation::Acquire {
            units,
            bound,
            observed,
            granted: true,
        };
        let returned = BudgetOperation::Release { units: 3, observed: 8 };
        assert_eq!(
            group_ops.operations(),
            vec![
                acquire(1, 100, 0),
                acquire(1, 100, 1),
                acquire(2, 100, 2),
                acquire(4, 100, 4),
                returned
            ]
        );
        assert_eq!(
            global_ops.operations(),
            vec![
                acquire(1, 1_000, 0),
                acquire(1, 1_000, 1),
                acquire(2, 1_000, 2),
                acquire(4, 1_000, 4),
                returned
            ]
        );

        // Blocks grow to `MAX_BLOCK` and stay there: 200 units take blocks of 1, 1, 2, 4, 8, 16, 32, then 64, 64, and
        // 64, of which the last hands out 8.
        let before = group_ops.operations().len();
        let mut reservation = BudgetReservation::new(&group, u64::MAX, &global);
        for _ in 0..200 {
            assert!(reservation.take());
        }
        assert_eq!(group_ops.operations().len() - before, 10);
        assert_eq!(reservation.unused(), 56);
        drop(reservation);
        assert_eq!((group.used(), global.used()), (205, 205));
    }

    #[test]
    fn a_reservation_gives_the_groups_units_back_when_the_global_budget_has_less_room() {
        // Room for three units in all: the third block asks for two units of the group, of which the global budget
        // has room for one, and the group's other unit is given back at once. The next block finds the global budget
        // full, and gives back all three units of the group it took.
        let (group, group_ops) = StateBudget::recorded(u64::MAX);
        let global = StateBudget::new(3);
        let mut reservation = BudgetReservation::new(&group, 10, &global);
        for _ in 0..3 {
            assert!(reservation.take());
        }
        assert!(!reservation.take(), "the global budget is full");
        assert_eq!((reservation.taken(), reservation.unused()), (3, 0));
        assert_eq!((group.used(), global.used()), (3, 3));
        assert_eq!(
            group_ops.operations()[2..],
            [
                BudgetOperation::Acquire {
                    units: 2,
                    bound: 10,
                    observed: 2,
                    granted: true
                },
                BudgetOperation::Release { units: 1, observed: 4 },
                BudgetOperation::Acquire {
                    units: 3,
                    bound: 10,
                    observed: 3,
                    granted: true
                },
                BudgetOperation::Release { units: 3, observed: 6 },
            ]
        );
        drop(reservation);
        assert_eq!((group.used(), global.used()), (3, 3));
    }

    #[test]
    fn units_sitting_in_a_reservation_can_refuse_another_holder_until_they_are_returned() {
        // A group's limit of 4: one holder's third unit takes a block of two, and holds one it has not handed out,
        // which refuses another holder of the group until the first reservation is dropped.
        let (group, global) = (StateBudget::unlimited(), StateBudget::new(100));
        let mut first = BudgetReservation::new(&group, 4, &global);
        assert!(first.take() && first.take() && first.take());
        assert_eq!(first.unused(), 1);

        let mut second = BudgetReservation::new(&group, 4, &global);
        assert!(!second.take(), "the group's last unit sits in the first reservation");
        drop(first);
        assert!(second.take(), "the first reservation returned it");
        assert!(!second.take());
        drop(second);
        assert_eq!((group.used(), global.used()), (4, 4));

        // A group at its limit takes no global unit, and an unlimited budget is bounded by the limit alone.
        let mut third = BudgetReservation::new(&group, 4, &global);
        assert!(!third.take());
        drop(third);
        assert_eq!(global.used(), 4);
    }

    #[test]
    fn a_reservation_whose_largest_block_is_one_unit_takes_each_unit_as_it_hands_it_out() {
        // Each unit is one acquisition of each budget, and none is ever held unused: a holder's refusals are those of
        // unit-at-a-time acquisition even while another holder's reservation is live, and dropping returns nothing.
        let (group, group_ops) = StateBudget::recorded(u64::MAX);
        let global = StateBudget::new(3);
        let mut first = BudgetReservation::new(&group, 10, &global).with_max_block(1);
        let mut second = BudgetReservation::new(&group, 10, &global).with_max_block(0);
        assert!(first.take() && first.take());
        assert_eq!((first.unused(), group.used(), global.used()), (0, 2, 2));
        assert!(second.take(), "the global budget's third unit sits in no reservation");
        assert!(!first.take() && !second.take());
        assert_eq!((second.unused(), group.used(), global.used()), (0, 3, 3));
        drop((first, second));
        assert_eq!((group.used(), global.used()), (3, 3));

        let acquire = |observed, granted| BudgetOperation::Acquire {
            units: 1,
            bound: 10,
            observed,
            granted,
        };
        let give_back = |observed| BudgetOperation::Release { units: 1, observed };
        assert_eq!(
            group_ops.operations(),
            vec![
                acquire(0, true),
                acquire(1, true),
                acquire(2, true),
                acquire(3, true),
                give_back(4),
                acquire(3, true),
                give_back(4),
            ],
            "blocks of one unit, and the group's unit given back when the global budget was full"
        );

        // A larger cap is capped at `MAX_BLOCK`.
        let global = StateBudget::unlimited();
        let mut reservation = BudgetReservation::new(&group, u64::MAX, &global).with_max_block(1_000);
        for _ in 0..200 {
            assert!(reservation.take());
        }
        assert_eq!(reservation.unused(), 56, "blocks of 1, 1, 2, ..., 32, then three of 64");
    }

    #[test]
    fn a_reservation_with_a_first_block_starts_its_blocks_at_that_size() {
        // A first block of three: blocks of 3, 3, and then 6, as large as what was handed out, of which the batch's
        // tenth unit leaves two unused.
        let (group, group_ops) = StateBudget::recorded(u64::MAX);
        let global = StateBudget::unlimited();
        let mut reservation = BudgetReservation::new(&group, u64::MAX, &global).with_first_block(3);
        assert!(reservation.take());
        assert_eq!(
            (reservation.taken(), reservation.unused()),
            (1, 2),
            "the first block holds more than was handed out"
        );
        for _ in 1..10 {
            assert!(reservation.take());
        }
        assert_eq!((reservation.taken(), reservation.unused()), (10, 2));
        drop(reservation);
        assert_eq!(group.used(), 10);
        let units = group_ops
            .operations()
            .iter()
            .map(|operation| match operation {
                BudgetOperation::Acquire { units, .. } => *units as i64,
                BudgetOperation::Release { units, .. } => -(*units as i64),
            })
            .collect::<Vec<_>>();
        assert_eq!(units, vec![3, 3, 6, -2]);

        // A first block beyond the largest is the largest, and one of none is one unit.
        let mut reservation = BudgetReservation::new(&group, u64::MAX, &global)
            .with_first_block(1_000)
            .with_max_block(4);
        assert!(reservation.take());
        assert_eq!(reservation.unused(), 3);
        drop(reservation);
        let mut reservation = BudgetReservation::new(&group, u64::MAX, &global).with_first_block(0);
        assert!(reservation.take());
        assert_eq!(reservation.unused(), 0);
        drop(reservation);
        assert_eq!(group.used(), 12);
    }

    /// Returns the most blocks a reservation whose blocks start at `first` units and grow up to `max` takes to hand out
    /// `units` units: each block as large as what it handed out before it, at least `first`, at most `max`, the last
    /// one perhaps only partly granted.
    fn most_blocks_for(units: u64, first: u64, max: u64) -> u64 {
        let (mut handed_out, mut blocks) = (0, 0);
        while handed_out < units {
            handed_out += handed_out.max(first).clamp(1, max);
            blocks += 1;
        }
        blocks
    }

    #[test]
    fn the_most_blocks_for_a_number_of_units_follow_the_ramp() {
        let max = BudgetReservation::MAX_BLOCK;
        // From one unit: 1, 1, 2, 4, ..., 32, then blocks of 64.
        assert_eq!(
            [0, 1, 2, 3, 4, 5, 64, 65, 128, 129, 256, 1_024].map(|units| most_blocks_for(units, 1, max)),
            [0, 1, 2, 3, 3, 4, 7, 8, 8, 9, 10, 22]
        );
        // From 64: one block per 64 units.
        assert_eq!(
            [1, 64, 65, 256, 1_024].map(|units| most_blocks_for(units, max, max)),
            [1, 1, 2, 4, 16]
        );
        // One unit per block.
        assert_eq!(most_blocks_for(5, 1, 1), 5);
    }

    /// One operation on budgets that two groups share: a batch of a group's holder that takes `units` units, one at a
    /// time, below `limit`, or a release of up to `units` that a group's holders took.
    #[derive(Clone, Debug)]
    enum GroupOp {
        Batch { group: usize, units: u64, limit: u64 },
        Release { group: usize, units: u64 },
    }

    fn arb_group_ops() -> impl Strategy<Value = Vec<GroupOp>> {
        let batch =
            (0usize..2, 0u64..150, 0u64..200).prop_map(|(group, units, limit)| GroupOp::Batch { group, units, limit });
        let release = (0usize..2, 0u64..150).prop_map(|(group, units)| GroupOp::Release { group, units });
        let op = prop_oneof![3 => batch, 1 => release];
        vec(op, 1..32)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn property_test_a_reservation_alone_takes_exactly_what_unit_acquisitions_take(
            capacity in proptest::option::of(0u64..300),
            one_group_budget in any::<bool>(),
            first_block in 0u64..100,
            max_block in 0u64..80,
            ops in arb_group_ops(),
        ) {
            // Holders of two groups share a global budget, and either one group budget or one each. Each batch's holder
            // takes its units through a reservation whose blocks start at `first_block` and grow up to `max_block`,
            // dropped before the next batch: as a model of unit-at-a-time acquisition, a unit is taken exactly while
            // the group's count is below the limit and the global budget has room. Once the reservation is dropped,
            // the budgets count exactly the units taken; while it lives, it holds fewer units unhanded than its
            // largest block, and than what it handed out unless its first block is larger, and none with blocks of one
            // unit; and it takes at most the blocks that unit count needs, one acquisition of each budget per block.
            let (first, max) = (first_block.max(1).min(max_block.clamp(1, 64)), max_block.clamp(1, 64));
            let (global, global_ops) = StateBudget::recorded(capacity.unwrap_or(u64::MAX));
            let shared = StateBudget::unlimited();
            let groups = if one_group_budget {
                [shared.clone(), shared]
            } else {
                [StateBudget::unlimited(), StateBudget::unlimited()]
            };
            let mut held = [0u64; 2];
            for op in ops {
                match op {
                    GroupOp::Batch { group, units, limit } => {
                        let counted = |held: &[u64; 2]| if one_group_budget { held[0] + held[1] } else { held[group] };
                        let acquisitions = global_ops.operations().len();
                        let mut reservation = BudgetReservation::new(&groups[group], limit, &global)
                            .with_first_block(first_block)
                            .with_max_block(max_block);
                        for _ in 0..units {
                            let fits = counted(&held) < limit && held[0] + held[1] < global.capacity();
                            prop_assert_eq!(reservation.take(), fits);
                            if fits {
                                held[group] += 1;
                            }
                            prop_assert!(reservation.unused() < max);
                            prop_assert!(reservation.unused() < reservation.taken().max(first));
                        }
                        let taken = reservation.taken();
                        drop(reservation);
                        let blocks = global_ops.operations()[acquisitions..]
                            .iter()
                            .filter(|op| matches!(op, BudgetOperation::Acquire { granted: true, .. }))
                            .count() as u64;
                        prop_assert!(
                            blocks <= most_blocks_for(taken, first, max),
                            "{} blocks for {} units",
                            blocks,
                            taken
                        );
                    }
                    GroupOp::Release { group, units } => {
                        let units = units.min(held[group]);
                        global.release(units);
                        groups[group].release(units);
                        held[group] -= units;
                    }
                }
                prop_assert_eq!(global.used(), held[0] + held[1]);
                if one_group_budget {
                    prop_assert_eq!(groups[0].used(), held[0] + held[1]);
                } else {
                    prop_assert_eq!((groups[0].used(), groups[1].used()), (held[0], held[1]));
                }
            }
        }
    }

    /// Runs one holder of a group: in each round, a batch takes `sizes[round]` units through a reservation whose blocks
    /// start at `first` units and grow up to `max`, checking after every unit that neither the group's limit nor the
    /// global capacity is exceeded, and then every other round releases what the holder holds, as a flush removes
    /// state. Returns the units the holder still holds.
    fn run_holder(
        group: &StateBudget, limit: u64, global: &StateBudget, sizes: &[u64], (first, max): (u64, u64),
    ) -> Result<u64, String> {
        let mut held = 0;
        for (round, size) in sizes.iter().enumerate() {
            let mut reservation = BudgetReservation::new(group, limit, global)
                .with_first_block(first)
                .with_max_block(max);
            for _ in 0..*size {
                if reservation.take() {
                    held += 1;
                }
                if group.used() > limit {
                    return Err(format!(
                        "the group counts {} units, over its limit of {}",
                        group.used(),
                        limit
                    ));
                }
                if global.used() > global.capacity() {
                    return Err(format!("the global budget counts {}, over its capacity", global.used()));
                }
            }
            let max = max.clamp(1, BudgetReservation::MAX_BLOCK);
            if reservation.unused() >= max || reservation.unused() >= reservation.taken().max(first.clamp(1, max)) {
                return Err(format!("{:?} holds too many units it did not hand out", reservation));
            }
            drop(reservation);
            if round % 2 == 1 {
                global.release(held);
                group.release(held);
                held = 0;
            }
        }
        Ok(held)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn property_test_reservations_never_exceed_the_limit_and_return_every_unit(
            holders in 2usize..5,
            limit in 1u64..300,
            spare in 0u64..300,
            sizes in vec(vec(0u64..150, 40), 5),
            blocks in vec((prop_oneof![Just(1u64), 0u64..100], prop_oneof![Just(1u64), Just(64u64), 0u64..80]), 5),
        ) {
            // Holders on threads of their own take units of one group at once, through reservations, under one limit,
            // while a holder of another group shares the global budget, which leaves `spare` units beyond the limit;
            // each holder's blocks start and grow as `blocks` says, one unit at a time for some. However their
            // reservations interleave, the group never counts more than its limit, nor the global budget more than its
            // capacity; and once every reservation is dropped, the budgets count exactly the units the holders took and
            // kept.
            let global = StateBudget::new(limit + spare);
            let (group, other) = (StateBudget::unlimited(), StateBudget::unlimited());
            let start = std::sync::Barrier::new(holders + 1);
            let outcomes = thread::scope(|scope| {
                let mut workers = (0..holders)
                    .map(|holder| {
                        let (global, group, start, sizes) = (&global, &group, &start, &sizes[holder]);
                        let blocks = blocks[holder];
                        scope.spawn(move || {
                            start.wait();
                            run_holder(group, limit, global, sizes, blocks)
                        })
                    })
                    .collect::<Vec<_>>();
                let (global, other, start, sizes) = (&global, &other, &start, &sizes[holders]);
                let blocks = blocks[holders];
                workers.push(scope.spawn(move || {
                    start.wait();
                    run_holder(other, u64::MAX, global, sizes, blocks)
                }));
                workers.into_iter().map(|worker| worker.join().expect("a holder panicked")).collect::<Vec<_>>()
            });
            let mut held = Vec::new();
            for outcome in outcomes {
                prop_assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
                held.push(outcome.unwrap());
            }
            let held_by_group = held[..holders].iter().sum::<u64>();
            prop_assert_eq!(
                (group.used(), other.used(), global.used()),
                (held_by_group, held[holders], held_by_group + held[holders])
            );
        }
    }

    /// One operation on a budget: an acquisition of `units` below `limit`, or a release of up to `units` held.
    #[derive(Clone, Debug)]
    enum Op {
        Acquire { units: u64, limit: u64 },
        Release { units: u64 },
    }

    fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            3 => (0u64..4, 0u64..12).prop_map(|(units, limit)| Op::Acquire { units, limit }),
            1 => (0u64..4).prop_map(|units| Op::Release { units }),
        ];
        vec(op, 1..64)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn property_test_acquisitions_below_a_limit_take_units_exactly_while_they_fit(
            capacity in proptest::option::of(0u64..10),
            ops in arb_ops(),
        ) {
            // A model of the count: an acquisition below a limit succeeds exactly when the units in use plus the units
            // asked for are at most the smaller of the limit and the capacity, and only a success changes the count.
            let budget = capacity.map_or_else(StateBudget::unlimited, StateBudget::new);
            let capacity = budget.capacity();
            let mut used = 0u64;
            for op in ops {
                match op {
                    Op::Acquire { units, limit } => {
                        let fits = used + units <= limit.min(capacity);
                        prop_assert_eq!(budget.try_acquire_below(units, limit), fits);
                        if fits {
                            used += units;
                        }
                    }
                    Op::Release { units } => {
                        let units = units.min(used);
                        budget.release(units);
                        used -= units;
                    }
                }
                prop_assert_eq!(budget.used(), used);
            }
        }
    }

    #[test]
    fn acquisitions_fit_within_the_capacity() {
        let budget = StateBudget::new(3);
        assert_eq!((budget.used(), budget.capacity()), (0, 3));

        assert!(budget.try_acquire(2));
        assert!(!budget.try_acquire(2), "2 + 2 exceeds the capacity of 3");
        assert_eq!(budget.used(), 2, "a refused acquisition changes nothing");
        assert!(budget.try_acquire(1));
        assert!(!budget.try_acquire(1));

        budget.release(2);
        assert_eq!(budget.used(), 1);
        assert!(budget.try_acquire(2));
        assert_eq!(budget.used(), 3);

        // Acquiring nothing always fits, even when the budget is full.
        assert!(budget.try_acquire(0));
    }

    #[test]
    fn a_zero_capacity_refuses_everything_and_unlimited_refuses_nothing() {
        let none = StateBudget::new(0);
        assert!(!none.try_acquire(1));
        assert!(!none.is_unlimited());

        let unlimited = StateBudget::unlimited();
        assert!(unlimited.is_unlimited());
        assert!(unlimited.try_acquire(u64::MAX - 1));
        assert!(unlimited.try_acquire(1));
        // The count cannot overflow: a total beyond the representable range does not fit.
        assert!(!unlimited.try_acquire(1));
        assert_eq!(unlimited.used(), u64::MAX);
    }

    #[test]
    fn clones_share_one_count() {
        let budget = StateBudget::new(2);
        let clone = budget.clone();
        assert!(budget.shares_with(&clone));
        assert!(!budget.shares_with(&StateBudget::new(2)));

        assert!(clone.try_acquire(2));
        assert!(!budget.try_acquire(1));
        clone.release(1);
        assert_eq!(budget.used(), 1);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "released 2 state units but only 1 were in use")]
    fn releasing_more_than_is_in_use_is_a_bug() {
        let budget = StateBudget::new(4);
        assert!(budget.try_acquire(1));
        budget.release(2);
    }

    #[test]
    fn concurrent_holders_never_exceed_the_capacity() {
        const CAPACITY: u64 = 1_000;
        const THREADS: usize = 8;
        let budget = StateBudget::new(CAPACITY);

        let acquired: u64 = thread::scope(|scope| {
            let workers = (0..THREADS)
                .map(|_| {
                    let budget = budget.clone();
                    scope.spawn(move || (0..CAPACITY).filter(|_| budget.try_acquire(1)).count() as u64)
                })
                .collect::<Vec<_>>();
            workers.into_iter().map(|worker| worker.join().unwrap()).sum()
        });

        assert_eq!(acquired, CAPACITY);
        assert_eq!(budget.used(), CAPACITY);
    }

    #[test]
    fn debug_output_names_the_capacity() {
        let budget = StateBudget::new(5);
        assert!(budget.try_acquire(2));
        assert_eq!(format!("{:?}", budget), "StateBudget { used: 2, capacity: 5 }");
        assert_eq!(
            format!("{:?}", StateBudget::unlimited()),
            "StateBudget { used: 0, capacity: \"unlimited\" }"
        );
    }
}

// Loom models of the context limit's admission across lanes: several tasks, each holding instances of one state
// owner's state, admit batches of new contexts against the node's count and the global budget they share.
//
// The models run this module's own code, whose count is a `loom` atomic under the feature: aggregation admits a batch's
// new contexts through one `BudgetReservation` of the node's and the global budget (`ContextLimit::try_admit` in
// `saluki-metric-kernels` calls `BudgetReservation::take`, and the batch drops the reservation when it ends), one unit
// per block for a state owner on one lane and in growing blocks for one on several, which `admit_reserved` runs as it
// does. The other lane functions transcribe protocols the models reject: the check-then-act that the bounded node
// acquire replaced, a node unit kept on a global refusal, a reservation that keeps the units it did not hand out, one
// that keeps the node's units the global budget had no room for, and blocks where one unit at a time is due.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{sync::Arc, thread};

    use super::{BudgetReservation, StateBudget};

    /// What one lane's batch did: the new contexts it admitted, and the units its reservation returned at the end,
    /// unused.
    #[derive(Clone, Copy)]
    struct Batch {
        admitted: u64,
        returned: u64,
    }

    /// How a lane's reservation sizes its blocks: its first block's size, and its largest.
    #[derive(Clone, Copy)]
    struct Blocks {
        first: u64,
        max: u64,
    }

    /// The blocks of a lane of a state owner on several lanes whose previous batch admitted nothing: from one unit up
    /// to `MAX_BLOCK`.
    const GROWING: Blocks = Blocks {
        first: 1,
        max: BudgetReservation::MAX_BLOCK,
    };

    /// A lane's protocol: admits a batch of `contexts` new contexts of a node whose context limit is `limit`, sizing
    /// its blocks as `blocks` says if it reserves units, and, if `checked`, checks the budgets with [`check`] after
    /// each one.
    type Admit = fn(&StateBudget, &StateBudget, u64, u64, Blocks, bool) -> Batch;

    /// Checks, at a point a lane observes, that its node is within its limit and the global budget within its capacity.
    fn check(global: &StateBudget, node: &StateBudget, limit: u64) {
        assert!(node.used() <= limit, "violated: node A counts {} contexts", node.used());
        assert!(
            global.used() <= global.capacity(),
            "violated: the global budget is over its capacity"
        );
    }

    /// Admits a batch as aggregation does: one reservation of node and global units for the whole batch, a unit of
    /// each taken per new context, the reservation dropped when the batch ends.
    fn admit_reserved(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, blocks: Blocks, checked: bool,
    ) -> Batch {
        let mut reservation = BudgetReservation::new(node, limit, global)
            .with_first_block(blocks.first)
            .with_max_block(blocks.max);
        let mut admitted = 0;
        for _ in 0..contexts {
            admitted += u64::from(reservation.take());
            if checked {
                check(global, node, limit);
            }
        }
        let returned = reservation.unused();
        drop(reservation);
        Batch { admitted, returned }
    }

    /// The protocol the bounded node acquire replaced, one context at a time: a check of the node's count, then the
    /// global unit, then the node's, whose budget is unlimited.
    fn admit_check_then_act(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, _: Blocks, checked: bool,
    ) -> Batch {
        let mut admitted = 0;
        for _ in 0..contexts {
            if node.used() < limit && global.try_acquire(1) {
                if node.try_acquire(1) {
                    admitted += 1;
                } else {
                    global.release(1);
                }
            }
            if checked {
                check(global, node, limit);
            }
        }
        Batch { admitted, returned: 0 }
    }

    /// The bounded node acquire one context at a time, except that a global refusal keeps the node's unit.
    fn admit_node_first_keeping_its_unit(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, _: Blocks, checked: bool,
    ) -> Batch {
        let mut admitted = 0;
        for _ in 0..contexts {
            admitted += u64::from(node.try_acquire_below(1, limit) && global.try_acquire(1));
            if checked {
                check(global, node, limit);
            }
        }
        Batch { admitted, returned: 0 }
    }

    /// Admits a batch through a reservation that is never dropped, so the units it did not hand out are never
    /// returned.
    fn admit_reserved_keeping_the_unused(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, blocks: Blocks, checked: bool,
    ) -> Batch {
        let mut reservation = BudgetReservation::new(node, limit, global)
            .with_first_block(blocks.first)
            .with_max_block(blocks.max);
        let mut admitted = 0;
        for _ in 0..contexts {
            admitted += u64::from(reservation.take());
            if checked {
                check(global, node, limit);
            }
        }
        std::mem::forget(reservation);
        Batch { admitted, returned: 0 }
    }

    /// Admits a batch as `BudgetReservation` does with growing blocks, transcribed, except that a block keeps the
    /// node's units that the global budget had no room for.
    fn admit_reserved_keeping_the_nodes_excess(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, _: Blocks, checked: bool,
    ) -> Batch {
        let (mut unused, mut taken) = (0, 0);
        for _ in 0..contexts {
            if unused == 0 {
                let from_node = node.try_acquire_up_to(taken.clamp(1, BudgetReservation::MAX_BLOCK), limit);
                if from_node > 0 {
                    unused = global.try_acquire_up_to(from_node, u64::MAX);
                }
            }
            if unused > 0 {
                unused -= 1;
                taken += 1;
            }
            if checked {
                check(global, node, limit);
            }
        }
        global.release(unused);
        node.release(unused);
        Batch {
            admitted: taken,
            returned: unused,
        }
    }

    /// Admits a batch through a reservation whose first block is two units, whatever blocks the lane is due: a state
    /// owner on one lane that reserved units in blocks.
    fn admit_reserved_in_blocks_regardless(
        global: &StateBudget, node: &StateBudget, limit: u64, contexts: u64, _: Blocks, checked: bool,
    ) -> Batch {
        let blocks = Blocks {
            first: 2,
            max: BudgetReservation::MAX_BLOCK,
        };
        admit_reserved(global, node, limit, contexts, blocks, checked)
    }

    /// A configuration of the model: lanes of node A, whose context limit is `limit_a`, each admit one batch of new
    /// contexts of `lanes_a[i]` contexts, while one more admits a batch of `contexts_b` contexts of node B, unlimited,
    /// all against a global budget of `capacity`, every lane sizing its blocks as `blocks` says.
    #[derive(Clone, Copy)]
    struct Config {
        lanes_a: &'static [u64],
        limit_a: u64,
        contexts_b: u64,
        capacity: u64,
        blocks: Blocks,
        /// The most times an explored execution preempts a thread, or `None` to explore every interleaving.
        preemption_bound: Option<usize>,
    }

    /// Room for one context of each node: the global budget never refuses, and three lanes race for node A's unit.
    ///
    /// Four threads of compare-and-swap loops interleave in too many ways to explore every one, so the model bounds
    /// how often an execution preempts a thread, as `loom` recommends: every interleaving with at most three
    /// preemption points is explored, which covers each pair of lanes racing on either budget.
    const ROOM_FOR_ONE_OF_EACH: Config = Config {
        lanes_a: &[1, 1, 1],
        limit_a: 1,
        contexts_b: 1,
        capacity: 2,
        blocks: GROWING,
        preemption_bound: Some(3),
    };

    /// Room for one context in all: node B and two lanes of node A race for the one global unit, so a lane of node A
    /// that took its node's unit can find the global budget full, and must give its node's unit back. Every
    /// interleaving is explored.
    const ROOM_FOR_ONE_IN_ALL: Config = Config {
        lanes_a: &[1, 1],
        limit_a: 1,
        contexts_b: 1,
        capacity: 1,
        blocks: GROWING,
        preemption_bound: None,
    };

    /// A block left over: a lane of node A admits three contexts, whose third takes a block of two units, and holds
    /// the second until its batch ends, while another lane of node A, which the left-over unit can refuse, and node B
    /// race with it for node A's limit of four and the global capacity of five.
    ///
    /// Three threads of reservations interleave in too many ways to explore every one, so, as for
    /// [`ROOM_FOR_ONE_OF_EACH`], every interleaving with at most three preemption points is explored.
    const A_BLOCK_LEFT_OVER: Config = Config {
        lanes_a: &[3, 1],
        limit_a: 4,
        contexts_b: 1,
        capacity: 5,
        blocks: GROWING,
        preemption_bound: Some(3),
    };

    /// A block the global budget has less room for: a lane of node A admits three contexts, whose third asks for two
    /// units of the node when the global budget, which node B races for, has room for one or none, so the lane gives
    /// back the node's units without global units at once. Every interleaving is explored.
    const A_PARTIAL_GLOBAL_GRANT: Config = Config {
        lanes_a: &[3],
        limit_a: 10,
        contexts_b: 1,
        capacity: 3,
        blocks: GROWING,
        preemption_bound: None,
    };

    /// Room for every context, two of node A, from two lanes, and one of node B, with blocks of one unit, as state
    /// owners on one lane take them: no unit ever sits in a reservation, so every context is admitted, however the
    /// lanes interleave.
    ///
    /// Three threads whose every acquisition succeeds interleave in too many ways to explore every one, so, as for
    /// [`ROOM_FOR_ONE_OF_EACH`], every interleaving with at most three preemption points is explored.
    const ONE_UNIT_AT_A_TIME: Config = Config {
        lanes_a: &[1, 1],
        limit_a: 2,
        contexts_b: 1,
        capacity: 3,
        blocks: Blocks { first: 1, max: 1 },
        preemption_bound: Some(3),
    };

    /// The same room, with first blocks of two units, as a lane whose previous batch admitted two new contexts takes
    /// them: a lane of node A that takes both of node A's units for its one context holds the second until its batch
    /// ends, which can refuse the other lane of node A, and a block of node B can find one global unit left. As for
    /// [`ONE_UNIT_AT_A_TIME`], every interleaving with at most three preemption points is explored.
    const A_FIRST_BLOCK_LEFT_OVER: Config = Config {
        blocks: Blocks {
            first: 2,
            max: BudgetReservation::MAX_BLOCK,
        },
        ..ONE_UNIT_AT_A_TIME
    };

    /// Runs the lanes of `config`, each admitting its batch with `admit`, at every interleaving `loom` explores, and
    /// checks the budgets' properties:
    ///
    /// - never above the node's limit or the capacity, at any point a lane can observe;
    /// - every unit reserved and not used returned, and the node's units the global budget had no room for given back,
    ///   so that once every batch ended, every unit held is a context held;
    /// - a context refused only while the budgets were full of units that admitted contexts hold or that a reservation
    ///   returned unused at the end of its batch, the capacity behavior of reservations: so with no unit returned,
    ///   every context that fits is admitted, and node B is refused only when the global budget is full of admitted
    ///   contexts;
    /// - with blocks of one unit, every context that fits admitted: no refusal for capacity behavior at all.
    fn model(admit: Admit, config: Config) {
        let Config {
            lanes_a,
            limit_a,
            contexts_b,
            capacity,
            blocks,
            preemption_bound,
        } = config;
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = preemption_bound;
        builder.check(move || {
            let global = Arc::new(StateBudget::new(capacity));
            let node_a = Arc::new(StateBudget::unlimited());
            let node_b = StateBudget::unlimited();

            let lanes = lanes_a
                .iter()
                .map(|&contexts| {
                    let (global, node_a) = (Arc::clone(&global), Arc::clone(&node_a));
                    thread::spawn(move || admit(&global, &node_a, limit_a, contexts, blocks, true))
                })
                .collect::<Vec<_>>();
            // Node B's lane checks nothing: every lane of node A checks the global budget, and each load would multiply
            // the interleavings to explore.
            let batch_b = admit(&global, &node_b, u64::MAX, contexts_b, blocks, false);
            let batches_a = lanes
                .into_iter()
                .map(|lane| lane.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .collect::<Vec<_>>();
            let admitted_a = batches_a.iter().map(|batch| batch.admitted).sum::<u64>();
            let admitted = admitted_a + batch_b.admitted;
            let returned = batches_a.iter().map(|batch| batch.returned).sum::<u64>() + batch_b.returned;

            // Every unit held is a context held: reservations returned what they did not hand out, and refusals gave
            // back whatever they took.
            assert_eq!(
                (node_a.used(), node_b.used(), global.used()),
                (admitted_a, batch_b.admitted, admitted),
                "violated: the budgets count units no context holds"
            );
            // A refusal is justified by units admitted contexts hold, or units a reservation held and returned.
            let demand_a = lanes_a.iter().sum::<u64>();
            let fits = capacity.min(limit_a.min(demand_a) + contexts_b);
            assert!(
                admitted + returned >= fits,
                "violated: a context that fits was refused, beyond the units reservations returned"
            );
            assert!(
                batch_b.admitted == contexts_b || admitted + returned >= capacity,
                "violated: node B was refused while the global budget had room"
            );
            if blocks.max == 1 {
                assert_eq!(
                    admitted, fits,
                    "violated: a context that fits was refused while no unit could sit in a reservation"
                );
            }
        });
    }

    #[test]
    fn the_bounded_node_acquire_first_keeps_every_limit_at_any_interleaving() {
        model(admit_reserved, ROOM_FOR_ONE_OF_EACH);
    }

    #[test]
    fn the_bounded_node_acquire_first_gives_the_nodes_unit_back_when_the_global_budget_is_full() {
        model(admit_reserved, ROOM_FOR_ONE_IN_ALL);
    }

    #[test]
    fn a_reservation_keeps_every_limit_and_returns_its_left_over_units_at_any_interleaving() {
        model(admit_reserved, A_BLOCK_LEFT_OVER);
    }

    #[test]
    fn a_reservation_gives_back_the_nodes_units_the_global_budget_has_no_room_for() {
        model(admit_reserved, A_PARTIAL_GLOBAL_GRANT);
    }

    #[test]
    fn reservations_of_one_unit_per_block_admit_every_context_that_fits_at_any_interleaving() {
        model(admit_reserved, ONE_UNIT_AT_A_TIME);
    }

    #[test]
    fn a_first_block_larger_than_the_batch_keeps_every_limit_and_returns_what_it_did_not_use() {
        model(admit_reserved, A_FIRST_BLOCK_LEFT_OVER);
    }

    // Checking the node's count and acquiring afterwards lets several lanes pass the check at once: loom finds an
    // interleaving where node A ends above its limit, or where node B is refused while node A's extra lane holds a
    // global unit.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn checking_the_node_then_acquiring_exceeds_the_limit() {
        model(admit_check_then_act, ROOM_FOR_ONE_OF_EACH);
    }

    // A lane of node A that takes its node's unit and then finds the global budget full, because node B's context
    // took the last unit, keeps a unit no context holds: loom finds that interleaving.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn keeping_the_nodes_unit_on_a_global_refusal_counts_a_context_never_admitted() {
        model(admit_node_first_keeping_its_unit, ROOM_FOR_ONE_IN_ALL);
    }

    // A reservation that keeps the unit left over in its last block counts a context never admitted, and refuses the
    // other lane of node A for it after the batch ended.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn keeping_a_reservations_left_over_units_counts_contexts_never_admitted() {
        model(admit_reserved_keeping_the_unused, A_BLOCK_LEFT_OVER);
    }

    // A block that keeps the node's units the global budget had no room for counts contexts never admitted.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn keeping_the_nodes_units_beyond_the_global_grant_counts_contexts_never_admitted() {
        model(admit_reserved_keeping_the_nodes_excess, A_PARTIAL_GLOBAL_GRANT);
    }

    // Lanes that reserve a block of two units where one unit at a time is due refuse the other lane of node A for a
    // unit their block holds and no context uses: loom finds a context that fits refused.
    #[test]
    #[should_panic(expected = "violated: a context that fits was refused while no unit could sit in a reservation")]
    fn reserving_in_blocks_where_one_unit_at_a_time_is_due_refuses_contexts_that_fit() {
        model(admit_reserved_in_blocks_regardless, ONE_UNIT_AT_A_TIME);
    }
}

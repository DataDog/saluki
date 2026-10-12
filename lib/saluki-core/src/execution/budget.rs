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
/// lock. A budget made with [`unlimited`][Self::unlimited] never refuses, and still counts what is in use.
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

/// One operation on a [recorded][StateBudget::recorded] budget, with the units in use it observed.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetOperation {
    /// An acquisition of `units`, bounded by `bound`, the smaller of its limit and the capacity.
    Acquire {
        /// The units asked for.
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
// owner's state, admit new contexts against the node's count and the global budget they share.
//
// The models run this module's own `StateBudget` code, whose count is a `loom` atomic under the feature. The admission
// protocol itself lives in the aggregation kernel (`ContextLimit::try_admit` in `saluki-metric-kernels`), which has no
// `loom` feature: `admit_node_first` transcribes it, and must stay in lockstep with it. `admit_check_then_act`
// transcribes the protocol it replaced, which checked the node's count before acquiring anything, and
// `admit_node_first_keeping_its_unit` one that forgets to give the node's unit back: the models reject both.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{sync::Arc, thread};

    use super::StateBudget;

    /// Admits one new context of a node with the context limit `limit`: takes the node's unit first, bounded by the
    /// limit, then the global unit, giving the node's back if the global budget is full.
    fn admit_node_first(global: &StateBudget, node: &StateBudget, limit: u64) -> bool {
        if !node.try_acquire_below(1, limit) {
            return false;
        }
        if !global.try_acquire(1) {
            node.release(1);
            return false;
        }
        true
    }

    /// The protocol `admit_node_first` replaced: a check of the node's count, then the global unit, then the node's,
    /// whose budget is unlimited.
    fn admit_check_then_act(global: &StateBudget, node: &StateBudget, limit: u64) -> bool {
        if node.used() >= limit || !global.try_acquire(1) {
            return false;
        }
        if !node.try_acquire(1) {
            global.release(1);
            return false;
        }
        true
    }

    /// The protocol `admit_node_first` follows, except that a global refusal keeps the node's unit.
    fn admit_node_first_keeping_its_unit(global: &StateBudget, node: &StateBudget, limit: u64) -> bool {
        node.try_acquire_below(1, limit) && global.try_acquire(1)
    }

    /// A configuration of the model: `lanes_a` lanes each admit a new context of node A, whose context limit is
    /// `limit_a`, while one more admits a context of node B, unlimited, all against a global budget of `capacity`.
    #[derive(Clone, Copy)]
    struct Config {
        lanes_a: usize,
        limit_a: u64,
        capacity: u64,
        /// The most times an explored execution preempts a thread, or `None` to explore every interleaving.
        preemption_bound: Option<usize>,
    }

    /// Room for one context of each node: the global budget never refuses, and three lanes race for node A's unit.
    ///
    /// Four threads of compare-and-swap loops interleave in too many ways to explore every one, so the model bounds
    /// how often an execution preempts a thread, as `loom` recommends: every interleaving with at most three
    /// preemption points is explored, which covers each pair of lanes racing on either budget.
    const ROOM_FOR_ONE_OF_EACH: Config = Config {
        lanes_a: 3,
        limit_a: 1,
        capacity: 2,
        preemption_bound: Some(3),
    };

    /// Room for one context in all: node B and two lanes of node A race for the one global unit, so a lane of node A
    /// that took its node's unit can find the global budget full, and must give its node's unit back. Every
    /// interleaving is explored.
    const ROOM_FOR_ONE_IN_ALL: Config = Config {
        lanes_a: 2,
        limit_a: 1,
        capacity: 1,
        preemption_bound: None,
    };

    /// Runs the lanes of `config`, each admitting its context with `admit`, at every interleaving `loom` explores, and
    /// checks the budgets' three properties: never above the node's limit or the capacity; the node's unit returned on
    /// a global refusal, so that every unit held is a context held; and a node at its limit never holding a global
    /// unit, so that node B is refused only when node A's admitted contexts hold every global unit.
    fn model(admit: fn(&StateBudget, &StateBudget, u64) -> bool, config: Config) {
        let Config {
            lanes_a,
            limit_a,
            capacity,
            preemption_bound,
        } = config;
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = preemption_bound;
        builder.check(move || {
            let global = Arc::new(StateBudget::new(capacity));
            let node_a = Arc::new(StateBudget::unlimited());
            let node_b = StateBudget::unlimited();

            let lanes = (0..lanes_a)
                .map(|_| {
                    let (global, node_a) = (Arc::clone(&global), Arc::clone(&node_a));
                    thread::spawn(move || {
                        let admitted = admit(&global, &node_a, limit_a);
                        // Never above the node's limit or the capacity, at any point a lane can observe.
                        assert!(
                            node_a.used() <= limit_a,
                            "violated: node A holds {} contexts",
                            node_a.used()
                        );
                        assert!(
                            global.used() <= global.capacity(),
                            "violated: the global budget is over its capacity"
                        );
                        admitted
                    })
                })
                .collect::<Vec<_>>();
            let admitted_b = admit(&global, &node_b, u64::MAX);
            let admitted_a = lanes
                .into_iter()
                .map(|lane| lane.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .filter(|admitted| *admitted)
                .count() as u64;

            // Every unit held is a context held: refusals gave back whatever they took.
            let admitted_b = u64::from(admitted_b);
            assert_eq!(
                (node_a.used(), node_b.used(), global.used()),
                (admitted_a, admitted_b, admitted_a + admitted_b),
                "violated: the budgets count units no context holds"
            );
            // A node at its limit never holds a global unit, even for a moment: a refusal of node B's context means
            // that node A's admitted contexts hold every global unit.
            assert!(
                admitted_b == 1 || admitted_a == capacity,
                "violated: node B was refused while node A held a global unit it could not keep"
            );
            // And so every context that fits is admitted: node A's up to its limit, and node B's, up to the capacity.
            assert_eq!(
                admitted_a + admitted_b,
                capacity.min(limit_a + 1),
                "violated: a context that fits was refused"
            );
        });
    }

    #[test]
    fn the_bounded_node_acquire_first_keeps_every_limit_at_any_interleaving() {
        model(admit_node_first, ROOM_FOR_ONE_OF_EACH);
    }

    #[test]
    fn the_bounded_node_acquire_first_gives_the_nodes_unit_back_when_the_global_budget_is_full() {
        model(admit_node_first, ROOM_FOR_ONE_IN_ALL);
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
}

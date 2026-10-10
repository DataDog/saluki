//! Budgets of state shared between kernels.

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
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
            }),
        }
    }

    /// Creates a budget that never refuses an acquisition.
    pub fn unlimited() -> Self {
        Self::new(u64::MAX)
    }

    /// Acquires `units` units if they fit: returns `true`, and counts them as in use, if the units in use plus `units`
    /// are at most the capacity, and `false`, changing nothing, otherwise.
    pub fn try_acquire(&self, units: u64) -> bool {
        let capacity = self.inner.capacity;
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(units).filter(|total| *total <= capacity)
            })
            .is_ok()
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

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

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

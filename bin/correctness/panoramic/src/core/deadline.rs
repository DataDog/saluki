//! Deadlines on Tokio's clock, tagged with the budget that set them.

use std::time::Duration;

use tokio::time::Instant;

/// The budget that set a [`Deadline`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Budget {
    /// The test case's `timeout`, enforced by `Runner::run_one`.
    Case,
    /// One procedure step's own budget.
    Step,
}

/// An instant on Tokio's clock, tagged with the budget that set it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    /// When the deadline passes.
    pub(crate) at: Instant,
    /// The budget that set `at`.
    pub(crate) budget: Budget,
}

impl Deadline {
    /// Starts `budget`, lasting `duration` from now.
    pub(crate) fn after(budget: Budget, duration: Duration) -> Self {
        Self {
            at: Instant::now() + duration,
            budget,
        }
    }
}

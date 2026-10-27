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
    /// The configured duration of `budget`.
    pub(crate) configured: Duration,
}

impl Deadline {
    /// Starts `budget`, lasting `duration` from now.
    pub(crate) fn after(budget: Budget, duration: Duration) -> Self {
        Self {
            at: Instant::now() + duration,
            budget,
            configured: duration,
        }
    }

    /// Returns the earlier of this deadline and a new `budget` lasting `duration`.
    ///
    /// On a tie, the new budget wins: a step receiving its full allowance is limited by its own budget.
    pub(crate) fn narrow(self, budget: Budget, duration: Duration) -> Self {
        let narrowed = Self::after(budget, duration);
        if self.at < narrowed.at {
            self
        } else {
            narrowed
        }
    }

    /// Returns the time left, or zero once the deadline has passed.
    pub(crate) fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn narrowing_keeps_the_earlier_deadline_and_its_budget() {
        let case = Deadline::after(Budget::Case, Duration::from_secs(10));

        let tighter = case.narrow(Budget::Step, Duration::from_secs(5));
        assert_eq!(
            (tighter.budget, tighter.configured),
            (Budget::Step, Duration::from_secs(5))
        );
        assert_eq!(tighter.remaining(), Duration::from_secs(5));

        let tie = case.narrow(Budget::Step, Duration::from_secs(10));
        assert_eq!((tie.budget, tie.configured), (Budget::Step, Duration::from_secs(10)));

        let clamped = case.narrow(Budget::Step, Duration::from_secs(20));
        assert_eq!(
            (clamped.budget, clamped.configured),
            (Budget::Case, Duration::from_secs(10))
        );
        assert_eq!(clamped.at, case.at);
    }

    #[tokio::test(start_paused = true)]
    async fn remaining_counts_down_to_zero_and_stays_there() {
        let deadline = Deadline::after(Budget::Case, Duration::from_secs(10));

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(deadline.remaining(), Duration::from_secs(6));

        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }
}

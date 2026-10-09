//! Panic budgets: how many panics a generation's program survives.

use std::{collections::VecDeque, time::Duration, time::Instant};

use crate::install::Origin;

/// How many panics a generation's program survives before it fails.
///
/// After a panic it survives, a region rebuilds every kernel and keeps running, losing the batch that panicked, as a
/// native component restart would.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanicBudget {
    /// No panic is survived: the first one fails the region's component, exactly as a native component failure does.
    ///
    /// This is the default program's budget.
    Zero,

    /// Panics are survived until there are more than `per_window` within any `window`, or more than `total` in all.
    /// The generation then fails, and the region drops its batches from then on, counting them as invalid loss.
    Limited {
        /// The most panics survived within one window.
        per_window: u32,

        /// The window.
        window: Duration,

        /// The most panics survived in all.
        total: u32,
    },
}

impl PanicBudget {
    /// The default program's budget: [`Zero`][Self::Zero].
    pub const DEFAULT_PROGRAM: Self = Self::Zero;

    /// An artifact generation's budget: 3 panics within 60 seconds, and 5 in all.
    pub const ARTIFACT: Self = Self::Limited {
        per_window: 3,
        window: Duration::from_secs(60),
        total: 5,
    };

    /// Returns the budget of a generation of the given origin.
    pub const fn for_origin(origin: &Origin) -> Self {
        match origin {
            Origin::DefaultProgram { .. } => Self::DEFAULT_PROGRAM,
            Origin::Artifact { .. } => Self::ARTIFACT,
        }
    }
}

/// The panics one generation's program has had.
#[derive(Clone, Debug, Default)]
pub(super) struct PanicHistory {
    recent: VecDeque<Instant>,
    total: u32,
}

impl PanicHistory {
    /// Records a panic at `now`, and returns `true` if the generation survives it.
    pub fn record(&mut self, budget: PanicBudget, now: Instant) -> bool {
        self.total = self.total.saturating_add(1);
        match budget {
            PanicBudget::Zero => false,
            PanicBudget::Limited {
                per_window,
                window,
                total,
            } => {
                while self
                    .recent
                    .front()
                    .is_some_and(|earlier| now.saturating_duration_since(*earlier) >= window)
                {
                    self.recent.pop_front();
                }
                self.recent.push_back(now);
                self.recent.len() <= per_window as usize && self.total <= total
            }
        }
    }

    /// Returns the number of panics recorded.
    pub fn total(&self) -> u32 {
        self.total
    }
}

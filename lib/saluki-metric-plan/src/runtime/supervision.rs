//! Panic budgets: how many panics a generation's program survives.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, PoisonError, Weak,
    },
    time::{Duration, Instant},
};

use saluki_core::execution::GenerationId;

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

/// How a region treats a generation after its program panicked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenerationMode {
    /// Its batches are processed.
    Healthy = 0,

    /// A state owner's kernel panicked under it, and the installer was asked to roll it back: its batches are still
    /// processed, on the rebuilt instance, until a replacement takes over.
    RollingBack = 1,

    /// It exceeded its panic budget, or a kernel it uses could not be rebuilt: its batches are dropped as invalid
    /// loss.
    Dropping = 2,
}

impl GenerationMode {
    /// Returns `true` if the generation's batches are processed.
    pub fn processes(self) -> bool {
        !matches!(self, Self::Dropping)
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Healthy,
            1 => Self::RollingBack,
            _ => Self::Dropping,
        }
    }
}

/// The panics of every generation that the lanes of one slot run, shared by the lanes.
///
/// A generation's panic budget belongs to the generation, not to a lane: a slot with lanes keeps one ledger, so that an
/// artifact's allowance does not grow with the lane count, and a generation that exhausts it in one lane stops in all
/// of them, its failure reported once. A slot without lanes keeps a ledger of its own. The ledger is touched only when
/// a generation is prepared, on a panic, and when a generation fails; every batch reads only its generation's mode, an
/// atomic.
#[derive(Debug, Default)]
pub(crate) struct PanicLedger {
    generations: Mutex<HashMap<GenerationId, Weak<GenerationLedger>>>,
}

impl PanicLedger {
    /// Creates an empty ledger.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the ledger entry of a generation, creating it if no lane holds it.
    ///
    /// Each lane that prepares the generation holds the entry for as long as it holds the generation, and the entry
    /// goes with the last of them.
    pub fn generation(&self, id: GenerationId) -> Arc<GenerationLedger> {
        let mut generations = self.generations.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = generations.get(&id).and_then(Weak::upgrade) {
            return entry;
        }
        // Entries of generations every lane released are dropped here, off the data path.
        generations.retain(|_, entry| entry.strong_count() > 0);
        let entry = Arc::new(GenerationLedger::default());
        generations.insert(id, Arc::downgrade(&entry));
        entry
    }
}

/// One generation's entry in a [`PanicLedger`]: its mode and the panics it has had, across lanes.
#[derive(Debug, Default)]
pub(crate) struct GenerationLedger {
    mode: AtomicU8,
    panics: Mutex<PanicHistory>,
}

impl GenerationLedger {
    /// Returns what lanes do with the generation's batches.
    pub fn mode(&self) -> GenerationMode {
        GenerationMode::from_u8(self.mode.load(Ordering::Acquire))
    }

    /// Records a panic at `now` against `budget`, in any lane, and returns whether the generation survives it, with the
    /// number of panics it has had in all.
    pub fn record(&self, budget: PanicBudget, now: Instant) -> (bool, u32) {
        let mut panics = self.panics.lock().unwrap_or_else(PoisonError::into_inner);
        let survives = panics.record(budget, now);
        (survives, panics.total())
    }

    /// Marks the generation as failed: its batches are dropped from now on, in every lane. Returns the mode it had, so
    /// that only the lane that moved it from healthy reports the failure.
    pub fn fail(&self) -> GenerationMode {
        GenerationMode::from_u8(self.mode.swap(GenerationMode::Dropping as u8, Ordering::AcqRel))
    }

    /// Marks a healthy generation as rolling back, and returns `true` if this call did, so that its rollback is
    /// requested once across lanes.
    pub fn roll_back(&self) -> bool {
        self.mode
            .compare_exchange(
                GenerationMode::Healthy as u8,
                GenerationMode::RollingBack as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_share_a_generations_budget_and_its_failure() {
        let ledger = PanicLedger::new();
        let (lane_0, lane_1) = (
            ledger.generation(GenerationId::new(3)),
            ledger.generation(GenerationId::new(3)),
        );
        let now = Instant::now();
        let budget = PanicBudget::Limited {
            per_window: 3,
            window: Duration::from_secs(60),
            total: 3,
        };
        // Three panics in all, whichever lanes they are in, are the budget; the fourth fails the generation.
        assert_eq!(lane_0.record(budget, now), (true, 1));
        assert_eq!(lane_1.record(budget, now), (true, 2));
        assert_eq!(lane_0.record(budget, now), (true, 3));
        assert_eq!(lane_1.record(budget, now), (false, 4));

        assert_eq!(lane_1.fail(), GenerationMode::Healthy, "the first failure reports");
        assert_eq!(lane_0.fail(), GenerationMode::Dropping, "a later one does not");
        assert!(!lane_0.mode().processes());
        assert!(!lane_1.roll_back(), "a failed generation does not roll back");
    }

    #[test]
    fn a_rollback_is_requested_once_and_entries_go_with_their_last_lane() {
        let ledger = PanicLedger::new();
        let entry = ledger.generation(GenerationId::new(1));
        assert_eq!(entry.mode(), GenerationMode::Healthy);
        assert!(entry.roll_back());
        assert!(!ledger.generation(GenerationId::new(1)).roll_back());
        assert!(entry.mode().processes());

        drop(entry);
        let fresh = ledger.generation(GenerationId::new(1));
        assert_eq!(
            fresh.mode(),
            GenerationMode::Healthy,
            "a released generation's entry is gone"
        );
        let _other = ledger.generation(GenerationId::new(2));
        assert_eq!(ledger.generations.lock().unwrap().len(), 2);
    }
}

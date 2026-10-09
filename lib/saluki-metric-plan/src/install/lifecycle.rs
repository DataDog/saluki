//! The lifecycle of a generation.

use std::fmt;

/// The state of one generation.
///
/// | From | Event | To |
/// |---|---|---|
/// | `Pending` | every region acknowledged ([`BarrierPassed`][LifecycleEvent::BarrierPassed]) | `Prepared` |
/// | `Pending`, `Prepared` | it failed or was cancelled ([`Rejected`][LifecycleEvent::Rejected]) | `Rejected` |
/// | `Prepared` | the schedule store ([`Committed`][LifecycleEvent::Committed]) | `Active` |
/// | `Active` | a newer generation committed, keeping this one to roll back to | `Prepared` |
/// | `Active` | a newer generation committed | `Draining` |
/// | `Prepared` | a newer last-known-good replaced it ([`Released`][LifecycleEvent::Released]) | `Draining` |
/// | `Draining` | its token was dropped ([`Retired`][LifecycleEvent::Retired]) | `Retired` |
/// | any but `Rejected`, `Retired`, `Failed` | supervision failed it ([`Failed`][LifecycleEvent::Failed]) | `Failed` |
///
/// A generation's token can outlive its preparation or its failure, so a rejected or failed generation retires too,
/// which leaves its state as it was.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GenerationState {
    /// The generation's programs were sent to every region, and not every region has acknowledged them yet.
    Pending,

    /// Every region holds the generation's programs: one schedule store would commit it. A generation kept as
    /// last-known-good is prepared too.
    Prepared,

    /// The schedule assigns new batches to the generation.
    Active,

    /// A newer generation replaced it; batches stamped with it still finish in it.
    Draining,

    /// Nothing references the generation any more: every region released its programs.
    Retired,

    /// Preparation failed or was cancelled; the generation was never committed.
    Rejected,

    /// Supervision failed the generation.
    Failed,
}

impl GenerationState {
    /// Returns the state's name, as acknowledgments and status report it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Prepared => "prepared",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Retired => "retired",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }

    /// Returns `true` if no event changes the state any more.
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Retired | Self::Rejected | Self::Failed)
    }

    /// Returns the state after `event`.
    ///
    /// # Errors
    ///
    /// Returns an error if the event cannot happen in this state, which is a bug in whoever reports it.
    pub fn apply(self, event: LifecycleEvent) -> Result<Self, InvalidTransition> {
        use GenerationState::*;
        use LifecycleEvent as E;

        let next = match (self, event) {
            (Pending, E::BarrierPassed) => Prepared,
            (Pending | Prepared, E::Rejected) => Rejected,
            (Prepared, E::Committed) => Active,
            (Prepared, E::Released) => Draining,
            (
                Active,
                E::Superseded {
                    kept_as_last_known_good,
                },
            ) => {
                if kept_as_last_known_good {
                    Prepared
                } else {
                    Draining
                }
            }
            (Draining, E::Retired) => Retired,
            (Rejected | Failed, E::Retired) => self,
            (Pending | Prepared | Active | Draining, E::Failed) => Failed,
            _ => return Err(InvalidTransition { from: self, event }),
        };
        Ok(next)
    }
}

impl fmt::Display for GenerationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Something that happens to a generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LifecycleEvent {
    /// Every region acknowledged the generation's programs.
    BarrierPassed,

    /// The schedule store that commits the generation happened.
    Committed,

    /// A newer generation was committed.
    Superseded {
        /// Whether the installer keeps the generation prepared as the last-known-good one, to roll back to.
        kept_as_last_known_good: bool,
    },

    /// The installer stopped keeping a prepared generation: a newer last-known-good replaced it.
    Released,

    /// The generation's token was dropped: nothing references it any more.
    Retired,

    /// Preparation failed, timed out, or was cancelled.
    Rejected,

    /// Supervision failed the generation.
    Failed,
}

/// An event that cannot happen in a generation's state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTransition {
    /// The state the generation was in.
    pub from: GenerationState,

    /// The event.
    pub event: LifecycleEvent,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} generation cannot be {:?}", self.from, self.event)
    }
}

impl std::error::Error for InvalidTransition {}

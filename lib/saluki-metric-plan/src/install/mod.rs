//! Installation: moving a running topology from one generation of the plan to the next.
//!
//! A **generation** is an immutable plan, compiled into the frame, together with where it came from. The installer
//! creates one whenever a candidate plan differs from the active one, prepares it in every region, and commits it with
//! one store into the admission schedule of the topology, after which every admission point stamps new batches with it.
//! Batches already stamped finish in the generation they were stamped with, and a generation retires when nothing
//! references it any more.
//!
//! - [`diff`][mod@diff]: what a candidate changes relative to the active plan, node by node, with each state owner's
//!   transition class, and which kernels each slot reuses ([`kernel_reuse`]).
//! - [`lifecycle`]: the states of a generation and the events that move it between them.
//! - [`InstallerCore`]: the installer's state and decisions, without I/O: the caller performs the
//!   [actions][Action] it returns and reports what happens. Artifacts from the [channel](crate::channel) reach it as
//!   snapshots ([`InstallerCore::on_snapshot`]), and it keeps each config's acknowledgment.
//! - [`correspondence`]: which plan node each artifact node became when it was composed, and the plan diff in the
//!   artifact's terms.
//!
//! Through stage 5 every change is stateless or `retain`, and commits at the next batch boundary of each segment:
//! the schedule is always `Steady`. A `cut` needs state transitions (stage 6), and is rejected naming them.

mod core;
pub use self::core::{
    Action, Boot, Candidate, CandidateId, GenerationRecord, InstallerConfig, InstallerCore, InstallerStatus, Origin,
    Outcome, PendingReason, Prepare, Rejection, Report, SlotPrepare,
};

pub mod correspondence;
pub use self::correspondence::{diff_with_correspondence, Correspondence, CorrespondingDiff};

pub mod diff;
pub use self::diff::{diff, kernel_reuse, PlanDiff};

pub mod lifecycle;
pub use self::lifecycle::{GenerationState, InvalidTransition, LifecycleEvent};

#[cfg(test)]
mod artifact_tests;

#[cfg(test)]
mod tests;

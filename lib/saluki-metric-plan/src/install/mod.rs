//! Installation: moving a running topology from one generation of the plan to the next.
//!
//! A **generation** is an immutable plan, compiled into the frame, together with where it came from. The installer
//! creates one whenever a candidate plan differs from the active one, prepares it in every region, and commits it with
//! one store into the admission schedule of the topology, after which every admission point stamps new batches with it.
//! Batches already stamped finish in the generation they were stamped with, and a generation retires when nothing
//! references it any more.
//!
//! - [`diff`][mod@diff]: what a candidate changes relative to the active plan, node by node, with each state owner's
//!   transition class and the cut a cut installation needs ([`PlanDiff::cut`]), which kernels each slot reuses
//!   ([`kernel_reuse`]), and how each slot binds its state owners to instances of their state ([`state_bindings`]).
//! - [`lifecycle`]: the states of a generation and the events that move it between them.
//! - [`InstallerCore`]: the installer's state and decisions, without I/O: the caller performs the
//!   [actions][Action] it returns and reports what happens. Artifacts from the [channel](crate::channel) reach it as
//!   snapshots ([`InstallerCore::on_snapshot`]), and it keeps each config's acknowledgment.
//! - [`correspondence`]: which plan node each artifact node became when it was composed, and the plan diff in the
//!   artifact's terms.
//!
//! A change whose state owners keep their state (`retain`), or that keeps no state, commits at the next batch boundary
//! of each segment, with the schedule `Steady`. A **cut installation**, in which a state owner changes its time
//! contract or is added or removed, switches the affected lineages at a cut time instead: the schedule holds `Cut {
//! old, new, at }` from arming to finalization (see [`InstallerCore`]).

mod core;
pub use self::core::{
    Action, AfterCancel, ArmCut, Boot, Candidate, CandidateId, CutDeferReason, CutInFlight, CutPhase, CutProgress,
    CutStatus, GenerationRecord, InstallerConfig, InstallerCore, InstallerStatus, Origin, Outcome, PendingReason,
    Prepare, Rejection, Report, SlotPrepare, StateBinding,
};

pub mod correspondence;
pub use self::correspondence::{diff_with_correspondence, Correspondence, CorrespondingDiff};

pub mod diff;
pub use self::diff::{diff, kernel_reuse, state_bindings, CutPlan, PlanDiff, StateOwnerChange, StateOwnerDiff};

pub mod lifecycle;
pub use self::lifecycle::{GenerationState, InvalidTransition, LifecycleEvent};

#[cfg(test)]
mod artifact_tests;

#[cfg(test)]
mod cut_tests;

#[cfg(test)]
mod scope_tests;

#[cfg(test)]
mod tests;

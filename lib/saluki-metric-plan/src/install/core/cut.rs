//! Cut installations: the actions that arm, watch, cancel, and finalize a cut, and the core's events for them.
//!
//! A candidate whose state owners change their time contract, or that adds or removes a state owner, is a **cut
//! installation**: its state owners' lineages cannot carry over at a batch boundary, so the old and new lineages switch
//! at a cut time `T`, a boundary every bucket width involved shares. After every region acknowledged the preparation,
//! the cut is armed (`T` set on the new generation's token and the schedule `Cut { old, new, at: T }` published under
//! the affected admission points' locks), becomes irrevocable when any admission point stamps the new generation, and
//! is finalized (the schedule `Steady(new)`) once every affected point has passed `T`. Until it is irrevocable, a
//! newer proposal can cancel it.
//!
//! # Phases
//!
//! | Phase | Event | Next |
//! |---|---|---|
//! | barrier (every region acknowledged) | [`cut_armed`][InstallerCore::cut_armed] | armed at `T` |
//! | barrier | [`cut_deferred`][InstallerCore::cut_deferred] | barrier, armed again after [`CUT_ARM_RETRY`]; an artifact is rejected after [`CUT_ARM_ATTEMPTS`] lock timeouts in a row |
//! | armed | a poll in which a point stamped the generation ([`cut_progress`][InstallerCore::cut_progress]) | irrevocable: the generation is active |
//! | armed | [`cut_cancelled`][InstallerCore::cut_cancelled] | cancelled: the generation is aborted, the schedule kept the old one |
//! | armed | [`cut_cancel_refused`][InstallerCore::cut_cancel_refused] | irrevocable |
//! | irrevocable | a poll in which every affected point passed `T` | finalized: the schedule holds only the generation |
//!
//! # What waits for a cut
//!
//! | Event | Barrier | Armed | Irrevocable |
//! |---|---|---|---|
//! | a newer artifact | aborts the cut, and is prepared | cancels the cut, and is prepared once it is cancelled | waits (latest wins), and is prepared after finalization |
//! | a re-composition of the cut's own artifact | aborts the cut, and is prepared | waits | waits |
//! | a default-program candidate | waits | waits | waits |
//! | a revocation | aborts the cut, and asks for the default program | cancels the cut, then asks for the default program | asks for the default program, which waits |
//! | the cut generation fails | aborts it, failed | cancels the cut, failed if it is cancelled | it keeps processing its input; the plan the cut replaced is installed again, after finalization |
//! | the replaced generation fails | aborts the cut, rolls back, and prepares the cut again, or rejects it if it serves an artifact of the failed generation | cancels the cut, then rolls back and prepares the cut again, or rejects it if it serves an artifact of the failed generation | recorded failed: it is draining |
//!
//! A cancellation that is refused makes the cut irrevocable, and whatever asked for it waits for the finalization. A
//! cancellation whose locks were not free in time is refused too: an admission point may have stamped the generation,
//! so the cut is acknowledged active from then on, possibly before any batch carries its generation.
//!
//! A newer artifact that could not follow the active plan (a state owner's transition is rejected, or the widths do not
//! line up) neither aborts nor cancels the cut: it waits for the cut if it can follow the cut's plan, and is rejected
//! at once otherwise. An artifact's widths must also line up with the default program's, so that the default program
//! can always replace it at a cut, as it does after a revocation.

use std::{fmt, time::Duration};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, GenerationId, GenerationRef};

use super::{
    artifacts::{Settle, REVOKED},
    retry::Wait,
    Action, Installed, InstallerCore, Origin, Outcome, PendingReason, Preparation, Queued, Rejection, Report, Rollback,
    PREPARED_AGAINST_FAILED,
};
use crate::{
    compile::CompiledPlan,
    install::{
        diff::CutPlan,
        lifecycle::{GenerationState, LifecycleEvent},
    },
    limits::values::{CUT_ARM_ATTEMPTS, CUT_ARM_RETRY, CUT_MARGIN},
    runtime::state::rules::lcm_secs,
};

/// Why a cut generation is aborted when its cut boundary was recorded but the cut never published.
const STRANDED: &str =
    "its cut boundary was recorded without the cut being published, so it is prepared again as a new \
                        generation";

/// What [`Action::ArmCut`] arms: a prepared generation that takes over from the active one at a cut.
#[derive(Clone, Debug)]
pub struct ArmCut {
    /// The generation that takes over at the cut, whose token records the cut's boundary.
    pub generation: GenerationRef,

    /// The generation it takes over from, which the schedule names as the cut's old generation.
    pub old: GenerationRef,

    /// The bucket widths of the affected state owners, before and after, in increasing order: the cut time is a
    /// multiple of their least common multiple.
    pub widths: Vec<Duration>,

    /// The admission points the cut is armed over, in the order to lock them: those upstream of the affected state
    /// owners, in ID order.
    pub affected: Vec<AdmissionPointId>,

    /// The least time between the affected points' latest stamps or wall clocks and the cut time, the limits table's
    /// cut margin.
    pub margin: Duration,
}

/// Why arming a cut was put off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutDeferReason {
    /// The affected admission points' locks were not free within the lock timeout.
    Timeout,

    /// An affected admission point's clock runs ahead of its wall clock, absorbing a backward jump: a cut time from it
    /// would wait far longer than a cut may.
    ClockAhead {
        /// How far the clock runs ahead.
        by: Duration,
    },
}

/// The phase of a cut installation past its barrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutPhase {
    /// Every region acknowledged the preparation; the cut is not armed yet.
    Barrier,

    /// The cut is armed at `at`, and no admission point stamped the new generation yet: it can still be cancelled.
    Armed {
        /// The cut time.
        at: AdmissionTime,
    },

    /// An admission point stamped the new generation: the cut can no longer be cancelled, and completes once every
    /// affected point passes `at`.
    Irrevocable {
        /// The cut time.
        at: AdmissionTime,
    },
}

impl CutPhase {
    /// Returns the cut time, once the cut is armed.
    pub const fn at(&self) -> Option<AdmissionTime> {
        match self {
            Self::Barrier => None,
            Self::Armed { at } | Self::Irrevocable { at } => Some(*at),
        }
    }
}

/// What the core does once a cancellation it asked for succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AfterCancel {
    /// Prepare the newer candidate that cancelled the cut.
    PrepareNewer,

    /// Roll back from the active generation, which failed while the cut was armed, and prepare the cut's candidate
    /// again against the generation rolled back to.
    RollBackOld {
        /// Why the active generation failed.
        reason: String,
    },

    /// Ask for the default program: the policy was revoked while the cut was armed.
    InstallDefaultProgram,
}

/// What one poll of the admission points says about a cut in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutProgress {
    /// Whether any admission point stamped the cut's new generation, data or heartbeat.
    pub stamped_new: bool,

    /// The affected admission points that passed the cut: every later stamp of theirs gets the new generation.
    pub passed: usize,

    /// The affected admission points.
    pub affected: usize,
}

impl CutProgress {
    /// Returns `true` if every affected admission point passed the cut, which finalizes it.
    pub const fn all_passed(&self) -> bool {
        self.passed >= self.affected
    }
}

/// The cut a cut installation's preparation carries, checked: its owners and widths, and the admission points it is
/// armed over.
#[derive(Clone, Debug)]
pub(super) struct CutSetup {
    pub(super) plan: CutPlan,
    pub(super) affected: Vec<AdmissionPointId>,
}

impl CutSetup {
    /// Checks that a cut's widths allow it: each is known, and together they line up at least every
    /// [`MAX_TRANSITION_LCM`][crate::limits::values::MAX_TRANSITION_LCM].
    ///
    /// # Errors
    ///
    /// Returns [`Rejection::CutWidths`] if they do not.
    pub(super) fn checked(plan: CutPlan, affected: Vec<AdmissionPointId>) -> Result<Self, Rejection> {
        if let Some(node) = plan.unknown_widths.first() {
            return Err(Rejection::CutWidths {
                widths: plan.widths.clone(),
                message: format!(
                    "state owner `{}` switches its state at a cut, but its bucket width is not known, so no cut time \
                     lines up with it: resolve the binding of its window",
                    node
                ),
            });
        }
        if let Err(error) = lcm_secs(&plan.widths) {
            return Err(Rejection::CutWidths {
                widths: plan.widths.clone(),
                message: error.to_string(),
            });
        }
        Ok(Self { plan, affected })
    }
}

/// A cut installation past its barrier: the generation armed, or being armed, at a cut, and what follows it.
#[derive(Clone)]
pub struct CutInFlight {
    /// The cut generation, as the core holds a generation.
    pub(super) installed: Installed,
    /// Its compilation, to prepare it again if the cut is aborted before it is irrevocable.
    pub(super) compiled: CompiledPlan,
    /// The preparations attempted.
    pub(super) attempts: u32,
    /// The generation the cut replaces.
    pub(super) old: GenerationId,
    pub(super) setup: CutSetup,
    pub(super) phase: CutPhase,
    pub(super) progress: Option<CutProgress>,
    /// Why arming was last put off, and how many lock timeouts in a row there were then.
    pub(super) deferred: Option<(CutDeferReason, u32)>,
    /// The lock timeouts in a row.
    pub(super) timeouts: u32,
    /// Whether arming was put off and waits for the retry delay.
    pub(super) arm_due: bool,
    /// Whether a cancellation was asked for and its outcome is not known yet.
    pub(super) cancelling: bool,
    pub(super) after_cancel: Option<AfterCancel>,
    /// Why supervision failed the cut generation, if it did.
    pub(super) failed: Option<String>,
    /// Why the cut's candidate is rejected rather than prepared again if the cut is cancelled after the active
    /// generation failed: it serves an artifact that failure took out of effect.
    pub(super) withdrawn: Option<String>,
}

impl fmt::Debug for CutInFlight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CutInFlight").field("status", &self.status()).finish()
    }
}

impl CutInFlight {
    /// Returns the generation that takes over at the cut.
    pub fn generation(&self) -> &GenerationRef {
        &self.installed.generation
    }

    /// Returns the generation it takes over from.
    pub fn old(&self) -> GenerationId {
        self.old
    }

    /// Returns the cut's phase.
    pub fn phase(&self) -> CutPhase {
        self.phase
    }

    /// Returns the bucket widths the cut time lines up with.
    pub fn widths(&self) -> &[Duration] {
        &self.setup.plan.widths
    }

    /// Returns the admission points the cut is armed over, in ID order.
    pub fn affected_points(&self) -> &[AdmissionPointId] {
        &self.setup.affected
    }

    /// Returns what the latest poll of the admission points said, if one was reported.
    pub fn progress(&self) -> Option<CutProgress> {
        self.progress
    }

    /// Returns why arming was last put off, and how many lock timeouts in a row there were then, if it was.
    pub fn deferred(&self) -> Option<(CutDeferReason, u32)> {
        self.deferred
    }

    /// Returns what follows a cancellation the core asked for, if it asked for one that has not ended.
    pub fn after_cancel(&self) -> Option<&AfterCancel> {
        self.after_cancel.as_ref()
    }

    /// Returns the cut's status.
    pub fn status(&self) -> CutStatus {
        CutStatus {
            generation: self.installed.generation.id(),
            at: self.phase.at(),
            phase: self.phase,
            stamped_new: self.progress.is_some_and(|progress| progress.stamped_new),
            passed: self.progress.map_or(0, |progress| progress.passed),
            total: self.setup.affected.len(),
            deferred: self.deferred,
        }
    }

    /// Returns the cut's candidate, to be prepared again as a new generation.
    fn into_queued(self) -> Queued {
        Queued {
            candidate: self.installed.candidate,
            origin: self.installed.origin,
            plan: self.installed.plan,
            plan_hash: self.installed.plan_hash,
            report: self.installed.report,
            compiled: self.compiled,
            attempts: self.attempts,
            reported: None,
        }
    }
}

/// The status of a cut in flight, for status and telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutStatus {
    /// The generation that takes over at the cut.
    pub generation: GenerationId,

    /// The cut time, once armed.
    pub at: Option<AdmissionTime>,

    /// The cut's phase.
    pub phase: CutPhase,

    /// Whether any admission point stamped the new generation, at the latest poll.
    pub stamped_new: bool,

    /// The affected admission points that passed the cut, at the latest poll.
    pub passed: usize,

    /// The affected admission points.
    pub total: usize,

    /// Why arming was last put off, and how many lock timeouts in a row there were then, if it was.
    pub deferred: Option<(CutDeferReason, u32)>,
}

impl InstallerCore {
    /// Returns the cut installation in flight, if there is one.
    #[cfg(test)]
    pub fn cut_in_flight(&self) -> Option<&CutInFlight> {
        self.cut.as_ref()
    }

    /// Returns the candidate a failed cut generation rolls back to: the plan and provenance of the generation the cut
    /// replaced, or `None` if that was the default program, which the core asks for instead.
    ///
    /// It is the plan of the generation the latest cut installation replaced, while no newer generation is kept to
    /// roll back to: an artifact generation that commits without a cut keeps the generation it replaces instead.
    #[cfg(test)]
    pub fn rollback_candidate(&self) -> Option<&super::Candidate> {
        self.rollback.as_ref().map(|rollback| &rollback.candidate)
    }

    /// Reports that an [`Action::ArmCut`] armed the cut of `generation` at `at`.
    ///
    /// The cut is armed: its generation is acknowledged `prepared` with its cut time, and the caller watches it
    /// ([`Action::WatchCut`]). A cut armed again after a restart keeps the cut time it was armed at first.
    pub fn cut_armed(&mut self, generation: GenerationId, at: AdmissionTime) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(cut) = self.cut_of(generation) else {
            return actions;
        };
        if cut.phase == CutPhase::Barrier {
            cut.phase = CutPhase::Armed { at };
            cut.deferred = None;
            cut.timeouts = 0;
            cut.arm_due = false;
        }
        let at = cut.phase.at().unwrap_or(at);
        actions.push(Action::WatchCut { generation, at });
        self.settle(actions, Settle::Quiet)
    }

    /// Reports that an [`Action::ArmCut`] of `generation` was put off, and why.
    ///
    /// Arming is tried again after the limits table's cut arm retry delay ([`Action::RetryAfter`], then
    /// [`retry`][Self::retry]). An artifact's cut whose locks were not free in time [`CUT_ARM_ATTEMPTS`] times in a row
    /// is rejected instead ([`Rejection::CutNotArmed`]); a default program's, or a rollback's, keeps being retried, so
    /// that it installs eventually. A clock that runs ahead does not count as an attempt: it catches up at the speed of
    /// time.
    pub fn cut_deferred(&mut self, generation: GenerationId, reason: CutDeferReason) -> Vec<Action> {
        let mut actions = Vec::new();
        let rollbacks = &self.rollbacks;
        let Some(cut) = self
            .cut
            .as_mut()
            .filter(|cut| cut.installed.generation.id() == generation && cut.phase == CutPhase::Barrier)
        else {
            return actions;
        };
        if reason == CutDeferReason::Timeout {
            cut.timeouts = cut.timeouts.saturating_add(1);
        }
        let attempts = cut.timeouts;
        cut.deferred = Some((reason, attempts));
        let candidate = cut.installed.candidate;
        let origin = cut.installed.origin;
        let gives_up = reason == CutDeferReason::Timeout
            && u64::from(attempts) >= CUT_ARM_ATTEMPTS
            && origin.can_roll_back()
            && !rollbacks.contains(&candidate);
        if !gives_up {
            cut.arm_due = true;
        }
        if gives_up {
            self.abort_cut(Rejection::CutNotArmed { attempts }, &mut actions);
            self.try_start(&mut actions);
            return self.settle(actions, Settle::AfterFailure { rolled_back: false });
        }
        actions.push(Action::Report(Report {
            candidate,
            origin,
            generation: Some(generation),
            outcome: Outcome::Pending(PendingReason::CutDeferred { reason, attempts }),
        }));
        self.start_wait(Wait::Cut, CUT_ARM_RETRY, &mut actions);
        self.settle(actions, Settle::Quiet)
    }

    /// Reports what a poll of the admission points says about the cut of `generation`.
    ///
    /// The first poll in which some admission point stamped the generation makes the cut irrevocable: the generation is
    /// active from then on, the generation it replaces drains, and the generation kept as last-known-good, if any, is
    /// released, since a cut installation keeps none. The first poll in which every affected point passed the cut
    /// finalizes it ([`Action::FinalizeCut`]), after which waiting candidates are prepared.
    pub fn cut_progress(&mut self, generation: GenerationId, progress: CutProgress) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(cut) = self.cut_of(generation).filter(|cut| cut.phase != CutPhase::Barrier) else {
            return actions;
        };
        cut.progress = Some(progress);
        // Every affected point passing the cut means each stamped the generation, with no affected point at all a cut
        // whose state owners no admission point feeds.
        let all_passed = progress.passed >= cut.setup.affected.len();
        if matches!(cut.phase, CutPhase::Armed { .. }) && (progress.stamped_new || all_passed) {
            self.make_irrevocable(&mut actions);
        }
        if all_passed {
            self.finalize(&mut actions);
        }
        self.settle(actions, Settle::Quiet)
    }

    /// Reports that an [`Action::CancelCut`] of `generation` cancelled the cut: no admission point had stamped it.
    ///
    /// The generation is aborted in every region, and rejected, or failed if supervision failed it meanwhile. Then
    /// what asked for the cancellation follows: the newer candidate is prepared, the default program is asked for, or
    /// the active generation, which failed, rolls back and the cut's candidate is prepared again.
    pub fn cut_cancelled(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.cut.as_ref().is_none_or(|cut| {
            cut.installed.generation.id() != generation || !matches!(cut.phase, CutPhase::Armed { .. })
        }) {
            return actions;
        }
        let Some(cut) = self.cut.take() else { return actions };
        self.push_abort(generation, &mut actions);
        let after = cut.after_cancel.clone();
        let candidate = cut.installed.candidate;
        let origin = cut.installed.origin;
        let rejected = |rejection: Rejection| {
            Action::Report(Report {
                candidate,
                origin,
                generation: Some(generation),
                outcome: Outcome::Rejected(rejection),
            })
        };
        match (&cut.failed, &after) {
            (Some(reason), _) => {
                self.transition(generation, LifecycleEvent::Failed, Some(reason.clone()));
                self.report_generation(generation, &mut actions);
            }
            (None, Some(AfterCancel::RollBackOld { .. })) => {
                // The cut's candidate is prepared again against the generation rolled back to, unless it serves an
                // artifact the failure or a revocation took out of effect, as a preparation in flight would be.
                let dropped = cut
                    .withdrawn
                    .clone()
                    .or_else(|| self.serves_revoked(candidate).then(|| REVOKED.to_string()));
                match dropped {
                    Some(message) => {
                        self.transition(generation, LifecycleEvent::Rejected, Some(message.clone()));
                        actions.push(rejected(Rejection::Refused { message }));
                    }
                    None => {
                        self.transition(
                            generation,
                            LifecycleEvent::Rejected,
                            Some(PREPARED_AGAINST_FAILED.to_string()),
                        );
                        self.report_generation(generation, &mut actions);
                        self.requeue(cut.into_queued(), Some(generation), &mut actions);
                    }
                }
            }
            (None, Some(AfterCancel::InstallDefaultProgram)) => {
                self.transition(generation, LifecycleEvent::Rejected, Some(REVOKED.to_string()));
                actions.push(rejected(Rejection::Refused {
                    message: REVOKED.to_string(),
                }));
            }
            (None, Some(AfterCancel::PrepareNewer) | None) => {
                self.transition(
                    generation,
                    LifecycleEvent::Rejected,
                    Some(Rejection::Cancelled.to_string()),
                );
                actions.push(rejected(Rejection::Cancelled));
            }
        }
        let mut settle = Settle::Quiet;
        match after {
            Some(AfterCancel::RollBackOld { .. }) => {
                let before = self.active.as_ref().map(|active| active.generation.id());
                self.roll_back_active(&mut actions);
                let rolled_back = self.active.as_ref().map(|active| active.generation.id()) != before;
                settle = Settle::AfterFailure { rolled_back };
            }
            Some(AfterCancel::InstallDefaultProgram) if !self.default_in_flight() => {
                actions.push(Action::InstallDefaultProgram);
            }
            Some(AfterCancel::InstallDefaultProgram | AfterCancel::PrepareNewer) | None => {}
        }
        self.prune();
        self.try_start(&mut actions);
        self.settle(actions, settle)
    }

    /// Reports that an [`Action::CancelCut`] of `generation` was refused: an admission point had stamped it, or the
    /// locks were not free in time. The cut is irrevocable, and proceeds; whatever asked for the cancellation waits for
    /// its finalization.
    pub fn cut_cancel_refused(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(cut) = self.cut_of(generation) else {
            return actions;
        };
        cut.cancelling = false;
        if matches!(cut.phase, CutPhase::Armed { .. }) {
            self.make_irrevocable(&mut actions);
        }
        self.settle(actions, Settle::Quiet)
    }

    /// Reports that a re-issued [`Action::ArmCut`] of `generation` found the cut's boundary recorded on the
    /// generation's token, but the schedule not holding the cut: the arming was interrupted between the two.
    ///
    /// Regions may have read that boundary, so the generation is never armed at another cut time: it is aborted, and
    /// its candidate prepared again as a new generation.
    pub fn cut_arm_abandoned(&mut self, generation: GenerationId) -> Vec<Action> {
        let mut actions = Vec::new();
        if self
            .cut
            .as_ref()
            .is_none_or(|cut| cut.installed.generation.id() != generation || cut.phase != CutPhase::Barrier)
        {
            return actions;
        }
        let Some(cut) = self.cut.take() else { return actions };
        self.push_abort(generation, &mut actions);
        self.transition(generation, LifecycleEvent::Rejected, Some(STRANDED.to_string()));
        self.report_generation(generation, &mut actions);
        self.requeue(cut.into_queued(), Some(generation), &mut actions);
        self.try_start(&mut actions);
        self.settle(actions, Settle::Quiet)
    }

    /// Returns the cut action in flight again, for a caller that restarted while performing it: arming (a caller that
    /// finds the cut already published reports it armed), watching, with a cancellation asked for if one is, or the
    /// finalization of the cut finalized last, while the schedule holds nothing later. Arming, cancelling, and
    /// finalizing are idempotent by the schedule's value.
    pub fn resume_actions(&self) -> Vec<Action> {
        let Some(cut) = &self.cut else {
            return self
                .finalized
                .iter()
                .filter(|generation| {
                    self.active
                        .as_ref()
                        .is_some_and(|active| active.generation.id() == generation.id())
                })
                .map(|generation| Action::FinalizeCut {
                    generation: generation.clone(),
                })
                .collect();
        };
        let generation = cut.installed.generation.id();
        match cut.phase {
            CutPhase::Barrier => self.arm_action().into_iter().collect(),
            CutPhase::Armed { at } | CutPhase::Irrevocable { at } => {
                let mut actions = vec![Action::WatchCut { generation, at }];
                if cut.cancelling {
                    actions.extend(self.cancel_action());
                }
                actions
            }
        }
    }

    /// Returns the cut in flight, if it is the one of `generation`.
    fn cut_of(&mut self, generation: GenerationId) -> Option<&mut CutInFlight> {
        self.cut
            .as_mut()
            .filter(|cut| cut.installed.generation.id() == generation)
    }

    /// Moves a preparation every region acknowledged into the cut in flight, and arms it.
    pub(super) fn begin_cut(&mut self, actions: &mut Vec<Action>) {
        let Some(Preparation {
            installed,
            compiled,
            attempts,
            cut: Some(setup),
            ..
        }) = self.preparing.take()
        else {
            return;
        };
        let Some(old) = self.active.as_ref().map(|active| active.generation.id()) else {
            return;
        };
        let id = installed.generation.id();
        self.transition(id, LifecycleEvent::BarrierPassed, None);
        self.report_generation(id, actions);
        self.cut = Some(CutInFlight {
            installed,
            compiled,
            attempts,
            old,
            setup,
            phase: CutPhase::Barrier,
            progress: None,
            deferred: None,
            timeouts: 0,
            arm_due: false,
            cancelling: false,
            after_cancel: None,
            failed: None,
            withdrawn: None,
        });
        self.finalized = None;
        actions.extend(self.arm_action());
    }

    /// Returns the action that arms the cut in flight.
    pub(super) fn arm_action(&self) -> Option<Action> {
        let cut = self.cut.as_ref()?;
        let old = self.active.as_ref()?;
        Some(Action::ArmCut(ArmCut {
            generation: cut.installed.generation.clone(),
            old: old.generation.clone(),
            widths: cut.setup.plan.widths.clone(),
            affected: cut.setup.affected.clone(),
            margin: CUT_MARGIN,
        }))
    }

    /// Returns the action that cancels the cut in flight.
    fn cancel_action(&self) -> Option<Action> {
        let cut = self.cut.as_ref()?;
        let old = self.active.as_ref()?;
        Some(Action::CancelCut {
            generation: cut.installed.generation.id(),
            old: old.generation.clone(),
        })
    }

    /// Aborts the cut in flight, which is not armed yet, rejecting its candidate.
    pub(super) fn abort_cut(&mut self, rejection: Rejection, actions: &mut Vec<Action>) {
        let Some(cut) = self.cut.take() else { return };
        let id = cut.installed.generation.id();
        self.push_abort(id, actions);
        self.transition(id, LifecycleEvent::Rejected, Some(rejection.to_string()));
        actions.push(Action::Report(Report {
            candidate: cut.installed.candidate,
            origin: cut.installed.origin,
            generation: Some(id),
            outcome: Outcome::Rejected(rejection),
        }));
    }

    /// Asks for the armed cut in flight to be cancelled, once, and records what follows a successful cancellation: a
    /// later request replaces an earlier one, except that a rollback of the failed active generation stays asked for.
    pub(super) fn request_cancel(&mut self, after: Option<AfterCancel>, actions: &mut Vec<Action>) {
        let Some(cut) = self
            .cut
            .as_mut()
            .filter(|cut| matches!(cut.phase, CutPhase::Armed { .. }))
        else {
            return;
        };
        match (after, &cut.after_cancel) {
            (Some(after @ AfterCancel::RollBackOld { .. }), _) => cut.after_cancel = Some(after),
            (Some(_), Some(AfterCancel::RollBackOld { .. })) | (None, _) => {}
            (Some(after), _) => cut.after_cancel = Some(after),
        }
        if !cut.cancelling {
            cut.cancelling = true;
            actions.extend(self.cancel_action());
        }
    }

    /// Handles a supervision failure that concerns the cut in flight, and returns `true` if nothing more is to be done
    /// for it: see [`generation_failed`][Self::generation_failed].
    pub(super) fn fail_during_cut(
        &mut self, generation: GenerationId, reason: &str, actions: &mut Vec<Action>,
    ) -> bool {
        let Some(cut) = &self.cut else { return false };
        let cut_id = cut.installed.generation.id();
        let phase = cut.phase;
        if generation == cut_id {
            match phase {
                CutPhase::Barrier => {
                    self.cut = None;
                    self.push_abort(generation, actions);
                    self.transition(generation, LifecycleEvent::Failed, Some(reason.to_string()));
                    self.report_generation(generation, actions);
                }
                CutPhase::Armed { .. } => {
                    if let Some(cut) = self.cut.as_mut() {
                        cut.failed = Some(reason.to_string());
                    }
                    self.request_cancel(None, actions);
                }
                CutPhase::Irrevocable { .. } => {
                    // The generation is active: it keeps processing its input on its restarted instances until its cut
                    // is finalized, and the plan the cut replaced is prepared to follow it.
                    if !self.has_failed(generation) {
                        if let Some(cut) = self.cut.as_mut() {
                            cut.failed = Some(reason.to_string());
                        }
                        self.transition(generation, LifecycleEvent::Failed, Some(reason.to_string()));
                        self.report_generation(generation, actions);
                        if self.pending.is_none() {
                            self.install_rollback(actions);
                        }
                    }
                }
            }
            return true;
        }
        // A replaced default program failed as a native component does: nothing rolls back, and the cut proceeds.
        let replaced =
            generation == cut.old && self.active.as_ref().is_some_and(|active| active.origin.can_roll_back());
        if replaced && phase == CutPhase::Barrier {
            // The cut was prepared against the failed generation: prepare it again against what it rolls back to,
            // which the failure of the active generation then decides.
            if let Some(cut) = self.cut.take() {
                self.push_abort(cut_id, actions);
                self.transition(
                    cut_id,
                    LifecycleEvent::Rejected,
                    Some(PREPARED_AGAINST_FAILED.to_string()),
                );
                self.report_generation(cut_id, actions);
                self.requeue(cut.into_queued(), Some(cut_id), actions);
            }
            return false;
        }
        if replaced && matches!(phase, CutPhase::Armed { .. }) {
            if !self.has_failed(generation) {
                self.transition(generation, LifecycleEvent::Failed, Some(reason.to_string()));
                self.report_generation(generation, actions);
                let after = AfterCancel::RollBackOld {
                    reason: reason.to_string(),
                };
                self.request_cancel(Some(after), actions);
            }
            return true;
        }
        false
    }

    /// Makes the armed cut in flight irrevocable: its generation is active, the one it replaces drains, and the
    /// generation kept as last-known-good, if any, is released, the plan the cut replaced being kept instead.
    fn make_irrevocable(&mut self, actions: &mut Vec<Action>) {
        let Some(cut) = self.cut.as_mut() else { return };
        let CutPhase::Armed { at } = cut.phase else { return };
        cut.phase = CutPhase::Irrevocable { at };
        cut.cancelling = false;
        let after_cancel = cut.after_cancel.take();
        let failed = cut.failed.clone();
        let installed = cut.installed.clone();
        let id = installed.generation.id();
        self.transition(id, LifecycleEvent::Committed, None);
        // An admission point stamped the generation: its leases and recipients are committed, and a recipient it
        // reopened that failed meanwhile has its next incarnation prepared.
        self.resources.commit(id);
        self.after_exit(actions);
        self.report_generation(id, actions);

        let keep = installed.origin.can_roll_back();
        let candidate = installed.candidate;
        if let Some(replaced) = self.active.replace(installed) {
            let replaced_id = replaced.generation.id();
            let previous = self.last_known_good.as_ref().map(Rollback::of);
            self.rollback = if self.state_of(replaced_id) == Some(GenerationState::Active) {
                // The rules of last-known-good, kept as a plan: the replaced artifact's, unless it is the same document
                // or a revoked one, when what was kept before stays kept.
                let kept = keep
                    && !self.serve_same_artifact(candidate, replaced.candidate)
                    && !self.serves_revoked(replaced.candidate);
                self.transition(
                    replaced_id,
                    LifecycleEvent::Superseded {
                        kept_as_last_known_good: false,
                    },
                    None,
                );
                self.report_generation(replaced_id, actions);
                if kept {
                    replaced.origin.can_roll_back().then(|| Rollback::of(&replaced))
                } else if keep {
                    previous.or(self.rollback.take())
                } else {
                    None
                }
            } else if keep {
                previous.or(self.rollback.take())
            } else {
                None
            };
        }
        // A cut installation keeps no last-known-good generation: rolling back to it with one store would run two
        // lineages of a state owner without a boundary between them.
        if let Some(released) = self.last_known_good.take() {
            let released_id = released.generation.id();
            if self.state_of(released_id) == Some(GenerationState::Prepared) {
                self.transition(released_id, LifecycleEvent::Released, None);
                self.report_generation(released_id, actions);
            }
        }
        if let Some(reason) = failed {
            self.withdraw_failing(id, &reason, actions);
            self.transition(id, LifecycleEvent::Failed, Some(reason));
            self.report_generation(id, actions);
            if self.pending.is_none() {
                self.install_rollback(actions);
            }
        }
        if after_cancel == Some(AfterCancel::InstallDefaultProgram) && !self.default_in_flight() {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Finalizes the irrevocable cut in flight: the schedule holds only its generation, and what waited is prepared.
    fn finalize(&mut self, actions: &mut Vec<Action>) {
        if !self
            .cut
            .as_ref()
            .is_some_and(|cut| matches!(cut.phase, CutPhase::Irrevocable { .. }))
        {
            return;
        }
        let Some(cut) = self.cut.take() else { return };
        let generation = cut.installed.generation.clone();
        self.finalized = Some(generation.clone());
        actions.push(Action::FinalizeCut { generation });
        // A cut generation that failed is replaced: by what waits, or else by the default program.
        if cut.failed.is_some() && self.pending.is_none() && !self.default_in_flight() {
            actions.push(Action::InstallDefaultProgram);
        }
        self.prune();
        self.try_start(actions);
    }

    /// Returns `true` if a default-program candidate waits, is being prepared, or is the cut in flight.
    pub(super) fn default_in_flight(&self) -> bool {
        self.pending
            .as_ref()
            .map(|queued| queued.origin)
            .into_iter()
            .chain(self.preparing.as_ref().map(|preparing| preparing.installed.origin))
            .chain(self.cut.as_ref().map(|cut| cut.installed.origin))
            .any(|origin| matches!(origin, Origin::DefaultProgram { .. }))
    }
}

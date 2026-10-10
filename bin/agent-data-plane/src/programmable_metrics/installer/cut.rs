//! Cut installations, as the installer worker performs them against the admission points of the topology.
//!
//! The installer core decides every step of a cut and does no I/O. The worker performs each step against the
//! [`AdmissionRegistry`] the frame's admission points register in, and reports the outcome back to the core:
//!
//! - [`arm`]: in one transaction over the locks of the affected admission points (those upstream of the state owners
//!   whose lineages change), compute the cut time from the points' latest stamps and wall clocks, record the cut's
//!   boundary on the new generation's token, and publish `Cut { old, new, at }`, all before any of them stamps again;
//! - [`poll`]: while the cut is in flight, read every point without waiting on a busy one, to tell whether any point
//!   stamped the new generation, which makes the cut irrevocable, and whether every affected point passed the cut,
//!   which finalizes it;
//! - [`cancel`]: in one transaction over every point, those that feed no state owner first, publish `Steady(old)` if no
//!   point stamped the new generation; otherwise, or if the locks are not free in time, refuse.
//!
//! Finalizing needs no lock: once every affected point passed the cut, `Steady(new)` and the cut assign every later
//! stamp alike, so the installer stores it directly.
//!
//! Every step is idempotent by the schedule's value, so that a restarted worker can perform again the step it was
//! performing when it stopped.

use std::{num::NonZeroU64, time::Duration};

use futures::future::join_all;
use saluki_core::execution::{
    AdmissionPointId, AdmissionPointStatus, AdmissionRegistry, AdmissionSchedule, AdmissionTime, CutBoundary,
    GenerationId, GenerationRef, ScheduleHandle, ScheduleTransaction, TransactionError,
};
use saluki_metric_plan::{
    install::{ArmCut, CutDeferReason, CutProgress},
    runtime::state::rules,
};

/// What arming a cut did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ArmOutcome {
    /// The cut is armed at `at`: the new generation's token records the cut's boundary, and the schedule holds `Cut {
    /// old, new, at }`.
    Armed {
        /// The cut time.
        at: AdmissionTime,
    },

    /// Arming was put off, for the reason given. Nothing changed.
    Deferred(CutDeferReason),

    /// The new generation's token records a cut boundary the schedule never published, and an affected admission
    /// point stamped past it since, for the reason given: the generation can never be armed, and nothing changed.
    Stranded(String),

    /// The cut cannot be armed, for the reason given. Nothing changed.
    Failed(String),
}

/// Arms `cut` over its affected admission points, taking their locks within `lock_timeout`.
///
/// While the locks are held, no affected point stamps: every affected stamp before the schedule changes has an
/// admission time at most the point's latest, which the cut time is after, and every affected stamp after it reads the
/// cut. A point whose clock runs ahead of its wall clock by more than the cut's margin is absorbing a backward jump of
/// the wall clock, and would put the cut time far behind the time the point stamps until it catches up: arming is put
/// off until it has.
///
/// Arming a cut the schedule already holds only reports it, so that a restarted worker can arm again. A token that
/// records a boundary the schedule does not hold, which only an interrupted arm leaves, is published with that same
/// boundary if it is still ahead of every affected point in the epoch it records, and is otherwise stranded: a
/// generation is never armed at another time, since regions may have read the first.
pub(super) async fn arm(
    registry: &AdmissionRegistry, schedule: &ScheduleHandle, cut: &ArmCut, lock_timeout: Duration,
) -> ArmOutcome {
    let new = cut.generation.id();
    if let AdmissionSchedule::Cut { new: armed, at, .. } = &**schedule.load() {
        if armed.id() == new {
            return ArmOutcome::Armed { at: *at };
        }
    }
    if cut.affected.is_empty() {
        return ArmOutcome::Failed(format!(
            "the cut of generation {} has no affected admission point to arm it over",
            new
        ));
    }
    let lcm = match rules::lcm_secs(&cut.widths) {
        Ok(lcm) => lcm,
        Err(error) => return ArmOutcome::Failed(error.to_string()),
    };

    let transaction = match registry.transaction(&cut.affected, lock_timeout).await {
        Ok(transaction) => transaction,
        Err(TransactionError::Timeout { .. }) => return ArmOutcome::Deferred(CutDeferReason::Timeout),
        Err(error) => return ArmOutcome::Failed(error.to_string()),
    };
    let statuses = transaction.statuses();

    if let Some(recorded) = cut.generation.cut() {
        return republish(transaction, &statuses, cut, recorded.clone());
    }

    let ahead = statuses
        .iter()
        .filter_map(|(_, status)| clock_ahead(status))
        .max()
        .filter(|ahead| *ahead > cut.margin);
    if let Some(by) = ahead {
        transaction.abandon();
        return ArmOutcome::Deferred(CutDeferReason::ClockAhead { by });
    }

    let at = cut_time(&statuses, lcm, cut.margin);
    let boundary = CutBoundary {
        at,
        replaces: cut.old.id(),
        epochs: statuses.iter().map(|(id, status)| (*id, status.epoch)).collect(),
    };
    // Recording the boundary and publishing the cut run back to back, with every lock held and nothing that can fail
    // between them.
    match cut.generation.set_cut(boundary) {
        Ok(()) => {
            transaction.publish(AdmissionSchedule::Cut {
                old: cut.old.clone(),
                new: cut.generation.clone(),
                at,
            });
            ArmOutcome::Armed { at }
        }
        Err(recorded) => republish(transaction, &statuses, cut, recorded),
    }
}

/// Publishes the cut whose boundary the new generation's token already records, if it is still ahead of every
/// affected point: every point's epoch is the one the boundary records, and its latest stamp is before the cut time.
fn republish(
    transaction: ScheduleTransaction, statuses: &[(AdmissionPointId, AdmissionPointStatus)], cut: &ArmCut,
    recorded: CutBoundary,
) -> ArmOutcome {
    let ahead = statuses.iter().all(|(id, status)| {
        recorded.epoch_of(*id) == Some(status.epoch)
            && status.last_admitted_at.is_none_or(|latest| latest < recorded.at)
    });
    if !ahead {
        transaction.abandon();
        return ArmOutcome::Stranded(format!(
            "generation {} records a cut at {} that its schedule never published, and an affected admission point has \
             stamped past it since; it cannot be armed at another time",
            cut.generation.id(),
            recorded.at
        ));
    }
    transaction.publish(AdmissionSchedule::Cut {
        old: cut.old.clone(),
        new: cut.generation.clone(),
        at: recorded.at,
    });
    ArmOutcome::Armed { at: recorded.at }
}

/// Returns how far a point's clock runs ahead of its wall clock, if it does.
fn clock_ahead(status: &AdmissionPointStatus) -> Option<Duration> {
    let latest = status.last_admitted_at?;
    let ahead = latest.as_unix_nanos().checked_sub(status.wall_now.as_unix_nanos())?;
    Some(Duration::from_nanos(ahead))
}

/// Returns the cut time for the affected points' statuses: the first multiple of `lcm` seconds at least `margin` after
/// the latest of their latest stamps and wall clocks.
fn cut_time(statuses: &[(AdmissionPointId, AdmissionPointStatus)], lcm: NonZeroU64, margin: Duration) -> AdmissionTime {
    let now = statuses
        .iter()
        .map(|(_, status)| match status.last_admitted_at {
            Some(latest) => latest.max(status.wall_now),
            None => status.wall_now,
        })
        .max()
        .unwrap_or(AdmissionTime::UNIX_EPOCH);
    rules::cut_time(now, lcm, margin)
}

/// What cancelling a cut did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CancelOutcome {
    /// The cut is cancelled: no admission point had stamped its new generation, and this cancellation published
    /// `Steady(old)`.
    Cancelled,

    /// The cut was cancelled before: the schedule already holds `Steady(old)`, as an earlier run of the same
    /// cancellation left it. Nothing changed.
    AlreadyCancelled,

    /// The cut was not cancelled, for the reason given, and proceeds.
    Refused(String),
}

/// Cancels the cut of `generation`, publishing `Steady(old)` if no admission point stamped `generation`, taking the
/// locks of the points in `lock_order` within `lock_timeout`.
///
/// `lock_order` lists every admission point, those that feed no state owner first: a point's lock is held across its
/// batches' fan-out, so the points the cut is armed over are held only for the check and the store. A timeout refuses
/// the cancellation, which a cut can always do safely.
///
/// Cancelling a cut the schedule no longer holds because it holds `Steady(old)` reports it cancelled already, so that
/// a restarted worker can cancel again.
pub(super) async fn cancel(
    registry: &AdmissionRegistry, schedule: &ScheduleHandle, lock_order: &[AdmissionPointId], generation: GenerationId,
    old: &GenerationRef, lock_timeout: Duration,
) -> CancelOutcome {
    match &**schedule.load() {
        AdmissionSchedule::Cut { new, .. } if new.id() == generation => {}
        AdmissionSchedule::Steady(steady) if steady.id() == old.id() => return CancelOutcome::AlreadyCancelled,
        AdmissionSchedule::Steady(steady) if steady.id() == generation => {
            return CancelOutcome::Refused(format!("the cut of generation {} is complete", generation))
        }
        _ => {
            return CancelOutcome::Refused(format!(
                "the schedule holds neither the cut of generation {} nor generation {}",
                generation,
                old.id()
            ))
        }
    }

    let transaction = match registry.transaction(lock_order, lock_timeout).await {
        Ok(transaction) => transaction,
        Err(TransactionError::Timeout { point, timeout }) => {
            return CancelOutcome::Refused(format!(
                "admission point {} was not free within {:?}",
                point.get(),
                timeout
            ))
        }
        Err(error) => return CancelOutcome::Refused(error.to_string()),
    };
    let stamped = transaction
        .statuses()
        .into_iter()
        .find(|(_, status)| status.admitted_to_new)
        .map(|(id, _)| id);
    match stamped {
        Some(point) => {
            transaction.abandon();
            CancelOutcome::Refused(format!(
                "admission point {} has stamped generation {}",
                point.get(),
                generation
            ))
        }
        None => {
            transaction.publish(AdmissionSchedule::Steady(old.clone()));
            CancelOutcome::Cancelled
        }
    }
}

/// What one poll of the admission points found about a cut in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Polled {
    /// The cut's progress, for the core.
    pub progress: CutProgress,

    /// The latest wall clock a point read, if any point answered.
    pub wall: Option<AdmissionTime>,
}

/// Reads every admission point in `every` and in `affected`, waiting at most `within` for each, all at once.
///
/// A point that is busy for longer, such as one whose dispatcher waits on a full channel, or that is not registered,
/// counts as neither having stamped the new generation nor having passed the cut this time.
pub(super) async fn poll(
    registry: &AdmissionRegistry, every: &[AdmissionPointId], affected: &[AdmissionPointId], within: Duration,
) -> Polled {
    let mut points = every.to_vec();
    points.extend(affected.iter().filter(|point| !every.contains(point)));
    let statuses = join_all(points.iter().map(|point| registry.status_within(*point, within))).await;

    let mut polled = Polled {
        progress: CutProgress {
            stamped_new: false,
            passed: 0,
            affected: affected.len(),
        },
        wall: None,
    };
    for (point, status) in points.iter().zip(statuses) {
        let Some(status) = status else { continue };
        polled.progress.stamped_new |= status.admitted_to_new;
        if status.passed_cut && affected.contains(point) {
            polled.progress.passed += 1;
        }
        polled.wall = Some(polled.wall.map_or(status.wall_now, |wall| wall.max(status.wall_now)));
    }
    polled
}

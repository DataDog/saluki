//! The scheduling policy: the single home for the time-advancement rule.
//!
//! This is the Rust port of the Go observer's `observer/impl/scheduler.go`. The engine does not decide
//! *when* to run analysis; it asks a [`SchedulerPolicy`] on every observation and at end of stream, and
//! executes the [`AdvanceRequest`]s the policy returns. Keeping the rule in one small trait makes the
//! trigger logic directly testable and preserves the Go behavior exactly.
//!
//! # The current behavior
//!
//! [`CurrentBehaviorPolicy`] reproduces the Go `currentBehaviorPolicy`:
//!
//! - **On observation** at data second `T`: advance to `T - 1` *only if* `T - 1` is strictly newer than
//!   the last analyzed time. The most recent second is never analyzed before it is complete.
//! - **On idle**: nothing. The current behavior has no idle wall-clock advances; the hook exists so a
//!   future periodic-flush policy can be slotted in without touching the engine.
//! - **On replay end**: advance to the latest observed data time, if it is newer than the last analyzed
//!   time. This flushes the tail of a replay without inventing a recovery second.
//!
//! All times are **data time** (Unix seconds attached to observations), never wall-clock time.

/// Why the engine advanced analysis.
///
/// Ported from the Go `advanceReason` enum in `observer/impl/scheduler.go`; the string labels are the wire
/// labels used by the Go `advanceReasonString` helper (and therefore by advance logs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdvanceReason {
    /// Data arrival triggered the advance (the common path).
    Input,
    /// A periodic timer triggered the advance. Unused by [`CurrentBehaviorPolicy`], reserved for future
    /// idle-flush policies.
    PeriodicFlush,
    /// Replay finished and the remaining data was flushed.
    ReplayEnd,
    /// An explicit call, for example a test or debug trigger.
    Manual,
}

impl AdvanceReason {
    /// Returns the wire label used by the Go advance log (`input`, `periodic`, `replay_end`, `manual`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::PeriodicFlush => "periodic",
            Self::ReplayEnd => "replay_end",
            Self::Manual => "manual",
        }
    }
}

impl std::fmt::Display for AdvanceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request to advance analysis to `up_to_sec`, with the reason it was issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceRequest {
    /// The data second to advance analysis to, inclusive.
    pub up_to_sec: i64,
    /// Why the advance is being requested.
    pub reason: AdvanceReason,
}

/// The read-only scheduler-relevant part of the engine state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerState {
    /// The data time up to which detection has already run.
    pub last_analyzed_data_time: i64,
    /// The latest data timestamp seen across all ingested observations.
    pub latest_data_time: i64,
}

/// Decides when the engine should advance analysis.
///
/// Implementations must be pure functions of their inputs: the engine records no scheduling state beyond
/// the two fields in [`SchedulerState`], so a policy can be swapped freely (including in tests) without
/// affecting any other behavior.
pub trait SchedulerPolicy {
    /// Returns the advances to run after an observation arrives at data second `data_time_sec`.
    ///
    /// Returns an empty vector when nothing should advance.
    fn on_observation(&self, data_time_sec: i64, state: SchedulerState) -> Vec<AdvanceRequest>;

    /// Returns the advances to run when wall-clock time passes without new observations.
    ///
    /// The current behavior returns nothing; the hook supports future periodic flushes.
    fn on_idle(&self, now_unix_nano: i64, state: SchedulerState) -> Vec<AdvanceRequest>;

    /// Returns the final advances to run at end of stream, flushing any remaining data.
    fn on_replay_end(&self, state: SchedulerState) -> Vec<AdvanceRequest>;
}

/// The exact current scheduling semantics, ported from the Go `currentBehaviorPolicy`.
///
/// See the module documentation for the rule. The zero-sized type is stateless, so it is trivially
/// shareable and testable in isolation from the engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct CurrentBehaviorPolicy;

impl CurrentBehaviorPolicy {
    /// Creates the current-behavior policy.
    pub const fn new() -> Self {
        Self
    }
}

impl SchedulerPolicy for CurrentBehaviorPolicy {
    fn on_observation(&self, data_time_sec: i64, state: SchedulerState) -> Vec<AdvanceRequest> {
        let analyze_up_to = data_time_sec - 1;
        if analyze_up_to <= state.last_analyzed_data_time {
            return Vec::new();
        }
        vec![AdvanceRequest {
            up_to_sec: analyze_up_to,
            reason: AdvanceReason::Input,
        }]
    }

    fn on_idle(&self, _now_unix_nano: i64, _state: SchedulerState) -> Vec<AdvanceRequest> {
        Vec::new()
    }

    fn on_replay_end(&self, state: SchedulerState) -> Vec<AdvanceRequest> {
        if state.latest_data_time <= state.last_analyzed_data_time {
            return Vec::new();
        }
        vec![AdvanceRequest {
            up_to_sec: state.latest_data_time,
            reason: AdvanceReason::ReplayEnd,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ports `scheduler_test.go` `TestCurrentBehaviorPolicy_OnObservation`.
    #[test]
    fn observation_advances_when_data_time_is_ahead() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 10,
        };

        let requests = policy.on_observation(15, state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].up_to_sec, 14);
        assert_eq!(requests[0].reason, AdvanceReason::Input);
    }

    #[test]
    fn observation_does_not_advance_when_data_time_equals_last_analyzed_plus_one() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 10,
        };

        assert!(policy.on_observation(11, state).is_empty());
    }

    #[test]
    fn observation_does_not_advance_when_data_time_is_behind() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 10,
        };

        assert!(policy.on_observation(5, state).is_empty());
    }

    #[test]
    fn out_of_order_data_does_not_trigger_advance() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 20,
            latest_data_time: 25,
        };

        // Data arrives at t=15, which is behind last_analyzed_data_time.
        assert!(policy.on_observation(15, state).is_empty());
    }

    #[test]
    fn sparse_input_triggers_advance_across_gap() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 10,
        };

        // Data jumps from t=10 to t=1000; a single advance covers the whole gap.
        let requests = policy.on_observation(1000, state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].up_to_sec, 999);
    }

    #[test]
    fn advancing_timestamps_produce_sequential_advances() {
        let policy = CurrentBehaviorPolicy::new();
        let mut state = SchedulerState {
            last_analyzed_data_time: 0,
            latest_data_time: 0,
        };

        let requests = policy.on_observation(5, state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].up_to_sec, 4);

        // Simulate the engine executing the advance.
        state.last_analyzed_data_time = 4;
        state.latest_data_time = 5;

        let requests = policy.on_observation(6, state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].up_to_sec, 5);
    }

    /// Ports `scheduler_test.go` `TestCurrentBehaviorPolicy_OnIdle`.
    #[test]
    fn idle_never_advances() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 20,
        };

        assert!(policy.on_idle(99_999, state).is_empty());
    }

    /// Ports `scheduler_test.go` `TestCurrentBehaviorPolicy_OnReplayEnd`.
    #[test]
    fn replay_end_advances_to_latest_when_data_remains() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 10,
            latest_data_time: 20,
        };

        let requests = policy.on_replay_end(state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].up_to_sec, 20);
        assert_eq!(requests[0].reason, AdvanceReason::ReplayEnd);
    }

    #[test]
    fn replay_end_does_not_advance_when_already_caught_up() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 20,
            latest_data_time: 20,
        };

        assert!(policy.on_replay_end(state).is_empty());
    }

    #[test]
    fn replay_end_does_not_advance_when_latest_is_behind_analyzed() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState {
            last_analyzed_data_time: 25,
            latest_data_time: 20,
        };

        assert!(policy.on_replay_end(state).is_empty());
    }

    #[test]
    fn replay_end_does_not_advance_when_both_zero() {
        let policy = CurrentBehaviorPolicy::new();
        let state = SchedulerState::default();

        assert!(policy.on_replay_end(state).is_empty());
    }

    #[test]
    fn advance_reason_labels_match_go() {
        assert_eq!(AdvanceReason::Input.as_str(), "input");
        assert_eq!(AdvanceReason::PeriodicFlush.as_str(), "periodic");
        assert_eq!(AdvanceReason::ReplayEnd.as_str(), "replay_end");
        assert_eq!(AdvanceReason::Manual.as_str(), "manual");
        assert_eq!(AdvanceReason::Manual.to_string(), "manual");
    }
}

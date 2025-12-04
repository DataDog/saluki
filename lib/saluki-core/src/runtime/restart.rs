use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;
use tracing::debug;

/// Restart mode for child processes.
#[derive(Clone, Copy)]
pub enum RestartMode {
    /// Restarts the failed child process only.
    OneForOne,

    /// Restarts all child processes, including the failed one.
    OneForAll,
}

/// Restart strategy for a supervisor.
///
/// Defaults to one-to-one mode (only restart the failed process) and a restart intensity of 1 over a period of 5
/// seconds.
///
/// # Restarts and permanent failure
///
/// A supervisor will allow up to `intensity` process restarts, across all child processes, over a given `period`. When
/// this limit is exceeded, the supervisor will stop all child processes and return an error itself, indicating that the
/// supervisor has failed overall.
///
/// Permanent failure bubbles up to the parent supervisor, until reaching the root supervisor. Once permanent failure
/// reaches the root supervisor, and the root supervisor exceeds its own restart limits, the root supervisor will fail
/// and cease execution.
#[derive(Clone, Copy)]
pub struct RestartStrategy {
    mode: RestartMode,
    intensity: usize,
    period: Duration,
}

impl RestartStrategy {
    /// Creates a new `RestartStrategy` with the given mode, intensity, and period.
    pub const fn new(mode: RestartMode, intensity: usize, period: Duration) -> Self {
        Self {
            mode,
            intensity,
            period,
        }
    }

    /// Creates a new `RestartStrategy` with the one-to-one restart mode, and the default intensity/period.
    pub fn one_to_one() -> Self {
        Self {
            mode: RestartMode::OneForOne,
            ..Default::default()
        }
    }

    /// Creates a new `RestartStrategy` with the one-for-all restart mode, and the default intensity/period.
    pub fn one_for_all() -> Self {
        Self {
            mode: RestartMode::OneForAll,
            ..Default::default()
        }
    }

    /// Sets the restart intensity and period for the strategy.
    pub const fn with_intensity_and_period(mut self, intensity: usize, period: Duration) -> Self {
        self.intensity = intensity;
        self.period = period;
        self
    }
}

impl Default for RestartStrategy {
    fn default() -> Self {
        Self::new(RestartMode::OneForOne, 1, Duration::from_secs(5))
    }
}

pub(super) enum RestartAction {
    /// Execute a restart with the given mode.
    Restart(RestartMode),

    /// Supervisor must shutdown as the maximum number of restarts has been reached.
    Shutdown,
}

pub(super) struct RestartState {
    strategy: RestartStrategy,
    restart_history: VecDeque<Instant>,
}

impl RestartState {
    /// Creates a new `RestartState` with the given strategy.
    pub fn new(strategy: RestartStrategy) -> Self {
        Self {
            strategy,
            restart_history: VecDeque::with_capacity(strategy.intensity),
        }
    }

    /// Evaluates a restart based on the current state and determine the action the supervisor should take in response.
    pub fn evaluate_restart(&mut self) -> RestartAction {
        // Short circuit if our intensity is zero.
        if self.strategy.intensity == 0 {
            debug!("Restart strategy configured with restart intensity of zero, shutting down.");
            return RestartAction::Shutdown;
        }

        // Since we only keep track of the last `intensity` restarts, we simply need to check if the oldest restart
        // we're tracking is within `period` of the current time, and if the number of tracked restarts is equal to
        // `intensity`.
        //
        // When both of these are true, we have exceeded the restart intensity limit and must shutdown.
        let now = Instant::now();
        if self.restart_history.len() == self.strategy.intensity {
            let oldest = self.restart_history.front().expect("restart history cannot be empty");
            if now.saturating_duration_since(*oldest) < self.strategy.period {
                debug!(
                    "Restart limit exceeded ({} in {:?}), shutting down.",
                    self.strategy.intensity, self.strategy.period
                );
                return RestartAction::Shutdown;
            }

            // Remove the oldest restart from the history since it is outside the period.
            self.restart_history.pop_front();
        }

        // Track this latest restart.
        self.restart_history.push_back(now);

        debug!("Restart limit not exceeded, restarting worker.");
        RestartAction::Restart(self.strategy.mode)
    }
}

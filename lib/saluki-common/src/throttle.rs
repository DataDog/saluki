//! A windowed budget for throttling repeated log lines.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::warn;

/// Throttles repeated actions to a fixed number per window.
///
/// A failing writer otherwise logs on every failed payload, burying every other line and costing
/// real money in log volume. The budget resets when the window rolls over, and one notice is
/// logged when suppression starts so the gap in the log is explained.
pub struct Throttle {
    limit: u32,
    window: Duration,
    state: Mutex<State>,
}

struct State {
    window_start: Instant,
    used: u32,
    notice_pending: bool,
}

impl Throttle {
    /// Creates a throttle that allows `limit` actions per `window`.
    ///
    /// # Panics
    ///
    /// Panics if `limit` is zero, which would suppress everything including the notice cycle.
    pub fn new(limit: u32, window: Duration) -> Self {
        assert!(limit > 0, "throttle limit must be greater than zero");
        Self {
            limit,
            window,
            state: Mutex::new(State {
                window_start: Instant::now(),
                used: 0,
                notice_pending: true,
            }),
        }
    }

    /// Returns whether the caller should emit its message.
    ///
    /// Once the limit is spent within the current window, every further call returns `false` until
    /// the window rolls over; the first suppressed call logs one notice. The lock is
    /// poison-recovered so a panicking caller cannot stop the budget.
    pub fn allow(&self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&self, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.saturating_duration_since(state.window_start) >= self.window {
            state.window_start = now;
            state.used = 0;
            state.notice_pending = true;
        }

        if state.used < self.limit {
            state.used += 1;
            return true;
        }

        if state.notice_pending {
            state.notice_pending = false;
            warn!(
                limit = self.limit,
                window_secs = self.window.as_secs(),
                "Log lines are being throttled for the rest of the window; further messages are suppressed until it rolls over."
            );
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn allows_up_to_the_limit_then_suppresses() {
        let throttle = Throttle::new(3, Duration::from_secs(10));
        let start = Instant::now();

        assert!(throttle.allow_at(step(start, 0)));
        assert!(throttle.allow_at(step(start, 1)));
        assert!(throttle.allow_at(step(start, 2)));
        assert!(!throttle.allow_at(step(start, 3)));
        assert!(!throttle.allow_at(step(start, 4)));
    }

    #[test]
    fn window_rollover_refills_the_budget() {
        let throttle = Throttle::new(2, Duration::from_secs(10));
        let start = Instant::now();

        assert!(throttle.allow_at(step(start, 0)));
        assert!(throttle.allow_at(step(start, 1)));
        assert!(!throttle.allow_at(step(start, 5)));

        // Past the window: the budget refills.
        assert!(throttle.allow_at(step(start, 10)));
        assert!(throttle.allow_at(step(start, 11)));
        assert!(!throttle.allow_at(step(start, 12)));
    }

    #[test]
    fn calls_faster_than_the_window_still_throttle() {
        // All calls land inside the first window, so only the budget passes.
        let throttle = Throttle::new(2, Duration::from_secs(10));
        let start = Instant::now();
        let allowed: usize = (0..100).filter(|_| throttle.allow_at(start)).count();
        assert_eq!(allowed, 2);
    }
}

use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use metrics::gauge;
use process_memory::Querier;
use tracing::debug;

use super::MemoryGrant;

/// A process-wide memory limiter.
///
/// In many cases, it can be useful to know when the process has exceeded a certain memory usage threshold in order to
/// be able to react that: clearing caches, temporarily blocking requests, and so on.
///
/// `MemoryLimiter` watches the process's physical memory usage (Resident Set Size/Working Set) and keeps track of when
/// the usage exceeds the configured limit. Cooperating tasks call `wait_for_capacity` to participate in accepting
/// backpressure that can be used to throttle work that's responsible for allocating memory.
///
/// The backpressure is scaled based on how close the memory usage is to the configured limit, with a minimum and
/// maximum backoff duration that's applied. This means that until we're using a certain percentage of the configured
/// limit, no backpressure is applied. Once that threshold is crossed, backpressure is applied proportionally to how
/// close to the limit we're. Callers are never fully blocked even if the limit is reached.
#[derive(Clone)]
pub struct MemoryLimiter {
    active_backoff_nanos: Arc<AtomicU64>,
}

impl MemoryLimiter {
    /// Creates a new `MemoryLimiter` based on the given `MemoryGrant`.
    ///
    /// A background task is spawned that frequently checks the used memory for the entire process, and callers of
    /// `wait_for_capacity` will observe waiting once the memory usage exceeds the configured limit threshold. The
    /// waiting time will scale progressively the closer the memory usage is to the configured limit.
    ///
    /// Defaults to a 95% threshold (that's, threshold begins at 95% of the limit), a minimum backoff duration of 1 ms, and
    /// a maximum backoff duration of 25 ms. The effective limit of the grant is used as the memory limit.
    pub fn new(grant: MemoryGrant) -> Option<Self> {
        // Smoke test to see if we can even collect memory stats on this system.
        Querier::default().resident_set_size()?;

        let rss_limit = grant.effective_limit_bytes();
        let backoff_threshold = 0.95;
        let backoff_min = Duration::from_millis(1);
        let backoff_max = Duration::from_millis(25);

        let active_backoff_nanos = Arc::new(AtomicU64::new(0));
        let active_backoff_nanos2 = Arc::clone(&active_backoff_nanos);

        std::thread::Builder::new()
            .name("memory-limiter-checker".to_string())
            .spawn(move || {
                check_memory_usage(
                    active_backoff_nanos2,
                    rss_limit,
                    backoff_threshold,
                    backoff_min,
                    backoff_max,
                )
            })
            .unwrap();

        Some(Self { active_backoff_nanos })
    }

    /// Creates a no-op `MemoryLimiter` that doesn't perform any limiting.
    ///
    /// All calls to `wait_for_capacity` will return immediately.
    pub fn noop() -> Self {
        Self {
            active_backoff_nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Waits a short period of time based on the available memory.
    ///
    /// If the used memory isn't within the threshold of the configured limit, no waiting will occur. Otherwise, the
    /// call will wait a variable amount of time depending on how close to the configured limit the process is.
    pub async fn wait_for_capacity(&self) {
        let active_backoff_nanos = self.active_backoff_nanos.load(Relaxed);
        if active_backoff_nanos > 0 {
            tokio::time::sleep(Duration::from_nanos(active_backoff_nanos)).await;
        }
    }
}

fn check_memory_usage(
    active_backoff_nanos: Arc<AtomicU64>, rss_limit: usize, backoff_threshold: f64, backoff_min: Duration,
    backoff_max: Duration,
) {
    debug!("Memory limiter checker started.");

    let mut querier = Querier::default();

    loop {
        let actual_rss = querier
            .resident_set_size()
            .expect("memory statistics should be available");

        let maybe_backoff_duration =
            calculate_backoff(rss_limit, actual_rss, backoff_threshold, backoff_min, backoff_max);
        match maybe_backoff_duration {
            Some(backoff_dur) => {
                active_backoff_nanos.store(backoff_dur.as_nanos() as u64, Relaxed);

                debug!(rss_limit, actual_rss, current_backoff = ?backoff_dur, "Enforcing backoff due to memory pressure.");
                gauge!("memory_limiter.current_backoff_secs").set(backoff_dur.as_secs_f64());
            }
            None => {
                active_backoff_nanos.store(0, Relaxed);

                gauge!("memory_limiter.current_backoff_secs").set(0.0);
            }
        }

        std::thread::sleep(Duration::from_millis(250));
    }
}

fn calculate_backoff(
    rss_limit: usize, actual_rss: usize, backoff_threshold: f64, backoff_min: Duration, backoff_max: Duration,
) -> Option<Duration> {
    if actual_rss as f64 > rss_limit as f64 * backoff_threshold {
        // When we're over the threshold, we want to scale our backoff range across the remainder of the threshold.
        //
        // For example, if our minimum and maximum backoff durations are 100ms and 1000ms, respectively, and our limit
        // is 100MB with a threshold of 95% (0.95), then we would start backing off by 100ms when we hit 95MB, and we
        // would back off at a maximum of 1000ms at 100% (or greater) of our limit.
        //
        // Between those two points, we would spread the difference of the minimum/maximum backoff duration (1000ms -
        // 100ms => 900ms) across that 5%. Thus, we would expect that at 97.5% of the limit, we would be backing off by
        // 550ms. (0.5 * 900ms => 450ms, 100ms + 450ms => 550ms)
        let rss_backoff_range = rss_limit as f64 - (rss_limit as f64 * backoff_threshold);
        let backoff_duration_range = backoff_max - backoff_min;
        let threshold_delta = actual_rss as f64 - (rss_limit as f64 * backoff_threshold);
        let variable_backoff_duration = backoff_duration_range.as_secs_f64() * (threshold_delta / rss_backoff_range);

        let backoff = backoff_min + Duration::from_secs_f64(variable_backoff_duration);
        if backoff > backoff_max {
            Some(backoff_max)
        } else {
            Some(backoff)
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::calculate_backoff;

    // These cases reproduce the worked example from `calculate_backoff`'s own doc comment: minimum/maximum backoff
    // durations of 100ms/1000ms, a 100MB limit, and a 95% threshold. The doc promises specific values at specific
    // fractions of the limit, so we assert those exact durations rather than merely that a backoff was produced.
    const LIMIT: usize = 100_000_000;
    const THRESHOLD: f64 = 0.95;
    const MIN: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_millis(1000);

    fn backoff_at(actual_rss: usize) -> Option<Duration> {
        calculate_backoff(LIMIT, actual_rss, THRESHOLD, MIN, MAX)
    }

    #[test]
    fn no_backoff_below_or_at_threshold() {
        // Well below the threshold: no backpressure at all.
        assert_eq!(backoff_at(90_000_000), None);

        // Exactly at the 95% threshold: the comparison is strictly greater-than, so still no backoff.
        assert_eq!(backoff_at(95_000_000), None);
    }

    #[test]
    fn backoff_scales_linearly_across_the_threshold_band() {
        // The doc's worked example: at 97.5% of the limit we are halfway through the 95%..100% band, so the backoff is
        // the minimum (100ms) plus half of the 900ms range (450ms) => 550ms.
        assert_eq!(backoff_at(97_500_000), Some(Duration::from_millis(550)));
    }

    #[test]
    fn backoff_saturates_at_the_maximum() {
        // At exactly 100% of the limit we reach the far end of the band, which is the maximum backoff (1000ms).
        assert_eq!(backoff_at(100_000_000), Some(MAX));

        // Beyond the limit the computed backoff would exceed the maximum, so it is clamped to the maximum.
        assert_eq!(backoff_at(120_000_000), Some(MAX));
    }
}

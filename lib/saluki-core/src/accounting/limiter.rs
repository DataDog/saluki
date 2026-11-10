use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use metrics::{counter, gauge};
use process_memory::Querier;
use tracing::{debug, info, warn};

use super::MemoryGrant;

/// How often the checker thread samples the process's memory usage.
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// A process-wide memory limiter.
///
/// In many cases, it can be useful to know when the process has exceeded a certain memory usage threshold in order to
/// be able to react that: clearing caches, temporarily blocking requests, and so on.
///
/// `MemoryLimiter` watches the process's physical memory usage (Resident Set Size/Working Set) and keeps track of when
/// the usage approaches the configured limit. Cooperating tasks call `wait_for_capacity` to participate in accepting
/// backpressure that can be used to throttle work that's responsible for allocating memory.
///
/// The backpressure is scaled based on how close the memory usage is to the configured limit, with a minimum and
/// maximum backoff duration that's applied. Until usage crosses a starting threshold, no backpressure is applied. Past
/// that threshold, backpressure grows proportionally until it reaches its maximum at a second, higher threshold. Once
/// throttling has started, it continues at the minimum backoff until usage falls below a lower clearing threshold, so
/// that usage hovering around the starting threshold doesn't flap between throttling and not throttling. Callers are
/// never fully blocked, even if the limit is reached.
///
/// Throttling is logged when it starts and when it clears, and is reported through internal metrics: the current
/// backoff, the number of throttled calls, and the total time callers were throttled.
#[derive(Clone)]
pub struct MemoryLimiter {
    active_backoff_nanos: Arc<AtomicU64>,
}

impl MemoryLimiter {
    /// Creates a new `MemoryLimiter` based on the given `MemoryGrant`.
    ///
    /// A background task is spawned that frequently checks the used memory for the entire process, and callers of
    /// `wait_for_capacity` will observe waiting once the memory usage approaches the grant's initial limit. The waiting
    /// time will scale progressively the closer the memory usage is to the limit.
    ///
    /// The thresholds are measured against the grant's initial limit rather than its effective limit: the slop factor
    /// is headroom for memory bounds verification, and applying it here as well would start throttling well before the
    /// process is actually close to its limit. Throttling starts above 85% of the limit, reaches its maximum at 95%, and
    /// clears once usage falls to 80% or below. The backoff ranges from 1 ms to 25 ms.
    pub fn new(grant: MemoryGrant) -> Option<Self> {
        // Smoke test to see if we can even collect memory stats on this system.
        Querier::default().resident_set_size()?;

        let policy = BackoffPolicy {
            rss_limit: grant.initial_limit_bytes(),
            start_ratio: 0.85,
            full_ratio: 0.95,
            clear_ratio: 0.80,
            backoff_min: Duration::from_millis(1),
            backoff_max: Duration::from_millis(25),
        };

        let active_backoff_nanos = Arc::new(AtomicU64::new(0));
        let active_backoff_nanos2 = Arc::clone(&active_backoff_nanos);

        std::thread::Builder::new()
            .name("memory-limiter-checker".to_string())
            .spawn(move || check_memory_usage(active_backoff_nanos2, policy))
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
            // The metric handles are only looked up while throttling, where the lookup is negligible next to the sleep
            // itself, so the unthrottled path stays a single atomic load.
            counter!("memory_limiter.throttled_calls_total").increment(1);
            counter!("memory_limiter.throttled_nanoseconds_total").increment(active_backoff_nanos);

            tokio::time::sleep(Duration::from_nanos(active_backoff_nanos)).await;
        }
    }
}

/// When, and how hard, the memory limiter throttles callers.
#[derive(Clone, Copy, Debug)]
struct BackoffPolicy {
    /// Memory limit, in bytes, that the ratios are measured against.
    rss_limit: usize,

    /// Fraction of the limit above which throttling starts, at the minimum backoff.
    start_ratio: f64,

    /// Fraction of the limit at which the backoff reaches its maximum.
    full_ratio: f64,

    /// Fraction of the limit at or below which active throttling clears.
    clear_ratio: f64,

    /// Backoff applied just above the starting threshold, and while throttling hasn't yet cleared.
    backoff_min: Duration,

    /// Backoff applied at or above the full threshold.
    backoff_max: Duration,
}

impl BackoffPolicy {
    /// Calculates the backoff to apply for the given memory usage.
    ///
    /// `throttling` is whether a backoff was applied for the previous sample. While usage sits between the clearing and
    /// starting thresholds, it keeps throttling active at the minimum backoff.
    fn calculate_backoff(&self, actual_rss: usize, throttling: bool) -> Option<Duration> {
        let limit = self.rss_limit as f64;
        let actual = actual_rss as f64;
        let start = limit * self.start_ratio;

        if actual > start {
            // Spread the backoff range across the band between the starting and full thresholds.
            //
            // For example, with minimum/maximum backoff durations of 100ms/1000ms, a 100MB limit, and a band of 85% to
            // 95%, we back off by just over 100ms past 85MB, and by 1000ms at 95MB or above. At 90MB, halfway through
            // the band, we back off by 100ms + (0.5 * 900ms) => 550ms.
            let band = limit * (self.full_ratio - self.start_ratio);
            let position = ((actual - start) / band).min(1.0);
            let backoff_range = self.backoff_max - self.backoff_min;
            Some(self.backoff_min + backoff_range.mul_f64(position))
        } else if throttling && actual > limit * self.clear_ratio {
            Some(self.backoff_min)
        } else {
            None
        }
    }
}

fn check_memory_usage(active_backoff_nanos: Arc<AtomicU64>, policy: BackoffPolicy) {
    debug!("Memory limiter checker started.");

    let mut querier = Querier::default();
    let mut throttling = false;

    loop {
        let actual_rss = querier
            .resident_set_size()
            .expect("memory statistics should be available");

        let maybe_backoff = policy.calculate_backoff(actual_rss, throttling);
        log_transition(&policy, actual_rss, throttling, maybe_backoff);
        throttling = maybe_backoff.is_some();

        let backoff = maybe_backoff.unwrap_or_default();
        active_backoff_nanos.store(backoff.as_nanos() as u64, Relaxed);
        gauge!("memory_limiter.current_backoff_secs").set(backoff.as_secs_f64());

        if throttling {
            debug!(rss_limit = policy.rss_limit, actual_rss, current_backoff = ?backoff, "Enforcing backoff due to memory pressure.");
        }

        std::thread::sleep(CHECK_INTERVAL);
    }
}

/// Logs when throttling starts or clears.
///
/// Logging only on transitions keeps sustained memory pressure from flooding the logs while still surfacing it at the
/// default log level. The clearing threshold keeps usage that hovers around the starting threshold from logging on
/// every check.
fn log_transition(policy: &BackoffPolicy, actual_rss: usize, was_throttling: bool, backoff: Option<Duration>) {
    let usage_pct = actual_rss as f64 / policy.rss_limit as f64 * 100.0;
    match (was_throttling, backoff) {
        (false, Some(backoff)) => warn!(
            rss_limit = policy.rss_limit,
            actual_rss,
            current_backoff = ?backoff,
            "Memory usage is at {:.1}% of the memory limit. Throttling ingestion until it falls to {:.0}%.",
            usage_pct,
            policy.clear_ratio * 100.0,
        ),
        (true, None) => info!(
            rss_limit = policy.rss_limit,
            actual_rss, "Memory usage is at {:.1}% of the memory limit. No longer throttling ingestion.", usage_pct,
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BackoffPolicy;

    // These cases reproduce the worked example from `calculate_backoff`'s own comment: minimum/maximum backoff durations
    // of 100ms/1000ms, a 100MB limit, and a band of 85% to 95%, clearing at 80%. The comment promises specific values at
    // specific fractions of the limit, so we assert those exact durations rather than merely that a backoff was
    // produced.
    const MIN: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_millis(1000);
    const POLICY: BackoffPolicy = BackoffPolicy {
        rss_limit: 100_000_000,
        start_ratio: 0.85,
        full_ratio: 0.95,
        clear_ratio: 0.80,
        backoff_min: MIN,
        backoff_max: MAX,
    };

    fn backoff_at(actual_rss: usize) -> Option<Duration> {
        POLICY.calculate_backoff(actual_rss, false)
    }

    fn backoff_while_throttling_at(actual_rss: usize) -> Option<Duration> {
        POLICY.calculate_backoff(actual_rss, true)
    }

    #[test]
    fn no_backoff_below_or_at_starting_threshold() {
        // Well below the threshold: no backpressure at all.
        assert_eq!(backoff_at(70_000_000), None);

        // Between the clearing and starting thresholds, throttling doesn't start.
        assert_eq!(backoff_at(83_000_000), None);

        // Exactly at the 85% threshold: the comparison is strictly greater-than, so still no backoff.
        assert_eq!(backoff_at(85_000_000), None);
    }

    #[test]
    fn backoff_scales_linearly_across_the_band() {
        // Halfway through the 85%..95% band, the backoff is the minimum (100ms) plus half of the 900ms range (450ms).
        assert_eq!(backoff_at(90_000_000), Some(Duration::from_millis(550)));
    }

    #[test]
    fn backoff_saturates_at_the_full_threshold() {
        // At exactly 95% of the limit we reach the far end of the band, which is the maximum backoff.
        assert_eq!(backoff_at(95_000_000), Some(MAX));

        // Beyond the full threshold, and beyond the limit itself, the backoff is clamped to the maximum.
        assert_eq!(backoff_at(99_000_000), Some(MAX));
        assert_eq!(backoff_at(120_000_000), Some(MAX));
    }

    #[test]
    fn active_throttling_holds_the_minimum_until_usage_clears() {
        // Once throttling, falling back below the starting threshold keeps the minimum backoff, so that usage hovering
        // around the starting threshold doesn't toggle throttling on every check.
        assert_eq!(backoff_while_throttling_at(85_000_000), Some(MIN));
        assert_eq!(backoff_while_throttling_at(81_000_000), Some(MIN));

        // At or below the clearing threshold, throttling stops.
        assert_eq!(backoff_while_throttling_at(80_000_000), None);
        assert_eq!(backoff_while_throttling_at(50_000_000), None);

        // Above the starting threshold, the backoff follows the band regardless of the previous state.
        assert_eq!(
            backoff_while_throttling_at(90_000_000),
            Some(Duration::from_millis(550))
        );
    }
}

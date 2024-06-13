use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use metrics::gauge;
use tracing::debug;

use crate::MemoryGrant;

/// A process-wide memory limiter.
///
/// In many cases, it can be useful to know when the process has exceeded a certain memory usage threshold in order to
/// be able to react that: clearing caches, temporarily blocking requests, and so on.
///
/// `MemoryLimiter` watches the process's physical memory usage (Resident Set Size/Working Set) and keeps track of when
/// the usage exceeds the configured limit. Cooperating tasks call `wait_for_capacity` to participate in accepting
/// backpressure that can be used to throttle work that is responsible for allocating memory.
///
/// The backpressure is scaled based on how close the memory usage is to the configured limit, with a minimum and
/// maximum backoff duration that is applied. This means that until we are using a certain percentage of the configured
/// limit, no backpressure is applied. Once that threshold is crossed, backpressure is applied proportionally to how
/// close to the limit we are. Callers are never fully blocked even if the limit is reached.
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
    /// Defaults to a 95% threshold (i.e. threshold begins at 95% of the limit), a minimum backoff duration of 1ms, and
    /// a maximum backoff duration of 25ms. The effective limit of the grant is used as the memory limit.
    pub fn new(grant: MemoryGrant) -> Option<Self> {
        // Smoke test to see if we can even collect memory stats on this system.
        memory_stats::memory_stats()?;

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

    /// Creates a no-op `MemoryLimiter` that does not perform any limiting.
    ///
    /// All calls to `wait_for_capacity` will return immediately.
    pub fn noop() -> Self {
        Self {
            active_backoff_nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Waits a short period of time based on the available memory.
    ///
    /// If the used memory is not within the threshold of the configured limit, no waiting will occur. Otherwise, the
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

    loop {
        let mem_stats = memory_stats::memory_stats().expect("memory statistics should be available");
        let actual_rss = mem_stats.physical_mem;

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

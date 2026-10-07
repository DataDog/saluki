//! Eviction policy adapted from Ryan Hall's foldspace-eviction in PR #59.
//! Source: 3fd33bdb323db6d5720e4d607f93622cbd806e9a.
//! Kept in the packaged core until the shared crate lands.
//!
//! Port target: `pkg/logs/patterns/eviction/{score,eviction,eviction_manager}.go`,
//! which the Agent likewise shares between its pattern table and its tag
//! dictionary.
//!
//! Scoring is LFU with power-law age decay and a hyperbolic recency boost.
//! Eviction is triggered by a dual watermark on both item count and estimated
//! memory: crossing the high watermark evicts back down to the low watermark,
//! so the cost is amortized instead of paid on every insert.
//!
//! The crate is std-only and reads no clock: `now` is a parameter on every
//! scoring call, so a caller inside a sans-I/O crate can supply the reading it
//! was handed.

#![deny(warnings)]

use std::time::{Duration, Instant};

/// Which limit triggered eviction, and therefore how much to remove.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strategy {
    /// Both limits are satisfied; nothing to do.
    None,
    /// Item count crossed the high watermark; evict a fixed number.
    ByCount,
    /// Estimated memory crossed the high watermark; evict until enough bytes
    /// are freed. Takes priority over count when both are over.
    ByBytes,
}

/// Eviction thresholds. Defaults mirror the Agent's `logs_config.patterns.*`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvictionConfig {
    /// Item budget; protected entries can exceed the eviction target.
    pub max_item_count: usize,
    /// Estimated byte budget; protected entries can exceed the eviction target.
    pub max_memory_bytes: i64,
    /// Fraction of a limit at which eviction triggers (`eviction_high_watermark`).
    pub high_watermark: f64,
    /// Fraction of a limit to evict back down to (`eviction_low_watermark`).
    pub low_watermark: f64,
    /// Exponent on the age term; higher forgets old items faster
    /// (`age_decay_factor`).
    pub age_decay_factor: f64,
    /// Newly created items are ineligible for eviction for this long, so an
    /// item always gets a chance to accumulate hits before being scored
    /// against established ones (`eviction_grace_period_seconds`).
    pub grace_period: Duration,
    /// Idle span after which an item is dropped whatever the watermarks say.
    ///
    /// Scoring only runs under pressure, so a table below its watermarks holds
    /// everything it has ever seen and replays all of it in every snapshot.
    /// This is the bound on that: an item nothing has touched for this long is
    /// worth less than the bytes it costs to re-establish it on each stream.
    ///
    /// [`Duration::ZERO`] disables staleness, leaving the watermarks as the
    /// only limit.
    pub stale_after: Duration,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            max_item_count: 700,
            max_memory_bytes: 4 * 1024 * 1024,
            high_watermark: 0.95,
            low_watermark: 0.85,
            age_decay_factor: 0.5,
            grace_period: Duration::from_secs(30),
            stale_after: Duration::from_secs(2 * 60 * 60),
        }
    }
}

impl EvictionConfig {
    /// Whether each limit has crossed its high watermark.
    #[must_use]
    pub fn should_evict(&self, item_count: usize, estimated_bytes: i64) -> (bool, bool) {
        let count_over = item_count as f64 > self.max_item_count as f64 * self.high_watermark;
        let bytes_over =
            estimated_bytes as f64 > self.max_memory_bytes as f64 * self.high_watermark;
        (count_over, bytes_over)
    }

    /// How much to evict, given which limits are over. Memory pressure wins
    /// when both are over, since freeing bytes also frees count.
    #[must_use]
    pub fn eviction_targets(
        &self,
        item_count: usize,
        estimated_bytes: i64,
        count_over: bool,
        bytes_over: bool,
    ) -> (usize, i64, Strategy) {
        if bytes_over {
            let target = (self.max_memory_bytes as f64 * self.low_watermark) as i64;
            return (0, estimated_bytes - target, Strategy::ByBytes);
        }
        if count_over {
            let target = (self.max_item_count as f64 * self.low_watermark) as usize;
            return (
                item_count.saturating_sub(target).max(1),
                0,
                Strategy::ByCount,
            );
        }
        (0, 0, Strategy::None)
    }

    /// The instant an item created at `created_at` leaves the grace period and
    /// becomes eligible for scoring.
    #[must_use]
    pub fn eligible_at(&self, created_at: Instant) -> Instant {
        created_at
            .checked_add(self.grace_period)
            .unwrap_or(created_at)
    }

    /// Whether an item has gone untouched long enough to drop on idleness
    /// alone. The grace period applies here as it does to scoring: an item
    /// young enough to still be establishing itself is never stale.
    #[must_use]
    pub fn is_stale(&self, created_at: Instant, last_access_at: Instant, now: Instant) -> bool {
        if self.stale_after.is_zero() {
            return false;
        }
        if !self.grace_period.is_zero()
            && now.saturating_duration_since(created_at) < self.grace_period
        {
            return false;
        }
        now.saturating_duration_since(last_access_at) >= self.stale_after
    }
}

/// Eviction score; **lower means evict sooner**.
///
/// `score = (frequency / (1 + age_days)^decay) * (1 + recency_boost)`
///
/// Frequency is the primary signal, damped by a power law in age so that an
/// item's lifetime count cannot keep it resident forever. The recency boost
/// decays hyperbolically from 1.0 (just matched) toward 0.0, breaking ties in
/// favor of items still in active use.
///
/// Unlike the Go original this takes monotonic [`Instant`]s, so the clock-skew
/// clamps it needs are unnecessary; only the 365-day saturation remains.
#[must_use]
pub fn calculate_score(
    frequency: f64,
    created_at: Instant,
    last_access_at: Instant,
    now: Instant,
    decay_factor: f64,
) -> f64 {
    let age_days = (now.saturating_duration_since(created_at).as_secs_f64() / 86_400.0).min(365.0);
    let base = frequency / (1.0 + age_days).powf(decay_factor);

    let hours_since_access = now.saturating_duration_since(last_access_at).as_secs_f64() / 3600.0;
    let recency_boost = 1.0 / (1.0 + hours_since_access / 24.0);

    base * (1.0 + recency_boost)
}

/// Score an item, returning [`f64::MAX`] while it is inside the grace period
/// so that it sorts last and is never selected for eviction.
#[must_use]
pub fn score_with_grace(
    frequency: f64,
    created_at: Instant,
    last_access_at: Instant,
    now: Instant,
    config: &EvictionConfig,
) -> f64 {
    if !config.grace_period.is_zero()
        && now.saturating_duration_since(created_at) < config.grace_period
    {
        return f64::MAX;
    }
    calculate_score(
        frequency,
        created_at,
        last_access_at,
        now,
        config.age_decay_factor,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(now: Instant, d: Duration) -> Instant {
        now.checked_sub(d).expect("test instant underflow")
    }

    #[test]
    fn frequent_item_outscores_rare_one_of_equal_age() {
        let now = Instant::now();
        let created = ago(now, Duration::from_secs(3600));
        let frequent = calculate_score(1000.0, created, now, now, 0.5);
        let rare = calculate_score(5.0, created, now, now, 0.5);
        assert!(frequent > rare);
    }

    #[test]
    fn recent_access_outscores_stale_at_equal_frequency() {
        let now = Instant::now();
        let created = ago(now, Duration::from_secs(7 * 86_400));
        let active = calculate_score(100.0, created, now, now, 0.5);
        let idle = calculate_score(
            100.0,
            created,
            ago(now, Duration::from_secs(72 * 3600)),
            now,
            0.5,
        );
        assert!(active > idle);
    }

    #[test]
    fn age_decay_damps_lifetime_count() {
        let now = Instant::now();
        // An old item with 100x the count of a young one still loses once
        // the age term bites; this is what stops the table freezing.
        let old = calculate_score(
            10_000.0,
            ago(now, Duration::from_secs(365 * 86_400)),
            ago(now, Duration::from_secs(200 * 3600)),
            now,
            2.0,
        );
        let young = calculate_score(100.0, ago(now, Duration::from_secs(60)), now, now, 2.0);
        assert!(young > old, "young={young} old={old}");
    }

    #[test]
    fn grace_period_makes_new_items_ineligible() {
        let now = Instant::now();
        let config = EvictionConfig::default();
        let fresh = score_with_grace(1.0, ago(now, Duration::from_secs(5)), now, now, &config);
        assert_eq!(fresh, f64::MAX);

        let settled = score_with_grace(1.0, ago(now, Duration::from_secs(60)), now, now, &config);
        assert!(settled < f64::MAX);
    }

    #[test]
    fn watermarks_trigger_and_target_the_low_mark() {
        let config = EvictionConfig::default();
        // 700 * 0.95 = 665
        assert_eq!(config.should_evict(600, 0), (false, false));
        assert_eq!(config.should_evict(666, 0), (true, false));

        // Evict back to 700 * 0.85 = 595.
        let (n, bytes, strategy) = config.eviction_targets(666, 0, true, false);
        assert_eq!((n, bytes, strategy), (71, 0, Strategy::ByCount));
    }

    #[test]
    fn memory_pressure_takes_priority_over_count() {
        let config = EvictionConfig::default();
        let bytes = 4 * 1024 * 1024;
        let (n, to_free, strategy) = config.eviction_targets(700, bytes, true, true);
        assert_eq!(strategy, Strategy::ByBytes);
        assert_eq!(n, 0);
        // 4 MiB - (4 MiB * 0.85)
        assert_eq!(to_free, bytes - (bytes as f64 * 0.85) as i64);
    }

    #[test]
    fn count_eviction_always_removes_at_least_one() {
        let config = EvictionConfig {
            max_item_count: 10,
            ..EvictionConfig::default()
        };
        // 10 * 0.95 = 9.5, so 10 is over; target is 8, but never return 0.
        let (n, _, strategy) = config.eviction_targets(10, 0, true, false);
        assert_eq!(strategy, Strategy::ByCount);
        assert!(n >= 1);
    }

    #[test]
    fn eligible_at_is_the_end_of_the_grace_period() {
        let now = Instant::now();
        let config = EvictionConfig::default();
        assert_eq!(config.eligible_at(now), now + config.grace_period);
    }

    #[test]
    fn idleness_past_the_threshold_is_stale() {
        let now = Instant::now();
        let config = EvictionConfig::default();
        let created = ago(now, Duration::from_secs(30 * 86_400));

        let idle = ago(now, config.stale_after + Duration::from_secs(1));
        assert!(config.is_stale(created, idle, now));

        let touched = ago(now, config.stale_after - Duration::from_secs(1));
        assert!(!config.is_stale(created, touched, now));
    }

    #[test]
    fn a_zero_threshold_disables_staleness() {
        let now = Instant::now();
        let config = EvictionConfig {
            stale_after: Duration::ZERO,
            ..EvictionConfig::default()
        };
        let long_ago = ago(now, Duration::from_secs(365 * 86_400));
        assert!(!config.is_stale(long_ago, long_ago, now));
    }

    #[test]
    fn the_grace_period_spares_a_young_item_from_staleness() {
        let now = Instant::now();
        let config = EvictionConfig {
            grace_period: Duration::from_secs(600),
            stale_after: Duration::from_secs(60),
            ..EvictionConfig::default()
        };
        // Untouched for ten times the threshold, but still inside its grace
        // period, so it has not had its chance yet.
        let created = ago(now, Duration::from_secs(599));
        assert!(!config.is_stale(created, created, now));

        let settled = ago(now, Duration::from_secs(601));
        assert!(config.is_stale(settled, settled, now));
    }
}

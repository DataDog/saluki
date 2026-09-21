//! Sampler telemetry: per-decision counters, signature-count gauges, and rare-sampler counters.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

/// Interval between window reports.
const WINDOW: Duration = Duration::from_secs(10);

// Metric names are kept identical to the sampler metrics that customer dashboards are built on.
const METRIC_SAMPLER_SEEN: &str = "datadog.trace_agent.sampler.seen";
const METRIC_SAMPLER_KEPT: &str = "datadog.trace_agent.sampler.kept";
const METRIC_SAMPLER_SIZE: &str = "datadog.trace_agent.sampler.size";
const METRIC_RARE_HITS: &str = "datadog.trace_agent.sampler.rare.hits";
const METRIC_RARE_MISSES: &str = "datadog.trace_agent.sampler.rare.misses";
const METRIC_RARE_SHRINKS: &str = "datadog.trace_agent.sampler.rare.shrinks";

/// The sampler that decided a trace's fate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SamplerName {
    /// No sampler ran, or none claimed the decision.
    Unknown,
    Priority,
    NoPriority,
    Error,
    Rare,
    Probabilistic,
}

impl SamplerName {
    /// Returns the `sampler` tag value.
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Priority => "priority",
            Self::NoPriority => "no_priority",
            Self::Error => "error",
            Self::Rare => "rare",
            Self::Probabilistic => "probabilistic",
        }
    }

    /// Returns whether decisions under this name carry a `target_env` tag.
    fn carries_env_tag(self) -> bool {
        matches!(self, Self::Priority | Self::NoPriority | Self::Error | Self::Rare)
    }
}

/// Returns the `sampling_priority` tag value for a sampling priority.
fn priority_tag_value(priority: i32) -> &'static str {
    match priority {
        -1 => "manual_drop",
        0 => "auto_drop",
        1 => "auto_keep",
        2 => "manual_keep",
        _ => "none",
    }
}

/// The trace dimensions a decision is counted under.
///
/// Service and env are `MetaString` so typical names clone without heap allocation on the per-trace
/// recording path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct DecisionKey {
    pub(super) sampler: SamplerName,
    pub(super) service: MetaString,
    pub(super) env: MetaString,
    pub(super) priority: Option<i32>,
}

impl DecisionKey {
    /// Assembles the metric tags for this key.
    ///
    /// The conditional tags are the contract: the priority tag belongs to the priority sampler
    /// only, the env tag to the adaptive samplers, and empty dimensions are omitted.
    fn tags(&self) -> Vec<(&'static str, String)> {
        let mut tags = Vec::with_capacity(4);
        tags.push(("sampler", self.sampler.as_str().to_string()));
        if self.sampler == SamplerName::Priority {
            if let Some(priority) = self.priority {
                tags.push(("sampling_priority", priority_tag_value(priority).to_string()));
            }
        }
        if !self.service.is_empty() {
            tags.push(("target_service", self.service.to_string()));
        }
        if !self.env.is_empty() && self.sampler.carries_env_tag() {
            tags.push(("target_env", self.env.to_string()));
        }
        tags
    }
}

/// Windowed decision counts for one key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DecisionCounts {
    /// Traces the sampler evaluated.
    pub(super) seen: u64,
    /// Traces the sampler kept.
    pub(super) kept: u64,
}

/// Sampler telemetry.
///
/// Decisions accumulate in a map for one window; [`Telemetry::flush`] reports every entry as
/// seen/kept delta counters, publishes the signature-count gauges and the rare-sampler counters,
/// and resets the window. Hits and misses are window deltas; shrinks accumulate for the process
/// lifetime and are published as a gauge. Cloning shares the accumulators, so the recording path,
/// the rare sampler, and the flush loop all see the same window.
#[derive(Clone)]
pub(super) struct Telemetry {
    inner: Arc<TelemetryInner>,
}

struct TelemetryInner {
    decisions: Mutex<FastHashMap<DecisionKey, DecisionCounts>>,
    // Absent in test instances, which accumulate but never emit.
    builder: Option<MetricsBuilder>,
    rare_hits: AtomicU64,
    rare_misses: AtomicU64,
    rare_shrinks: AtomicU64,
    priority_tracked: AtomicI64,
    no_priority_tracked: AtomicI64,
    error_tracked: AtomicI64,
}

impl Telemetry {
    /// Creates a telemetry that reports through the given metrics builder.
    pub(super) fn new(builder: &MetricsBuilder) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                decisions: Mutex::new(FastHashMap::default()),
                builder: Some(builder.clone()),
                rare_hits: AtomicU64::new(0),
                rare_misses: AtomicU64::new(0),
                rare_shrinks: AtomicU64::new(0),
                priority_tracked: AtomicI64::new(0),
                no_priority_tracked: AtomicI64::new(0),
                error_tracked: AtomicI64::new(0),
            }),
        }
    }

    /// Creates a telemetry for tests: decisions accumulate and can be inspected, but nothing is
    /// emitted.
    #[cfg(test)]
    pub(super) fn for_tests() -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                decisions: Mutex::new(FastHashMap::default()),
                builder: None,
                rare_hits: AtomicU64::new(0),
                rare_misses: AtomicU64::new(0),
                rare_shrinks: AtomicU64::new(0),
                priority_tracked: AtomicI64::new(0),
                no_priority_tracked: AtomicI64::new(0),
                error_tracked: AtomicI64::new(0),
            }),
        }
    }

    /// Records one sampler decision.
    ///
    /// The priority is carried only by the priority sampler; every other sampler counts under a
    /// key without it.
    pub(super) fn record_decision(
        &self, kept: bool, sampler: SamplerName, priority: i32, service: MetaString, env: MetaString,
    ) {
        let key = DecisionKey {
            sampler,
            service,
            env,
            priority: (sampler == SamplerName::Priority).then_some(priority),
        };
        let mut decisions = self.inner.decisions.lock().unwrap();
        let counts = decisions.entry(key).or_default();
        counts.seen += 1;
        counts.kept += u64::from(kept);
    }

    /// Records a rare-sampler keep.
    pub(super) fn record_rare_hit(&self) {
        self.inner.rare_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a rare-sampler drop.
    pub(super) fn record_rare_miss(&self) {
        self.inner.rare_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a rare-sampler signature-table shrink.
    pub(super) fn record_rare_shrink(&self) {
        self.inner.rare_shrinks.fetch_add(1, Ordering::Relaxed);
    }

    /// Publishes the tracked-signature counts of the adaptive samplers.
    pub(super) fn set_tracked_signature_counts(&self, priority: i64, no_priority: i64, error: i64) {
        self.inner.priority_tracked.store(priority, Ordering::Relaxed);
        self.inner.no_priority_tracked.store(no_priority, Ordering::Relaxed);
        self.inner.error_tracked.store(error, Ordering::Relaxed);
    }

    /// Reports the window and starts a new one.
    ///
    /// Returns the drained entries so tests can assert what a window held; emission itself goes
    /// through the metrics builder and is skipped in test instances.
    pub(super) fn flush(&self) -> Vec<(DecisionKey, DecisionCounts)> {
        let entries: Vec<_> = {
            let mut decisions = self.inner.decisions.lock().unwrap();
            std::mem::take(&mut *decisions).into_iter().collect()
        };

        if let Some(builder) = self.inner.builder.as_ref() {
            for (key, counts) in entries.iter() {
                let tags = key.tags();
                if counts.seen > 0 {
                    builder
                        .register_counter_with_tags(METRIC_SAMPLER_SEEN, tags.clone())
                        .increment(counts.seen);
                }
                if counts.kept > 0 {
                    builder
                        .register_counter_with_tags(METRIC_SAMPLER_KEPT, tags)
                        .increment(counts.kept);
                }
            }

            for (name, tracked) in [
                (
                    SamplerName::Priority,
                    self.inner.priority_tracked.load(Ordering::Relaxed),
                ),
                (
                    SamplerName::NoPriority,
                    self.inner.no_priority_tracked.load(Ordering::Relaxed),
                ),
                (SamplerName::Error, self.inner.error_tracked.load(Ordering::Relaxed)),
            ] {
                builder
                    .register_gauge_with_tags(METRIC_SAMPLER_SIZE, [("sampler", name.as_str().to_string())])
                    .set(tracked as f64);
            }

            let hits = self.inner.rare_hits.swap(0, Ordering::Relaxed);
            if hits > 0 {
                builder.register_counter(METRIC_RARE_HITS).increment(hits);
            }
            let misses = self.inner.rare_misses.swap(0, Ordering::Relaxed);
            if misses > 0 {
                builder.register_counter(METRIC_RARE_MISSES).increment(misses);
            }
            builder
                .register_gauge(METRIC_RARE_SHRINKS)
                .set(self.inner.rare_shrinks.load(Ordering::Relaxed) as f64);
        } else {
            // Test instances still observe window semantics: deltas reset, the gauge does not.
            self.inner.rare_hits.swap(0, Ordering::Relaxed);
            self.inner.rare_misses.swap(0, Ordering::Relaxed);
        }

        entries
    }

    /// Runs the window flush loop.
    ///
    /// The sampler transform is built once per process, so the loop runs for the process lifetime.
    pub(super) async fn run_flush_loop(self) {
        let mut window = tokio::time::interval(WINDOW);
        loop {
            window.tick().await;
            self.flush();
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot_decisions(&self) -> FastHashMap<DecisionKey, DecisionCounts> {
        self.inner.decisions.lock().unwrap().clone()
    }

    #[cfg(test)]
    fn rare_totals(&self) -> (u64, u64, u64) {
        (
            self.inner.rare_hits.load(Ordering::Relaxed),
            self.inner.rare_misses.load(Ordering::Relaxed),
            self.inner.rare_shrinks.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(telemetry: &Telemetry, kept: bool, sampler: SamplerName, service: &str, env: &str) {
        telemetry.record_decision(kept, sampler, 1, MetaString::from(service), MetaString::from(env));
    }

    #[test]
    fn decision_tags_follow_the_conditional_rules() {
        let priority = DecisionKey {
            sampler: SamplerName::Priority,
            service: MetaString::from("checkout"),
            env: MetaString::from("prod"),
            priority: Some(2),
        };
        assert_eq!(
            priority.tags(),
            vec![
                ("sampler", "priority".to_string()),
                ("sampling_priority", "manual_keep".to_string()),
                ("target_service", "checkout".to_string()),
                ("target_env", "prod".to_string()),
            ]
        );

        // The probabilistic sampler carries no priority and no env tag.
        let probabilistic = DecisionKey {
            sampler: SamplerName::Probabilistic,
            service: MetaString::from("checkout"),
            env: MetaString::from("prod"),
            priority: None,
        };
        assert_eq!(
            probabilistic.tags(),
            vec![
                ("sampler", "probabilistic".to_string()),
                ("target_service", "checkout".to_string())
            ]
        );

        // Empty dimensions are omitted; unknown priorities fall back to "none".
        let unknown_priority = DecisionKey {
            sampler: SamplerName::Priority,
            service: MetaString::empty(),
            env: MetaString::empty(),
            priority: Some(9),
        };
        assert_eq!(
            unknown_priority.tags(),
            vec![
                ("sampler", "priority".to_string()),
                ("sampling_priority", "none".to_string())
            ]
        );
    }

    #[test]
    fn decisions_accumulate_per_key_and_flush_resets_the_window() {
        let telemetry = Telemetry::for_tests();
        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        record(&telemetry, false, SamplerName::Probabilistic, "checkout", "prod");
        record(&telemetry, true, SamplerName::Probabilistic, "payments", "prod");

        let snapshot = telemetry.snapshot_decisions();
        let checkout = snapshot
            .iter()
            .find(|(key, _)| key.service == "checkout")
            .map(|(_, counts)| *counts)
            .unwrap();
        assert_eq!(checkout, DecisionCounts { seen: 2, kept: 1 });

        let drained = telemetry.flush();
        assert_eq!(drained.len(), 2);
        assert!(telemetry.snapshot_decisions().is_empty());

        // A second, quiet window drains nothing.
        assert!(telemetry.flush().is_empty());
    }

    #[test]
    fn rare_counters_reset_on_flush_but_shrinks_accumulate() {
        let telemetry = Telemetry::for_tests();
        telemetry.record_rare_hit();
        telemetry.record_rare_hit();
        telemetry.record_rare_miss();
        telemetry.record_rare_shrink();
        assert_eq!(telemetry.rare_totals(), (2, 1, 1));

        telemetry.flush();
        assert_eq!(telemetry.rare_totals(), (0, 0, 1));

        telemetry.record_rare_shrink();
        telemetry.flush();
        assert_eq!(telemetry.rare_totals(), (0, 0, 2));
    }

    #[test]
    fn only_the_priority_sampler_carries_the_priority_dimension() {
        let telemetry = Telemetry::for_tests();
        record(&telemetry, true, SamplerName::NoPriority, "checkout", "prod");
        let snapshot = telemetry.snapshot_decisions();
        let (key, _) = snapshot.iter().next().unwrap();
        assert_eq!(key.priority, None);
    }
}

//! Sampler telemetry: per-decision counters, signature-count gauges, and rare-sampler counters.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use saluki_common::collections::FastHashMap;
use saluki_metrics::{Counter, Gauge, MetricsBuilder};
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

/// Cap on distinct decision keys held in one window.
///
/// Ten times the priority catalog's default capacity. Once a window holds this many keys, unseen
/// combinations count under one overflow bucket per sampler while existing keys keep their own
/// buckets, so burst cardinality cannot grow the map without bound. Past this many combinations
/// in one window, per-combination counts carry no actionable signal. Deliberately not a tuning
/// knob.
const MAX_DECISION_KEYS: usize = 50_000;

/// Service tag value marking decisions rolled into a sampler's overflow bucket.
const OVERFLOW_SERVICE: &str = "other";

/// Quiet windows a decision's cached metric handles survive after its last traffic.
///
/// The internal metrics registry releases an idle series a few seconds after its handles go
/// away, and windows are ten seconds apart, so handles must be held between windows or the
/// series churns out between reports and slower readers see them missing, or their counts reset.
/// Handles held past this grace let the registry reclaim series that stayed quiet, bounding the
/// cache to recently active keys.
const HANDLE_GRACE_WINDOWS: u32 = 3;

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
/// and resets the window. The window map is capped at [`MAX_DECISION_KEYS`] entries; combinations
/// beyond the cap count under one overflow bucket per sampler. Hits and misses are window deltas;
/// shrinks accumulate for the process lifetime and are published as a gauge. Metric handles are
/// cached across flushes so the series stay registered between windows, and handles for decisions
/// that stay quiet are released after [`HANDLE_GRACE_WINDOWS`] windows. Cloning shares the
/// accumulators, so the recording path, the rare sampler, and the flush loop all see the same
/// window.
#[derive(Clone)]
pub(super) struct Telemetry {
    inner: Arc<TelemetryInner>,
}

struct TelemetryInner {
    decisions: Mutex<FastHashMap<DecisionKey, DecisionCounts>>,
    // Absent in test instances, which accumulate but never emit.
    emitter: Option<TelemetryEmitter>,
    max_decision_keys: usize,
    rare_hits: AtomicU64,
    rare_misses: AtomicU64,
    rare_shrinks: AtomicU64,
    priority_tracked: AtomicI64,
    no_priority_tracked: AtomicI64,
    error_tracked: AtomicI64,
}

/// The emission side of telemetry: the builder, the cached handles for decision metrics, and the
/// handles for the metrics reported every window.
struct TelemetryEmitter {
    builder: MetricsBuilder,
    handles: Mutex<FastHashMap<DecisionKey, TrackedHandles>>,
    fixed: FixedHandles,
}

/// Cached metric handles for one decision key.
pub(super) struct TrackedHandles {
    seen: Counter,
    // Created on the first kept decision, so keys that keep nothing never emit a kept series.
    kept: Option<Counter>,
    // Windows since the key last appeared in a report.
    idle_windows: u32,
}

impl TrackedHandles {
    fn new(builder: &MetricsBuilder, key: &DecisionKey) -> Self {
        Self {
            seen: builder.register_counter_with_tags(METRIC_SAMPLER_SEEN, key.tags()),
            kept: None,
            idle_windows: 0,
        }
    }
}

/// Handles for the metrics reported on every window, independent of individual decisions.
struct FixedHandles {
    priority_size: Gauge,
    no_priority_size: Gauge,
    error_size: Gauge,
    rare_hits: Counter,
    rare_misses: Counter,
    rare_shrinks: Gauge,
}

impl FixedHandles {
    fn new(builder: &MetricsBuilder) -> Self {
        let size = |sampler: SamplerName| {
            builder.register_gauge_with_tags(METRIC_SAMPLER_SIZE, [("sampler", sampler.as_str().to_string())])
        };
        Self {
            priority_size: size(SamplerName::Priority),
            no_priority_size: size(SamplerName::NoPriority),
            error_size: size(SamplerName::Error),
            rare_hits: builder.register_counter(METRIC_RARE_HITS),
            rare_misses: builder.register_counter(METRIC_RARE_MISSES),
            rare_shrinks: builder.register_gauge(METRIC_RARE_SHRINKS),
        }
    }
}

impl Telemetry {
    /// Creates a telemetry that reports through the given metrics builder.
    pub(super) fn new(builder: &MetricsBuilder) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                decisions: Mutex::new(FastHashMap::default()),
                emitter: Some(TelemetryEmitter {
                    builder: builder.clone(),
                    handles: Mutex::new(FastHashMap::default()),
                    fixed: FixedHandles::new(builder),
                }),
                max_decision_keys: MAX_DECISION_KEYS,
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
        Self::for_tests_inner(MAX_DECISION_KEYS)
    }

    /// Creates a telemetry for tests with a reduced decision-key cap, for exercising the overflow
    /// roll-over without recording tens of thousands of decisions.
    #[cfg(test)]
    pub(super) fn for_tests_with_decision_cap(max_decision_keys: usize) -> Self {
        Self::for_tests_inner(max_decision_keys)
    }

    #[cfg(test)]
    fn for_tests_inner(max_decision_keys: usize) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                decisions: Mutex::new(FastHashMap::default()),
                emitter: None,
                max_decision_keys,
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
        // A full window rolls unseen combinations into one overflow bucket per sampler, so burst
        // cardinality cannot grow the map without bound; keys already present keep counting.
        let counts = if decisions.len() >= self.inner.max_decision_keys && !decisions.contains_key(&key) {
            decisions.entry(self.overflow_key(sampler)).or_default()
        } else {
            decisions.entry(key).or_default()
        };
        counts.seen += 1;
        counts.kept += u64::from(kept);
    }

    /// Returns the key counting decisions rolled past the window's cap.
    ///
    /// One bucket per sampler: the service collapses to [`OVERFLOW_SERVICE`] and the env and
    /// priority dimensions are dropped, so the roll-up itself stays bounded.
    fn overflow_key(&self, sampler: SamplerName) -> DecisionKey {
        DecisionKey {
            sampler,
            service: MetaString::from_static(OVERFLOW_SERVICE),
            env: MetaString::empty(),
            priority: None,
        }
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
    /// Emission goes through cached metric handles and is skipped in test instances without an
    /// emitter; tests inspect a window through [`Telemetry::snapshot_decisions`] before flushing.
    pub(super) fn flush(&self) {
        let Some(emitter) = self.inner.emitter.as_ref() else {
            // Test instances still observe window semantics: deltas reset, the gauge does not.
            self.inner.decisions.lock().unwrap().clear();
            self.inner.rare_hits.swap(0, Ordering::Relaxed);
            self.inner.rare_misses.swap(0, Ordering::Relaxed);
            return;
        };

        // Report the window through cached handles, draining the window map in place so its
        // capacity carries into the next window and no intermediate collection is built.
        // Registering fresh handles each flush would drop them right after, and the registry
        // would reclaim the series before the next window: readers slower than one window
        // would then see series missing, or see their counts reset.
        {
            let mut decisions = self.inner.decisions.lock().unwrap();
            let mut handles = emitter.handles.lock().unwrap();
            for (key, counts) in decisions.drain() {
                let tracked = handles
                    .entry(key.clone())
                    .or_insert_with(|| TrackedHandles::new(&emitter.builder, &key));
                tracked.idle_windows = 0;
                tracked.seen.increment(counts.seen);
                if counts.kept > 0 {
                    let kept = tracked.kept.get_or_insert_with(|| {
                        emitter
                            .builder
                            .register_counter_with_tags(METRIC_SAMPLER_KEPT, key.tags())
                    });
                    kept.increment(counts.kept);
                }
            }
            // Series that stayed quiet keep their handles for a few more windows, then release
            // them so the cache tracks recent traffic instead of every key ever seen.
            handles.retain(|_, tracked| {
                tracked.idle_windows = tracked.idle_windows.saturating_add(1);
                tracked.idle_windows <= HANDLE_GRACE_WINDOWS
            });
        }

        let fixed = &emitter.fixed;
        fixed
            .priority_size
            .set(self.inner.priority_tracked.load(Ordering::Relaxed) as f64);
        fixed
            .no_priority_size
            .set(self.inner.no_priority_tracked.load(Ordering::Relaxed) as f64);
        fixed
            .error_size
            .set(self.inner.error_tracked.load(Ordering::Relaxed) as f64);

        let hits = self.inner.rare_hits.swap(0, Ordering::Relaxed);
        fixed.rare_hits.increment(hits);
        let misses = self.inner.rare_misses.swap(0, Ordering::Relaxed);
        fixed.rare_misses.increment(misses);
        fixed
            .rare_shrinks
            .set(self.inner.rare_shrinks.load(Ordering::Relaxed) as f64);
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
    fn tracked_handle_count(&self) -> usize {
        self.inner
            .emitter
            .as_ref()
            .map(|emitter| emitter.handles.lock().unwrap().len())
            .unwrap_or(0)
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
    use metrics::{set_default_local_recorder, Key, Label};
    use saluki_metrics::test::TestRecorder;

    use super::*;

    fn record(telemetry: &Telemetry, kept: bool, sampler: SamplerName, service: &str, env: &str) {
        telemetry.record_decision(kept, sampler, 1, MetaString::from(service), MetaString::from(env));
    }

    fn probabilistic_key(service: &str) -> DecisionKey {
        DecisionKey {
            sampler: SamplerName::Probabilistic,
            service: MetaString::from(service),
            env: MetaString::from("prod"),
            priority: None,
        }
    }

    fn decision_metric_key(metric_name: &'static str, key: &DecisionKey) -> Key {
        Key::from_parts(
            metric_name,
            key.tags()
                .into_iter()
                .map(|(name, value)| Label::new(name, value))
                .collect::<Vec<_>>(),
        )
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

        let window = telemetry.snapshot_decisions();
        assert_eq!(window.len(), 2);
        telemetry.flush();
        assert!(telemetry.snapshot_decisions().is_empty());

        // A second, quiet window drains nothing.
        telemetry.flush();
        assert!(telemetry.snapshot_decisions().is_empty());
    }

    #[test]
    fn decision_keys_roll_into_one_overflow_bucket_per_sampler() {
        let telemetry = Telemetry::for_tests_with_decision_cap(3);
        for i in 0..3 {
            record(&telemetry, true, SamplerName::Probabilistic, &format!("svc{i}"), "prod");
        }

        // The window is full: unseen services roll into the overflow bucket.
        record(&telemetry, true, SamplerName::Probabilistic, "svc3", "prod");
        record(&telemetry, false, SamplerName::Probabilistic, "svc4", "prod");
        // A different sampler gets its own overflow bucket, and existing keys keep counting.
        record(&telemetry, true, SamplerName::Rare, "svc5", "prod");
        record(&telemetry, false, SamplerName::Probabilistic, "svc0", "prod");

        let snapshot = telemetry.snapshot_decisions();
        // Three real keys plus one overflow bucket per overflowing sampler.
        assert_eq!(snapshot.len(), 5);

        let (overflow_key, overflow_counts) = snapshot
            .iter()
            .find(|(key, _)| key.sampler == SamplerName::Probabilistic && key.service == "other")
            .map(|(key, counts)| (key.clone(), *counts))
            .unwrap();
        assert_eq!(overflow_counts, DecisionCounts { seen: 2, kept: 1 });
        assert_eq!(
            overflow_key.tags(),
            vec![
                ("sampler", "probabilistic".to_string()),
                ("target_service", "other".to_string()),
            ]
        );

        let svc0 = snapshot
            .iter()
            .find(|(key, _)| key.service == "svc0")
            .map(|(_, counts)| *counts)
            .unwrap();
        assert_eq!(svc0, DecisionCounts { seen: 2, kept: 1 });
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
    fn flush_reports_decisions_through_cached_handles() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        record(&telemetry, false, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();

        let key = probabilistic_key("checkout");
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_SEEN, &key)),
            Some(2)
        );
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_KEPT, &key)),
            Some(1)
        );
        assert_eq!(telemetry.tracked_handle_count(), 1);
    }

    #[test]
    fn decision_handles_are_reused_across_windows() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();
        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();

        // One cached handle pair serves both windows, so the series stays registered between
        // windows and the counts accumulate instead of resetting.
        assert_eq!(telemetry.tracked_handle_count(), 1);

        let key = probabilistic_key("checkout");
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_SEEN, &key)),
            Some(2)
        );
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_KEPT, &key)),
            Some(2)
        );
    }

    #[test]
    fn quiet_decisions_release_their_handles() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();
        assert_eq!(telemetry.tracked_handle_count(), 1);

        // Quiet windows within the grace keep the handle, so the series survives for readers
        // slower than one window.
        for _ in 0..HANDLE_GRACE_WINDOWS - 1 {
            telemetry.flush();
        }
        assert_eq!(telemetry.tracked_handle_count(), 1);

        // The next quiet window is past the grace: the handle releases and the registry can
        // reclaim the series.
        telemetry.flush();
        assert_eq!(telemetry.tracked_handle_count(), 0);
    }

    #[test]
    fn kept_series_appear_only_after_a_kept_decision() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        record(&telemetry, false, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();

        let key = probabilistic_key("checkout");
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_SEEN, &key)),
            Some(1)
        );
        assert_eq!(recorder.counter(decision_metric_key(METRIC_SAMPLER_KEPT, &key)), None);

        record(&telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        telemetry.flush();
        assert_eq!(
            recorder.counter(decision_metric_key(METRIC_SAMPLER_KEPT, &key)),
            Some(1)
        );
    }

    #[test]
    fn fixed_metrics_report_every_window() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        telemetry.record_rare_hit();
        telemetry.record_rare_miss();
        telemetry.record_rare_shrink();
        telemetry.set_tracked_signature_counts(11, 7, 3);
        telemetry.flush();

        assert_eq!(recorder.counter(METRIC_RARE_HITS), Some(1));
        assert_eq!(recorder.counter(METRIC_RARE_MISSES), Some(1));
        assert_eq!(recorder.gauge(METRIC_RARE_SHRINKS), Some(1.0));
        let size_key =
            |sampler: &str| Key::from_parts(METRIC_SAMPLER_SIZE, vec![Label::new("sampler", sampler.to_string())]);
        assert_eq!(recorder.gauge(size_key(SamplerName::Priority.as_str())), Some(11.0));
        assert_eq!(recorder.gauge(size_key(SamplerName::NoPriority.as_str())), Some(7.0));
        assert_eq!(recorder.gauge(size_key(SamplerName::Error.as_str())), Some(3.0));

        // A quiet window leaves the delta counters at their window totals and republishes the
        // gauges unchanged.
        telemetry.flush();
        assert_eq!(recorder.counter(METRIC_RARE_HITS), Some(1));
        assert_eq!(recorder.gauge(METRIC_RARE_SHRINKS), Some(1.0));

        telemetry.record_rare_shrink();
        telemetry.flush();
        assert_eq!(recorder.gauge(METRIC_RARE_SHRINKS), Some(2.0));
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

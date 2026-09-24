//! Sampler telemetry, emitted as metric events.
//!
//! The telemetry is split by who can write it: [`SamplerCounters`] holds the fixed counters the
//! individual samplers record through shared references, with the recording sampler's identity
//! baked into each counter name, and [`DecisionWindow`] holds the per-decision counts, keyed at
//! runtime by the sampler that decided. The window is owned and flushed exclusively by the
//! transform's loop, so it needs no lock; only the counters shared into the inner samplers stay
//! atomic.
//!
//! Decisions accumulate in a map for one window; [`DecisionWindow::take_window_events`]
//! reports every entry as seen/kept delta counters, the signature-count gauges, and the
//! rare-sampler counters, then resets the window. The events flow through the sampler transform's
//! metrics output into the metrics pipeline, so the metrics reach the backend under their
//! published names without internal-registry involvement. The window map is capped at
//! [`MAX_DECISION_KEYS`] entries; combinations beyond the cap count under one overflow bucket per
//! sampler. Hits and misses are window deltas; shrinks accumulate for the process lifetime and are
//! published as a gauge.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use saluki_common::collections::FastHashMap;
use saluki_context::{tags::TagSet, Context};
use saluki_core::data_model::event::{metric::Metric, Event};
use stringtheory::MetaString;

/// How long a telemetry window accumulates decisions before reporting.
pub(super) const WINDOW: Duration = Duration::from_secs(10);

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

    /// Assembles the metric tag set for this key, in `name:value` form.
    fn tag_set(&self) -> TagSet {
        let tags = self.tags();
        let mut tag_set = TagSet::with_capacity(tags.len());
        for (name, value) in tags {
            tag_set.insert_tag(MetaString::from(format!("{}:{}", name, value)));
        }
        tag_set
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

/// Counters shared with the individual samplers.
///
/// The inner samplers record through shared references, so the counters are atomic; cloning shares
/// them. Attribution lives in the counter names themselves: each counter is specific to the
/// sampler that records it, so the counters carry no runtime sampler identity.
#[derive(Clone)]
pub(super) struct SamplerCounters {
    inner: Arc<CountersInner>,
}

struct CountersInner {
    rare_hits: AtomicU64,
    rare_misses: AtomicU64,
    rare_shrinks: AtomicU64,
}

impl SamplerCounters {
    /// Creates a telemetry with all counters at zero.
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(CountersInner {
                rare_hits: AtomicU64::new(0),
                rare_misses: AtomicU64::new(0),
                rare_shrinks: AtomicU64::new(0),
            }),
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

    #[cfg(test)]
    fn rare_totals(&self) -> (u64, u64, u64) {
        (
            self.inner.rare_hits.load(Ordering::Relaxed),
            self.inner.rare_misses.load(Ordering::Relaxed),
            self.inner.rare_shrinks.load(Ordering::Relaxed),
        )
    }
}

/// The decision window, owned by the transform's loop.
///
/// Decisions accumulate in a map for one window and are reported as metric events by
/// [`DecisionWindow::take_window_events`], which also resets the window. Each decision is keyed at
/// runtime by the sampler that made it, unlike [`SamplerCounters`] where the recording sampler is
/// fixed per counter. The loop holds this state exclusively, so the decision map is a plain map;
/// the counters shared into the inner samplers stay atomic behind [`SamplerCounters`].
pub(super) struct DecisionWindow {
    decisions: FastHashMap<DecisionKey, DecisionCounts>,
    max_decision_keys: usize,
    counters: SamplerCounters,
    priority_tracked: i64,
    no_priority_tracked: i64,
    error_tracked: i64,
}

impl DecisionWindow {
    /// Creates a telemetry with the default decision-key cap.
    pub(super) fn new() -> Self {
        Self::with_decision_key_cap(MAX_DECISION_KEYS)
    }

    /// Creates a telemetry with a reduced decision-key cap, for exercising the overflow roll-over
    /// without recording tens of thousands of decisions.
    fn with_decision_key_cap(max_decision_keys: usize) -> Self {
        Self {
            decisions: FastHashMap::default(),
            max_decision_keys,
            counters: SamplerCounters::new(),
            priority_tracked: 0,
            no_priority_tracked: 0,
            error_tracked: 0,
        }
    }

    /// Returns the counters shared with the individual samplers.
    pub(super) fn counters(&self) -> &SamplerCounters {
        &self.counters
    }

    /// Records one sampler decision.
    ///
    /// The priority is carried only by the priority sampler; every other sampler counts under a
    /// key without it.
    pub(super) fn record_decision(
        &mut self, kept: bool, sampler: SamplerName, priority: i32, service: MetaString, env: MetaString,
    ) {
        let key = DecisionKey {
            sampler,
            service,
            env,
            priority: (sampler == SamplerName::Priority).then_some(priority),
        };
        // A full window rolls unseen combinations into one overflow bucket per sampler, so burst
        // cardinality cannot grow the map without bound; keys already present keep counting.
        let counts = if self.decisions.len() >= self.max_decision_keys && !self.decisions.contains_key(&key) {
            self.decisions.entry(self.overflow_key(sampler)).or_default()
        } else {
            self.decisions.entry(key).or_default()
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

    /// Publishes the tracked-signature counts of the adaptive samplers.
    pub(super) fn set_tracked_signature_counts(&mut self, priority: i64, no_priority: i64, error: i64) {
        self.priority_tracked = priority;
        self.no_priority_tracked = no_priority;
        self.error_tracked = error;
    }

    /// Reports the window as metric events and starts a new one.
    ///
    /// Every decision reports its seen count, and keys that kept nothing never report a kept
    /// series. The signature-count gauges and the rare-sampler counters are reported every window,
    /// including quiet ones, so readers see gauges republished unchanged between active windows.
    pub(super) fn take_window_events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();

        // Drain the window map in place so its capacity carries into the next window.
        for (key, counts) in self.decisions.drain() {
            let tags = key.tag_set();
            events.push(Event::Metric(Metric::counter(
                Context::from_parts(METRIC_SAMPLER_SEEN, tags.clone()),
                counts.seen as f64,
            )));
            if counts.kept > 0 {
                events.push(Event::Metric(Metric::counter(
                    Context::from_parts(METRIC_SAMPLER_KEPT, tags),
                    counts.kept as f64,
                )));
            }
        }

        events.push(gauge_event(
            METRIC_SAMPLER_SIZE,
            &[("sampler", SamplerName::Priority.as_str())],
            self.priority_tracked as f64,
        ));
        events.push(gauge_event(
            METRIC_SAMPLER_SIZE,
            &[("sampler", SamplerName::NoPriority.as_str())],
            self.no_priority_tracked as f64,
        ));
        events.push(gauge_event(
            METRIC_SAMPLER_SIZE,
            &[("sampler", SamplerName::Error.as_str())],
            self.error_tracked as f64,
        ));

        let hits = self.counters.inner.rare_hits.swap(0, Ordering::Relaxed);
        if hits > 0 {
            events.push(Event::Metric(Metric::counter(
                Context::from_parts(METRIC_RARE_HITS, TagSet::default()),
                hits as f64,
            )));
        }
        let misses = self.counters.inner.rare_misses.swap(0, Ordering::Relaxed);
        if misses > 0 {
            events.push(Event::Metric(Metric::counter(
                Context::from_parts(METRIC_RARE_MISSES, TagSet::default()),
                misses as f64,
            )));
        }
        events.push(gauge_event(
            METRIC_RARE_SHRINKS,
            &[],
            self.counters.inner.rare_shrinks.load(Ordering::Relaxed) as f64,
        ));

        events
    }

    #[cfg(test)]
    pub(super) fn snapshot_decisions(&self) -> FastHashMap<DecisionKey, DecisionCounts> {
        self.decisions.clone()
    }
}

fn gauge_event(name: &str, tags: &[(&str, &str)], value: f64) -> Event {
    let mut tag_set = TagSet::with_capacity(tags.len());
    for (name, value) in tags {
        tag_set.insert_tag(MetaString::from(format!("{}:{}", name, value)));
    }
    Event::Metric(Metric::gauge(Context::from_parts(name, tag_set), value))
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;

    fn record(telemetry: &mut DecisionWindow, kept: bool, sampler: SamplerName, service: &str, env: &str) {
        telemetry.record_decision(kept, sampler, 1, MetaString::from(service), MetaString::from(env));
    }

    fn metric_with_tags(events: &[Event], name: &str, tags: &[&str]) -> bool {
        events.iter().any(|event| {
            let Some(metric) = event.try_as_metric() else {
                return false;
            };
            metric.context().name() == name && tags.iter().all(|tag| metric.context().tags().has_tag(tag))
        })
    }

    fn has_counter(events: &[Event], name: &str, tags: &[&str], value: f64) -> bool {
        events.iter().any(|event| {
            let Some(metric) = event.try_as_metric() else {
                return false;
            };
            metric.context().name() == name
                && tags.iter().all(|tag| metric.context().tags().has_tag(tag))
                && *metric.values() == MetricValues::counter(value)
        })
    }

    fn has_gauge(events: &[Event], name: &str, tags: &[&str], value: f64) -> bool {
        events.iter().any(|event| {
            let Some(metric) = event.try_as_metric() else {
                return false;
            };
            metric.context().name() == name
                && tags.iter().all(|tag| metric.context().tags().has_tag(tag))
                && *metric.values() == MetricValues::gauge(value)
        })
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
        let mut telemetry = DecisionWindow::new();
        record(&mut telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        record(&mut telemetry, false, SamplerName::Probabilistic, "checkout", "prod");
        record(&mut telemetry, true, SamplerName::Probabilistic, "payments", "prod");

        let snapshot = telemetry.snapshot_decisions();
        let checkout = snapshot
            .iter()
            .find(|(key, _)| key.service == "checkout")
            .map(|(_, counts)| *counts)
            .unwrap();
        assert_eq!(checkout, DecisionCounts { seen: 2, kept: 1 });

        let events = telemetry.take_window_events();
        assert!(has_counter(
            &events,
            METRIC_SAMPLER_SEEN,
            &["sampler:probabilistic", "target_service:checkout"],
            2.0
        ));
        assert!(has_counter(
            &events,
            METRIC_SAMPLER_KEPT,
            &["sampler:probabilistic", "target_service:checkout"],
            1.0
        ));
        assert!(telemetry.snapshot_decisions().is_empty());

        // A second, quiet window republishes the gauges but reports no decision counters.
        let events = telemetry.take_window_events();
        assert!(!metric_with_tags(
            &events,
            METRIC_SAMPLER_SEEN,
            &["sampler:probabilistic"]
        ));
    }

    #[test]
    fn decision_keys_roll_into_one_overflow_bucket_counters() {
        let mut telemetry = DecisionWindow::with_decision_key_cap(3);
        for i in 0..3 {
            record(
                &mut telemetry,
                true,
                SamplerName::Probabilistic,
                &format!("svc{i}"),
                "prod",
            );
        }

        // The window is full: unseen services roll into the overflow bucket.
        record(&mut telemetry, true, SamplerName::Probabilistic, "svc3", "prod");
        record(&mut telemetry, false, SamplerName::Probabilistic, "svc4", "prod");
        // A different sampler gets its own overflow bucket, and existing keys keep counting.
        record(&mut telemetry, true, SamplerName::Rare, "svc5", "prod");
        record(&mut telemetry, false, SamplerName::Probabilistic, "svc0", "prod");

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
        let mut telemetry = DecisionWindow::new();
        telemetry.counters().record_rare_hit();
        telemetry.counters().record_rare_hit();
        telemetry.counters().record_rare_miss();
        telemetry.counters().record_rare_shrink();
        assert_eq!(telemetry.counters().rare_totals(), (2, 1, 1));

        let events = telemetry.take_window_events();
        assert!(has_counter(&events, METRIC_RARE_HITS, &[], 2.0));
        assert!(has_counter(&events, METRIC_RARE_MISSES, &[], 1.0));
        assert!(has_gauge(&events, METRIC_RARE_SHRINKS, &[], 1.0));
        assert_eq!(telemetry.counters().rare_totals(), (0, 0, 1));

        telemetry.counters().record_rare_shrink();
        let events = telemetry.take_window_events();
        // A quiet window for hits and misses still republishes the cumulative shrink gauge.
        assert!(!metric_with_tags(&events, METRIC_RARE_HITS, &[]));
        assert!(has_gauge(&events, METRIC_RARE_SHRINKS, &[], 2.0));
    }

    #[test]
    fn kept_series_appear_only_after_a_kept_decision() {
        let mut telemetry = DecisionWindow::new();
        record(&mut telemetry, false, SamplerName::Probabilistic, "checkout", "prod");

        let events = telemetry.take_window_events();
        assert!(has_counter(
            &events,
            METRIC_SAMPLER_SEEN,
            &["target_service:checkout"],
            1.0
        ));
        assert!(!metric_with_tags(
            &events,
            METRIC_SAMPLER_KEPT,
            &["target_service:checkout"]
        ));

        record(&mut telemetry, true, SamplerName::Probabilistic, "checkout", "prod");
        let events = telemetry.take_window_events();
        assert!(has_counter(
            &events,
            METRIC_SAMPLER_KEPT,
            &["target_service:checkout"],
            1.0
        ));
    }

    #[test]
    fn fixed_metrics_report_every_window() {
        let mut telemetry = DecisionWindow::new();
        telemetry.counters().record_rare_hit();
        telemetry.counters().record_rare_miss();
        telemetry.counters().record_rare_shrink();
        telemetry.set_tracked_signature_counts(11, 7, 3);

        let events = telemetry.take_window_events();
        assert!(has_gauge(&events, METRIC_SAMPLER_SIZE, &["sampler:priority"], 11.0));
        assert!(has_gauge(&events, METRIC_SAMPLER_SIZE, &["sampler:no_priority"], 7.0));
        assert!(has_gauge(&events, METRIC_SAMPLER_SIZE, &["sampler:error"], 3.0));

        // A quiet window leaves the delta counters unreported and republishes the gauges
        // unchanged.
        let events = telemetry.take_window_events();
        assert!(!metric_with_tags(&events, METRIC_RARE_HITS, &[]));
        assert!(has_gauge(&events, METRIC_RARE_SHRINKS, &[], 1.0));
    }

    #[test]
    fn only_the_priority_sampler_carries_the_priority_dimension() {
        let mut telemetry = DecisionWindow::new();
        record(&mut telemetry, true, SamplerName::NoPriority, "checkout", "prod");
        let snapshot = telemetry.snapshot_decisions();
        let (key, _) = snapshot.iter().next().unwrap();
        assert_eq!(key.priority, None);
    }

    #[test]
    fn sampler_counters_share_counts_between_clones() {
        // The transform hands clones into the individual samplers, so every clone must observe the
        // same counters.
        let telemetry = DecisionWindow::new();
        let counters = telemetry.counters().clone();
        counters.record_rare_hit();
        assert_eq!(telemetry.counters().rare_totals(), (1, 0, 0));
    }
}

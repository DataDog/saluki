//! Sampler metrics for the trace sampler.
//!
//! Mirrors the `Metrics` / `MetricsKey` types from
//! [`datadog-agent/pkg/trace/sampler/metrics.go`](https://github.com/DataDog/datadog-agent/blob/be33ac1490c4a34602cbc65a211406b73ad6d00b/pkg/trace/sampler/metrics.go).
//!
//! Per-decision counters (updated inline on every trace):
//! - `datadog.trace_agent.sampler.seen` — every trace that passed through the sampler
//! - `datadog.trace_agent.sampler.kept` — traces that were kept
//!
//! Both are tagged with `sampler`, `target_service`, and (when relevant) `target_env` and
//! `sampling_priority`.
//!
//! Per-sampler gauges (updated after each batch in `transform_buffer`):
//! - `datadog.trace_agent.sampler.size` — current signature-table size for priority/no_priority/error samplers

use metrics::{Counter, Gauge};
use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;

const METRIC_SEEN: &str = "datadog.trace_agent.sampler.seen";
const METRIC_KEPT: &str = "datadog.trace_agent.sampler.kept";
const METRIC_SIZE: &str = "datadog.trace_agent.sampler.size";

/// The sampler that made a sampling decision.
///
/// Mirrors `sampler.Name` in the Go agent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum SamplerName {
    Unknown,
    Priority,
    NoPriority,
    Error,
    Rare,
    Probabilistic,
}

impl SamplerName {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Priority => "priority",
            Self::NoPriority => "no_priority",
            Self::Error => "error",
            Self::Rare => "rare",
            Self::Probabilistic => "probabilistic",
        }
    }

    /// Whether the `target_env` tag should be included for this sampler.
    ///
    /// Mirrors `Name.shouldAddEnvTag()` in the Go agent: true for priority, no-priority, rare, and error.
    fn should_add_env_tag(self) -> bool {
        matches!(self, Self::Priority | Self::NoPriority | Self::Rare | Self::Error)
    }
}

/// Returns the `sampling_priority` tag value for a given priority integer.
///
/// Mirrors `SamplingPriority.tagValue()` in the Go agent.
fn priority_tag_value(priority: i32) -> &'static str {
    match priority {
        -1 => "manual_drop",
        0 => "auto_drop",
        1 => "auto_keep",
        2 => "manual_keep",
        _ => "none",
    }
}

/// Cache key for a pair of seen/kept counters.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CounterKey {
    sampler: SamplerName,
    service: String,
    env: String,
    /// Only set when `sampler == Priority`.
    sampling_priority: Option<i32>,
}

/// Tracks all sampler metrics:
/// - `datadog.trace_agent.sampler.seen` / `kept` per `(sampler, service, env, priority)` combination
/// - `datadog.trace_agent.sampler.size` gauges for priority, no_priority, and error samplers
///
/// Counter handles are lazily registered and cached so that the hot path — `record()` — pays only
/// a hash-map lookup after the first observation of a given key combination.
pub(super) struct SamplerMetrics {
    counters: FastHashMap<CounterKey, (Counter, Counter)>,
    priority_size: Gauge,
    no_priority_size: Gauge,
    error_size: Gauge,
    builder: MetricsBuilder,
}

impl SamplerMetrics {
    pub(super) fn new(builder: MetricsBuilder) -> Self {
        let priority_size = builder.register_gauge_with_tags(METRIC_SIZE, ["sampler:priority"]);
        let no_priority_size = builder.register_gauge_with_tags(METRIC_SIZE, ["sampler:no_priority"]);
        let error_size = builder.register_gauge_with_tags(METRIC_SIZE, ["sampler:error"]);
        Self {
            counters: FastHashMap::default(),
            priority_size,
            no_priority_size,
            error_size,
            builder,
        }
    }

    /// Updates the `sampler.size` gauges for the three score-based samplers.
    ///
    /// Should be called after each `transform_buffer` batch.
    pub(super) fn report_sizes(&self, priority: i64, no_priority: i64, error: i64) {
        self.priority_size.set(priority as f64);
        self.no_priority_size.set(no_priority as f64);
        self.error_size.set(error as f64);
    }

    /// Records a sampling decision.
    ///
    /// - `sampler`           — which sampler made the decision
    /// - `service`           — root span service name
    /// - `env`               — tracer environment (empty string if absent)
    /// - `sampling_priority` — the trace's sampling priority; only included in tags for `Priority` sampler
    pub(super) fn record(
        &mut self, keep: bool, sampler: SamplerName, service: &str, env: &str, sampling_priority: Option<i32>,
    ) {
        let key = CounterKey {
            sampler,
            service: service.to_owned(),
            env: env.to_owned(),
            sampling_priority: if sampler == SamplerName::Priority {
                sampling_priority
            } else {
                None
            },
        };

        let (seen, kept) = self.counters.entry(key.clone()).or_insert_with(|| {
            let seen = register_counter(&self.builder, METRIC_SEEN, &key);
            let kept = register_counter(&self.builder, METRIC_KEPT, &key);
            (seen, kept)
        });

        seen.increment(1);
        if keep {
            kept.increment(1);
        }
    }
}

fn register_counter(builder: &MetricsBuilder, name: &'static str, key: &CounterKey) -> Counter {
    let mut tags: Vec<String> = Vec::with_capacity(4);
    tags.push(format!("sampler:{}", key.sampler.as_str()));
    if key.sampler == SamplerName::Priority {
        if let Some(p) = key.sampling_priority {
            tags.push(format!("sampling_priority:{}", priority_tag_value(p)));
        }
    }
    if !key.service.is_empty() {
        tags.push(format!("target_service:{}", key.service));
    }
    if !key.env.is_empty() && key.sampler.should_add_env_tag() {
        tags.push(format!("target_env:{}", key.env));
    }
    builder.register_counter_with_tags(name, tags)
}

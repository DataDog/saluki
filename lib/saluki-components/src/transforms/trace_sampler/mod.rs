//! Trace sampling transform.
//!
//! This transform implements agent-side head sampling for traces, supporting:
//! - Probabilistic sampling based on trace ID
//! - User-set priority preservation
//! - Error-based sampling as a safety net
//! - OTLP trace ingestion with proper sampling decision handling
//!
//! # Missing
//!
//! add trace metrics: datadog-agent/pkg/trace/sampler/metrics.go
//! adding missing samplers (priority, nopriority, rare)
//! add error tracking standalone mode

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::collections::FastHashMap;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::{Span, Trace, TraceSampling},
        Event, EventType,
    },
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

mod core_sampler;
mod errors;
mod probabilistic;
mod score_sampler;
mod signature;

use self::probabilistic::PROB_RATE_KEY;
use crate::common::datadog::{
    apm::ApmConfig, sample_by_rate, DECISION_MAKER_PROBABILISTIC, OTEL_TRACE_ID_META_KEY, SAMPLING_PRIORITY_METRIC_KEY,
    TAG_DECISION_MAKER,
};
use crate::common::otlp::config::TracesConfig;

// Sampling priority constants (matching datadog-agent)
const PRIORITY_AUTO_DROP: i32 = 0;
const PRIORITY_AUTO_KEEP: i32 = 1;
const PRIORITY_USER_KEEP: i32 = 2;

const ERROR_SAMPLE_RATE: f64 = 1.0; // Default extra sample rate (matches agent's ExtraSampleRate)

// Single Span Sampling and Analytics Events keys
const KEY_SPAN_SAMPLING_MECHANISM: &str = "_dd.span_sampling.mechanism";
const KEY_ANALYZED_SPANS: &str = "_dd.analyzed";

// Decision maker values for `_dd.p.dm` (matching datadog-agent).
const DECISION_MAKER_MANUAL_PRIORITY: &str = "-4";

fn normalize_sampling_rate(rate: f64) -> f64 {
    if rate <= 0.0 || rate >= 1.0 {
        1.0
    } else {
        rate
    }
}

/// Configuration for the trace sampler transform.
#[derive(Debug)]
pub struct TraceSamplerConfiguration {
    apm_config: ApmConfig,
    otlp_sampling_rate: f64,
}

impl TraceSamplerConfiguration {
    /// Creates a new `TraceSamplerConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        let otlp_traces: TracesConfig = config.try_get_typed("otlp_config.traces")?.unwrap_or_default();
        let otlp_sampling_rate = normalize_sampling_rate(otlp_traces.probabilistic_sampler.sampling_percentage / 100.0);
        Ok(Self {
            apm_config,
            otlp_sampling_rate,
        })
    }
}

#[async_trait]
impl TransformBuilder for TraceSamplerConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Trace
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Trace)];
        OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let sampler = TraceSampler {
            sampling_rate: self.apm_config.probabilistic_sampler_sampling_percentage() / 100.0,
            error_sampling_enabled: self.apm_config.error_sampling_enabled(),
            error_tracking_standalone: self.apm_config.error_tracking_standalone_enabled(),
            probabilistic_sampler_enabled: self.apm_config.probabilistic_sampler_enabled(),
            otlp_sampling_rate: self.otlp_sampling_rate,
            error_sampler: errors::ErrorsSampler::new(self.apm_config.errors_per_second(), ERROR_SAMPLE_RATE),
            no_priority_sampler: score_sampler::NoPrioritySampler::new(
                self.apm_config.target_traces_per_second(),
                ERROR_SAMPLE_RATE,
            ),
        };

        Ok(Box::new(sampler))
    }
}

impl MemoryBounds for TraceSamplerConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<TraceSampler>("component struct");
    }
}

pub struct TraceSampler {
    sampling_rate: f64,
    error_tracking_standalone: bool,
    error_sampling_enabled: bool,
    probabilistic_sampler_enabled: bool,
    otlp_sampling_rate: f64,
    error_sampler: errors::ErrorsSampler,
    no_priority_sampler: score_sampler::NoPrioritySampler,
}

impl TraceSampler {
    // TODO: merge this with the other duplicate "find root span of trace" functions
    /// Find the root span index of a trace.
    fn get_root_span_index(&self, trace: &Trace) -> Option<usize> {
        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/traceutil/trace.go#L36
        let spans = trace.spans();
        if spans.is_empty() {
            return None;
        }
        let length = spans.len();
        // General case: go over all spans and check for one without a matching parent.
        // This intentionally mirrors `datadog-agent/pkg/trace/traceutil/trace.go:GetRoot`:
        // - Fast-path: return the last span with `parent_id == 0` (some clients report the root last)
        // - Otherwise: build a map of `parent_id -> child_span_index`, delete entries whose parent
        //   exists in the trace, and pick any remaining "orphan" child span.
        let mut parent_id_to_child: FastHashMap<u64, usize> = FastHashMap::default();

        for i in 0..length {
            // Common case optimization: check for span with parent_id == 0, starting from the end,
            // since some clients report the root last.
            let j = length - 1 - i;
            if spans[j].parent_id() == 0 {
                return Some(j);
            }
            parent_id_to_child.insert(spans[j].parent_id(), j);
        }

        for span in spans.iter() {
            parent_id_to_child.remove(&span.span_id());
        }

        // Here, if the trace is valid, we should have `len(parent_id_to_child) == 1`.
        if parent_id_to_child.len() != 1 {
            debug!(
                "Didn't reliably find the root span for traceID:{}",
                &spans[0].trace_id()
            );
        }

        // Have a safe behavior if that's not the case.
        // Pick a random span without its parent.
        if let Some((_, child_idx)) = parent_id_to_child.iter().next() {
            return Some(*child_idx);
        }

        // Gracefully fail with the last span of the trace.
        Some(length - 1)
    }

    /// Check for user-set sampling priority in trace
    fn get_user_priority(&self, trace: &Trace, root_span_idx: usize) -> Option<i32> {
        // First check trace-level sampling priority (last-seen priority from OTLP ingest)
        if let Some(sampling) = trace.sampling() {
            if let Some(priority) = sampling.priority {
                return Some(priority);
            }
        }

        if trace.spans().is_empty() {
            return None;
        }

        // Fall back to checking spans (for compatibility with non-OTLP traces)
        // Prefer the root span (common case), but fall back to scanning all spans to be robust to ordering.
        if let Some(root) = trace.spans().get(root_span_idx) {
            if let Some(&p) = root.metrics().get(SAMPLING_PRIORITY_METRIC_KEY) {
                return Some(p as i32);
            }
        }
        let spans = trace.spans();
        spans
            .iter()
            .find_map(|span| span.metrics().get(SAMPLING_PRIORITY_METRIC_KEY).map(|&p| p as i32))
    }

    /// Returns `true` if the given trace ID should be probabilistically sampled.
    fn sample_probabilistic(&self, trace_id: u64) -> bool {
        probabilistic::ProbabilisticSampler::sample(trace_id, self.sampling_rate)
    }

    fn is_otlp_trace(&self, trace: &Trace, root_span_idx: usize) -> bool {
        trace
            .spans()
            .get(root_span_idx)
            .map(|span| {
                span.meta()
                    .contains_key(&MetaString::from_static(OTEL_TRACE_ID_META_KEY))
            })
            .unwrap_or(false)
    }

    fn sample_otlp(&self, trace_id: u64) -> bool {
        sample_by_rate(trace_id, self.otlp_sampling_rate)
    }

    /// Returns `true` if the trace contains a span with an error.
    fn trace_contains_error(&self, trace: &Trace, consider_exception_span_events: bool) -> bool {
        trace.spans().iter().any(|span| {
            span.error() != 0 || (consider_exception_span_events && self.span_contains_exception_span_event(span))
        })
    }

    /// Returns `true` if the span has exception span events.
    ///
    /// This checks for the `_dd.span_events.has_exception` meta field set to `"true"`.
    fn span_contains_exception_span_event(&self, span: &Span) -> bool {
        if let Some(has_exception) = span.meta().get("_dd.span_events.has_exception") {
            return has_exception == "true";
        }
        false
    }

    /// Returns all spans from the given trace that have Single Span Sampling tags present.
    fn get_single_span_sampled_spans(&self, trace: &Trace) -> Vec<Span> {
        let mut sampled_spans = Vec::new();
        for span in trace.spans().iter() {
            if span.metrics().contains_key(KEY_SPAN_SAMPLING_MECHANISM) {
                sampled_spans.push(span.clone());
            }
        }
        sampled_spans
    }

    /// Returns all spans from the given trace that have Single Span Sampling tags present.
    fn get_analyzed_spans(&self, trace: &Trace) -> Vec<Span> {
        let mut analyzed_spans = Vec::new();
        for span in trace.spans().iter() {
            if span.metrics().contains_key(KEY_ANALYZED_SPANS) {
                // Keep spans that have the analyzed tag
                analyzed_spans.push(span.clone());
            }
        }
        analyzed_spans
    }

    /// Returns `true` if the given trace has any analyzed spans.
    fn has_analyzed_spans(&self, trace: &Trace) -> bool {
        trace
            .spans()
            .iter()
            .any(|span| span.metrics().contains_key(KEY_ANALYZED_SPANS))
    }

    /// Apply Single Span Sampling to the trace
    /// Returns true if the trace was modified
    fn single_span_sampling(&self, trace: &mut Trace) -> bool {
        let ss_spans = self.get_single_span_sampled_spans(trace);
        if !ss_spans.is_empty() {
            // Span sampling has kept some spans -> update the trace
            trace.set_spans(ss_spans);
            // Set high priority and mark as kept
            let sampling = TraceSampling::new(
                false,
                Some(PRIORITY_USER_KEEP),
                None, // No decision maker for SSS
                Some(MetaString::from(format!("{:.2}", self.sampling_rate))),
            );
            trace.set_sampling(Some(sampling));
            true
        } else {
            false
        }
    }

    /// Evaluates the given trace against all configured samplers.
    ///
    /// Return a tuple containing whether or not the trace should be kept, the decision maker tag (which sampler is responsible),
    /// and the index of the root span used for evaluation.
    fn run_samplers(&mut self, trace: &mut Trace) -> (bool, i32, &'static str, Option<usize>) {
        // logic taken from: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/agent/agent.go#L1066
        let now = std::time::SystemTime::now();
        // Empty trace check
        if trace.spans().is_empty() {
            return (false, PRIORITY_AUTO_DROP, "", None);
        }
        let contains_error = self.trace_contains_error(trace, false);
        let Some(root_span_idx) = self.get_root_span_index(trace) else {
            return (false, PRIORITY_AUTO_DROP, "", None);
        };

        // Modern path: ProbabilisticSamplerEnabled = true
        if self.probabilistic_sampler_enabled {
            let mut prob_keep = false;
            let mut decision_maker = "";

            // Run probabilistic sampler - use root span's trace ID
            let root_trace_id = trace.spans()[root_span_idx].trace_id();
            if self.sample_probabilistic(root_trace_id) {
                decision_maker = DECISION_MAKER_PROBABILISTIC; // probabilistic sampling
                prob_keep = true;

                if let Some(root_span) = trace.spans_mut().get_mut(root_span_idx) {
                    let metrics = root_span.metrics_mut();
                    metrics.insert(MetaString::from(PROB_RATE_KEY), self.sampling_rate);
                }
            } else if self.error_sampling_enabled && contains_error {
                prob_keep = self.error_sampler.sample_error(now, trace, root_span_idx);
            }

            let priority = if prob_keep {
                PRIORITY_AUTO_KEEP
            } else {
                PRIORITY_AUTO_DROP
            };

            return (prob_keep, priority, decision_maker, Some(root_span_idx));
        }

        let user_priority = self.get_user_priority(trace, root_span_idx);
        if let Some(priority) = user_priority {
            if priority > 0 {
                // User wants to keep this trace
                return (true, priority, DECISION_MAKER_MANUAL_PRIORITY, Some(root_span_idx));
            }
        } else if self.is_otlp_trace(trace, root_span_idx) {
            let root_trace_id = trace.spans()[root_span_idx].trace_id();
            if self.sample_otlp(root_trace_id) {
                if let Some(root_span) = trace.spans_mut().get_mut(root_span_idx) {
                    root_span.metrics_mut().remove(PROB_RATE_KEY);
                }
                return (
                    true,
                    PRIORITY_AUTO_KEEP,
                    DECISION_MAKER_PROBABILISTIC,
                    Some(root_span_idx),
                );
            }
        } else if self.no_priority_sampler.sample(now, trace, root_span_idx) {
            return (true, PRIORITY_AUTO_KEEP, "", Some(root_span_idx));
        }

        if self.error_sampling_enabled && self.trace_contains_error(trace, false) {
            let keep = self.error_sampler.sample_error(now, trace, root_span_idx);
            if keep {
                return (true, PRIORITY_AUTO_KEEP, "", Some(root_span_idx));
            }
        }

        // Default: drop the trace
        (false, PRIORITY_AUTO_DROP, "", Some(root_span_idx))
    }

    /// Apply sampling metadata to the trace in-place.
    ///
    /// The `root_span_id` parameter identifies which span should receive the sampling metadata.
    /// This avoids recalculating the root span since it was already found in `run_samplers`.
    fn apply_sampling_metadata(
        &self, trace: &mut Trace, keep: bool, priority: i32, decision_maker: &str, root_span_idx: usize,
    ) {
        let is_otlp = self.is_otlp_trace(trace, root_span_idx);
        let root_span_value = match trace.spans_mut().get_mut(root_span_idx) {
            Some(span) => span,
            None => return,
        };

        // Add tag for the decision maker
        let meta = root_span_value.meta_mut();
        if priority > 0 && !decision_maker.is_empty() {
            meta.insert(MetaString::from(TAG_DECISION_MAKER), MetaString::from(decision_maker));
        }

        // Now we can use trace again to set sampling metadata
        let sampling_rate = if is_otlp {
            self.otlp_sampling_rate
        } else {
            self.sampling_rate
        };
        let sampling = TraceSampling::new(
            !keep,
            Some(priority),
            if priority > 0 && !decision_maker.is_empty() {
                Some(MetaString::from(decision_maker))
            } else {
                None
            },
            Some(MetaString::from(format!("{:.2}", sampling_rate))),
        );
        trace.set_sampling(Some(sampling));
    }
}

#[async_trait]
impl Transform for TraceSampler {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("Trace sampler transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        for event in events {
                            match event {
                                Event::Trace(mut trace) => {
                                    // keep is a boolean that indicates if the trace should be kept or dropped
                                    // priority is the sampling priority
                                    // decision_maker is the tag that indicates the decision maker (probabilistic, error, etc.)
                                    // root_span_idx is the index of the root span of the trace
                                    let (keep, priority, decision_maker, root_span_idx) = self.run_samplers(&mut trace);
                                    if keep {
                                        if let Some(root_idx) = root_span_idx {
                                            self.apply_sampling_metadata(
                                                &mut trace,
                                                keep,
                                                priority,
                                                decision_maker,
                                                root_idx,
                                            );
                                        }

                                        // Send the trace to the next component
                                        let mut dispatcher = context
                                            .dispatcher()
                                            .buffered()
                                            .expect("default output should always exist");

                                        dispatcher.push(Event::Trace(trace)).await?;
                                        dispatcher.flush().await?;
                                    } else if !self.error_tracking_standalone {
                                        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/agent/agent.go#L980-L990

                                        // try single span sampling (keeps spans marked for sampling when trace would be dropped)
                                        let modified = self.single_span_sampling(&mut trace);
                                        if !modified {
                                            // Fall back to analytics events if no SSS spans
                                            let analyzed_spans = self.get_analyzed_spans(&trace);
                                            if !analyzed_spans.is_empty() {
                                                // Replace trace spans with analyzed events
                                                trace.set_spans(analyzed_spans);
                                                // Mark trace as kept with high priority
                                                let sampling = TraceSampling::new(
                                                    false,
                                                    Some(PRIORITY_USER_KEEP),
                                                    None,
                                                    Some(MetaString::from(format!("{:.2}", self.sampling_rate))),
                                                );
                                                trace.set_sampling(Some(sampling));

                                                // Send the modified trace downstream
                                                let mut dispatcher = context
                                                    .dispatcher()
                                                    .buffered()
                                                    .expect("default output should always exist");
                                                dispatcher.push(Event::Trace(trace)).await?;
                                                dispatcher.flush().await?;
                                                continue; // Skip to next event
                                            }
                                        } else if self.has_analyzed_spans(&trace) {
                                            // Warn about both SSS and analytics events
                                            debug!("Detected both analytics events AND single span sampling in the same trace. Single span sampling wins because App Analytics is deprecated.");

                                            // Send the SSS-modified trace downstream
                                            let mut dispatcher = context
                                                .dispatcher()
                                                .buffered()
                                                .expect("default output should always exist");
                                            dispatcher.push(Event::Trace(trace)).await?;
                                            dispatcher.flush().await?;
                                            continue; // Skip to next event
                                        }

                                        // If we modified the trace with SSS, send it
                                        if modified {
                                            let mut dispatcher = context
                                                .dispatcher()
                                                .buffered()
                                                .expect("default output should always exist");
                                            dispatcher.push(Event::Trace(trace)).await?;
                                            dispatcher.flush().await?;
                                        } else {
                                            // Neither SSS nor analytics events found, drop the trace
                                            debug!("Dropping trace with priority {}", priority);
                                        }
                                    }
                                }
                                other => {
                                    // Pass through non-trace events
                                    let mut dispatcher = context
                                        .dispatcher()
                                        .buffered()
                                        .expect("default output should always exist");
                                    dispatcher.push(other).await?;
                                    dispatcher.flush().await?;
                                }
                            }
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down trace sampler transform");
                        break;
                    }
                }
            }
        }

        debug!("Trace sampler transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{Span as DdSpan, Trace};

    use super::*;
    const PRIORITY_USER_DROP: i32 = -1;

    fn create_test_sampler() -> TraceSampler {
        TraceSampler {
            sampling_rate: 1.0,
            error_sampling_enabled: true,
            error_tracking_standalone: false,
            probabilistic_sampler_enabled: true,
            otlp_sampling_rate: 1.0,
            error_sampler: errors::ErrorsSampler::new(10.0, 1.0),
            no_priority_sampler: score_sampler::NoPrioritySampler::new(10.0, 1.0),
        }
    }

    fn create_test_span(trace_id: u64, span_id: u64, error: i32) -> DdSpan {
        DdSpan::new(
            MetaString::from("test-service"),
            MetaString::from("test-operation"),
            MetaString::from("test-resource"),
            MetaString::from("test-type"),
            trace_id,
            span_id,
            0,    // parent_id
            0,    // start
            1000, // duration
            error,
        )
    }

    fn create_test_span_with_metrics(trace_id: u64, span_id: u64, metrics: HashMap<String, f64>) -> DdSpan {
        let mut metrics_map = saluki_common::collections::FastHashMap::default();
        for (k, v) in metrics {
            metrics_map.insert(MetaString::from(k), v);
        }
        create_test_span(trace_id, span_id, 0).with_metrics(metrics_map)
    }

    #[allow(dead_code)]
    fn create_test_span_with_meta(trace_id: u64, span_id: u64, meta: HashMap<String, String>) -> DdSpan {
        let mut meta_map = saluki_common::collections::FastHashMap::default();
        for (k, v) in meta {
            meta_map.insert(MetaString::from(k), MetaString::from(v));
        }
        create_test_span(trace_id, span_id, 0).with_meta(meta_map)
    }

    fn create_test_trace(spans: Vec<DdSpan>) -> Trace {
        let tags = TagSet::default();
        Trace::new(spans, tags)
    }

    #[test]
    fn test_user_priority_detection() {
        let sampler = create_test_sampler();

        // Test trace with user-set priority = 2 (UserKeep)
        let mut metrics = HashMap::new();
        metrics.insert(SAMPLING_PRIORITY_METRIC_KEY.to_string(), 2.0);
        let span = create_test_span_with_metrics(12345, 1, metrics);
        let trace = create_test_trace(vec![span]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();

        assert_eq!(sampler.get_user_priority(&trace, root_idx), Some(2));

        // Test trace with user-set priority = -1 (UserDrop)
        let mut metrics = HashMap::new();
        metrics.insert(SAMPLING_PRIORITY_METRIC_KEY.to_string(), -1.0);
        let span = create_test_span_with_metrics(12345, 1, metrics);
        let trace = create_test_trace(vec![span]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();

        assert_eq!(sampler.get_user_priority(&trace, root_idx), Some(-1));

        // Test trace without user priority
        let span = create_test_span(12345, 1, 0);
        let trace = create_test_trace(vec![span]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();

        assert_eq!(sampler.get_user_priority(&trace, root_idx), None);
    }

    #[test]
    fn test_trace_level_priority_takes_precedence() {
        let sampler = create_test_sampler();

        // Test trace-level priority overrides span priorities (last-seen priority)
        // Create spans with different priorities - root has 0, later span has 2
        let mut metrics_root = HashMap::new();
        metrics_root.insert(SAMPLING_PRIORITY_METRIC_KEY.to_string(), 0.0);
        let root_span = create_test_span_with_metrics(12345, 1, metrics_root);

        let mut metrics_later = HashMap::new();
        metrics_later.insert(SAMPLING_PRIORITY_METRIC_KEY.to_string(), 1.0);
        let later_span = create_test_span_with_metrics(12345, 2, metrics_later).with_parent_id(1);

        let mut trace = create_test_trace(vec![root_span, later_span]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();

        // Without trace-level priority, should get priority from root (0)
        assert_eq!(sampler.get_user_priority(&trace, root_idx), Some(0));

        // Now set trace-level priority to 2 (simulating last-seen priority from OTLP translator)
        trace.set_sampling(Some(TraceSampling::new(false, Some(2), None, None)));

        // Trace-level priority should take precedence
        assert_eq!(sampler.get_user_priority(&trace, root_idx), Some(2));

        // Test that trace-level priority is used even when no span has priority
        let span_no_priority = create_test_span(12345, 3, 0);
        let mut trace_only_trace_level = create_test_trace(vec![span_no_priority]);
        trace_only_trace_level.set_sampling(Some(TraceSampling::new(false, Some(1), None, None)));
        let root_idx = sampler.get_root_span_index(&trace_only_trace_level).unwrap();

        assert_eq!(sampler.get_user_priority(&trace_only_trace_level, root_idx), Some(1));
    }

    #[test]
    fn test_manual_keep_with_trace_level_priority() {
        let mut sampler = create_test_sampler();
        sampler.probabilistic_sampler_enabled = false; // Use legacy path that checks user priority

        // Test that manual keep (priority = 2) works via trace-level priority
        let span = create_test_span(12345, 1, 0);
        let mut trace = create_test_trace(vec![span]);
        trace.set_sampling(Some(TraceSampling::new(false, Some(PRIORITY_USER_KEEP), None, None)));

        let (keep, priority, decision_maker, _) = sampler.run_samplers(&mut trace);
        assert!(keep);
        assert_eq!(priority, PRIORITY_USER_KEEP);
        assert_eq!(decision_maker, DECISION_MAKER_MANUAL_PRIORITY);

        // Test manual drop (priority = -1) via trace-level priority
        let span = create_test_span(12345, 1, 0);
        let mut trace = create_test_trace(vec![span]);
        trace.set_sampling(Some(TraceSampling::new(false, Some(PRIORITY_USER_DROP), None, None)));

        let (keep, priority, _, _) = sampler.run_samplers(&mut trace);
        assert!(!keep); // Should not keep when user drops
        assert_eq!(priority, PRIORITY_AUTO_DROP); // Fallthrough to auto-drop

        // Test that priority = 1 (auto keep) via trace-level is also respected
        let span = create_test_span(12345, 1, 0);
        let mut trace = create_test_trace(vec![span]);
        trace.set_sampling(Some(TraceSampling::new(false, Some(PRIORITY_AUTO_KEEP), None, None)));

        let (keep, priority, decision_maker, _) = sampler.run_samplers(&mut trace);
        assert!(keep);
        assert_eq!(priority, PRIORITY_AUTO_KEEP);
        assert_eq!(decision_maker, DECISION_MAKER_MANUAL_PRIORITY);
    }

    #[test]
    fn test_probabilistic_sampling_determinism() {
        let sampler = create_test_sampler();

        // Same trace ID should always produce same decision
        let trace_id = 0x1234567890ABCDEF_u64;
        let result1 = sampler.sample_probabilistic(trace_id);
        let result2 = sampler.sample_probabilistic(trace_id);
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_error_detection() {
        let sampler = create_test_sampler();

        // Test trace with error field set
        let span_with_error = create_test_span(12345, 1, 1);
        let trace = create_test_trace(vec![span_with_error]);
        assert!(sampler.trace_contains_error(&trace, false));

        // Test trace without error
        let span_without_error = create_test_span(12345, 1, 0);
        let trace = create_test_trace(vec![span_without_error]);
        assert!(!sampler.trace_contains_error(&trace, false));
    }

    #[test]
    fn test_sampling_priority_order() {
        // Test modern path: error sampler overrides probabilistic drop
        let mut sampler = create_test_sampler();
        sampler.sampling_rate = 0.5; // 50% sampling rate
        sampler.probabilistic_sampler_enabled = true;

        // Create trace with error that would be dropped by probabilistic
        // Using a trace ID that we know will be dropped at 50% rate
        let span_with_error = create_test_span(u64::MAX - 1, 1, 1);
        let mut trace = create_test_trace(vec![span_with_error]);

        let (keep, priority, decision_maker, _) = sampler.run_samplers(&mut trace);
        assert!(keep);
        assert_eq!(priority, PRIORITY_AUTO_KEEP);
        assert_eq!(decision_maker, ""); // Error sampler doesn't set decision_maker

        // Test legacy path: user priority is respected
        let mut sampler = create_test_sampler();
        sampler.probabilistic_sampler_enabled = false; // Use legacy path

        let mut metrics = HashMap::new();
        metrics.insert(SAMPLING_PRIORITY_METRIC_KEY.to_string(), 2.0);
        let span = create_test_span_with_metrics(12345, 1, metrics);
        let mut trace = create_test_trace(vec![span]);

        let (keep, priority, decision_maker, _) = sampler.run_samplers(&mut trace);
        assert!(keep);
        assert_eq!(priority, 2); // UserKeep
        assert_eq!(decision_maker, DECISION_MAKER_MANUAL_PRIORITY); // manual decision
    }

    #[test]
    fn test_empty_trace_handling() {
        let mut sampler = create_test_sampler();
        let mut trace = create_test_trace(vec![]);

        let (keep, priority, _, _) = sampler.run_samplers(&mut trace);
        assert!(!keep);
        assert_eq!(priority, PRIORITY_AUTO_DROP);
    }

    #[test]
    fn test_root_span_detection() {
        let sampler = create_test_sampler();

        // Test 1: Root span with parent_id = 0 (common case)
        let root_span = DdSpan::new(
            MetaString::from("service"),
            MetaString::from("operation"),
            MetaString::from("resource"),
            MetaString::from("type"),
            12345,
            1,
            0, // parent_id = 0 indicates root
            0,
            1000,
            0,
        );
        let child_span = DdSpan::new(
            MetaString::from("service"),
            MetaString::from("child_op"),
            MetaString::from("resource"),
            MetaString::from("type"),
            12345,
            2,
            1, // parent_id = 1 (points to root)
            100,
            500,
            0,
        );
        // Put root span second to test that we find it even when not first
        let trace = create_test_trace(vec![child_span.clone(), root_span.clone()]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();
        assert_eq!(trace.spans()[root_idx].span_id(), 1);

        // Test 2: Orphaned span (parent not in trace)
        let orphan_span = DdSpan::new(
            MetaString::from("service"),
            MetaString::from("orphan"),
            MetaString::from("resource"),
            MetaString::from("type"),
            12345,
            3,
            999, // parent_id = 999 (doesn't exist in trace)
            200,
            300,
            0,
        );
        let trace = create_test_trace(vec![orphan_span]);
        let root_idx = sampler.get_root_span_index(&trace).unwrap();
        assert_eq!(trace.spans()[root_idx].span_id(), 3);

        // Test 3: Multiple root candidates: should return the last one found (index 1)
        let span1 = create_test_span(12345, 1, 0);
        let span2 = create_test_span(12345, 2, 0);
        let trace = create_test_trace(vec![span1, span2]);
        // Both have parent_id = 0, should return the last one found (span_id = 2)
        let root_idx = sampler.get_root_span_index(&trace).unwrap();
        assert_eq!(trace.spans()[root_idx].span_id(), 2);
    }

    #[test]
    fn test_single_span_sampling() {
        let mut sampler = create_test_sampler();

        // Test 1: Trace with SSS tags should be kept even when probabilistic would drop it
        sampler.sampling_rate = 0.0; // 0% sampling rate - should drop everything
        sampler.probabilistic_sampler_enabled = true;

        // Create span with SSS metric
        let mut metrics_map = saluki_common::collections::FastHashMap::default();
        metrics_map.insert(MetaString::from(KEY_SPAN_SAMPLING_MECHANISM), 8.0); // Any value
        let sss_span = create_test_span(12345, 1, 0).with_metrics(metrics_map.clone());

        // Create regular span without SSS
        let regular_span = create_test_span(12345, 2, 0);

        let mut trace = create_test_trace(vec![sss_span.clone(), regular_span]);

        // Apply SSS
        let modified = sampler.single_span_sampling(&mut trace);
        assert!(modified);
        assert_eq!(trace.spans().len(), 1); // Only SSS span kept
        assert_eq!(trace.spans()[0].span_id(), 1); // It's the SSS span

        // Check that trace has been marked as kept with high priority
        assert!(trace.sampling().is_some());
        assert_eq!(trace.sampling().as_ref().unwrap().priority, Some(PRIORITY_USER_KEEP));

        // Test 2: Trace without SSS tags should not be modified
        let trace_without_sss = create_test_trace(vec![create_test_span(12345, 3, 0)]);
        let mut trace_copy = trace_without_sss.clone();
        let modified = sampler.single_span_sampling(&mut trace_copy);
        assert!(!modified);
        assert_eq!(trace_copy.spans().len(), trace_without_sss.spans().len());
    }

    #[test]
    fn test_analytics_events() {
        let sampler = create_test_sampler();

        // Test 1: Trace with analyzed spans
        let mut metrics_map = saluki_common::collections::FastHashMap::default();
        metrics_map.insert(MetaString::from(KEY_ANALYZED_SPANS), 1.0);
        let analyzed_span = create_test_span(12345, 1, 0).with_metrics(metrics_map.clone());
        let regular_span = create_test_span(12345, 2, 0);

        let trace = create_test_trace(vec![analyzed_span.clone(), regular_span]);

        let analyzed_spans = sampler.get_analyzed_spans(&trace);
        assert_eq!(analyzed_spans.len(), 1);
        assert_eq!(analyzed_spans[0].span_id(), 1);

        assert!(sampler.has_analyzed_spans(&trace));

        // Test 2: Trace without analyzed spans
        let trace_no_analytics = create_test_trace(vec![create_test_span(12345, 3, 0)]);
        let analyzed_spans = sampler.get_analyzed_spans(&trace_no_analytics);
        assert!(analyzed_spans.is_empty());
        assert!(!sampler.has_analyzed_spans(&trace_no_analytics));
    }

    #[test]
    fn test_probabilistic_sampling_with_prob_rate_key() {
        let mut sampler = create_test_sampler();
        sampler.sampling_rate = 0.75; // 75% sampling rate
        sampler.probabilistic_sampler_enabled = true;

        // Use a trace ID that we know will be sampled
        let trace_id = 12345_u64;
        let root_span = DdSpan::new(
            MetaString::from("service"),
            MetaString::from("operation"),
            MetaString::from("resource"),
            MetaString::from("type"),
            trace_id,
            1,
            0, // parent_id = 0 indicates root
            0,
            1000,
            0,
        );
        let mut trace = create_test_trace(vec![root_span]);

        let (keep, priority, decision_maker, root_span_idx) = sampler.run_samplers(&mut trace);

        if keep && decision_maker == DECISION_MAKER_PROBABILISTIC {
            // If sampled probabilistically, check that probRateKey was already added
            assert_eq!(priority, PRIORITY_AUTO_KEEP);
            assert_eq!(decision_maker, DECISION_MAKER_PROBABILISTIC); // probabilistic sampling marker

            // Check that the root span already has the probRateKey (it should have been added in run_samplers)
            let root_idx = root_span_idx.unwrap_or(0);
            let root_span = &trace.spans()[root_idx];
            assert!(root_span.metrics().contains_key(PROB_RATE_KEY));
            assert_eq!(*root_span.metrics().get(PROB_RATE_KEY).unwrap(), 0.75);

            // Test that apply_sampling_metadata still works correctly for other metadata
            let mut trace_with_metadata = trace.clone();
            sampler.apply_sampling_metadata(&mut trace_with_metadata, keep, priority, decision_maker, root_idx);

            // Check that decision maker tag was added
            let modified_root = &trace_with_metadata.spans()[root_idx];
            assert!(modified_root.meta().contains_key(TAG_DECISION_MAKER));
            assert_eq!(
                modified_root.meta().get(TAG_DECISION_MAKER).unwrap(),
                &MetaString::from(DECISION_MAKER_PROBABILISTIC)
            );
        }
    }
}

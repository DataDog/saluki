//! V1 trace sampling transform.
//!
//! Implements `runSamplersV1` from `pkg/trace/agent/agent.go`: reads the tracer-set
//! sampling priority from each chunk, runs the appropriate sampler(s), and writes the
//! final decision back to `chunk.priority` / `chunk.dropped_trace` in place.
//!
//! Unlike the OTLP-path `TraceSampler`, the V1 path carries sampling decisions
//! pre-made by the tracer; the agent's role is to:
//! 1. Respect and count those decisions for the rate-feedback loop.
//! 2. Override `PriorityAutoDrop` traces when the rare sampler or error sampler fires.
//! 3. Propagate per-service rates back to tracers via the `ApmReceiver` HTTP response.

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::rate::TokenBucket;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{trace::Trace, Event},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use std::time::{Duration, SystemTime};
use tracing::debug;

mod no_priority;
mod priority;
mod rare_sampler;

use self::no_priority::V1NoPrioritySampler;
use self::priority::V1PrioritySampler;
use self::rare_sampler::V1RareSampler;

use crate::common::datadog::apm::ApmConfig;
use crate::sources::apm::sampling_rates::V1SamplingRatesHandle;

/// Sentinel indicating the tracer set no priority (matches Go's `PriorityNone = math.MinInt8`).
const PRIORITY_NONE: i32 = i8::MIN as i32;

const PRIORITY_AUTO_KEEP: i32 = 1;
const ERROR_SAMPLER_BURST: usize = 100;

/// Configuration for the V1 trace sampler transform.
pub struct V1TraceSamplerConfiguration {
    apm_config: ApmConfig,
    sampling_rates: V1SamplingRatesHandle,
}

impl V1TraceSamplerConfiguration {
    /// Creates a new `V1TraceSamplerConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self {
            apm_config,
            sampling_rates: V1SamplingRatesHandle::new(),
        })
    }

    /// Attaches a shared [`V1SamplingRatesHandle`] so the sampler can push rates to the
    /// APM receiver source for inclusion in HTTP responses.
    pub fn with_sampling_rates(mut self, handle: V1SamplingRatesHandle) -> Self {
        self.sampling_rates = handle;
        self
    }
}

#[async_trait]
impl SynchronousTransformBuilder for V1TraceSamplerConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let error_token_bucket = if self.apm_config.error_sampling_enabled() {
            Some(TokenBucket::new(self.apm_config.errors_per_second(), ERROR_SAMPLER_BURST))
        } else {
            None
        };

        // TODO: implement the probabilistic sampler path from the Go agent
        // (agent.go ProbabilisticSamplerEnabled branch). Users who enable
        // apm_config.probabilistic_sampler.enabled will silently receive
        // the priority-sampler path instead.
        if self.apm_config.probabilistic_sampler_enabled() {
            tracing::warn!(
                "apm_config.probabilistic_sampler.enabled is set but the V1 trace sampler \
                 does not yet implement the probabilistic path; falling back to priority sampler"
            );
        }

        let sampler = V1TraceSampler {
            priority_sampler: V1PrioritySampler::new(
                self.apm_config.default_env().clone(),
                self.apm_config.target_traces_per_second(),
                1.0,
                self.sampling_rates.clone(),
            ),
            no_priority_sampler: V1NoPrioritySampler::new(self.apm_config.target_traces_per_second()),
            rare_sampler: V1RareSampler::new(
                self.apm_config.rare_sampler_enabled(),
                self.apm_config.rare_sampler_tps(),
                Duration::from_secs_f64(self.apm_config.rare_sampler_cooldown_period_secs()),
                self.apm_config.rare_sampler_cardinality(),
            ),
            error_token_bucket,
            error_sampling_enabled: self.apm_config.error_sampling_enabled(),
            error_tracking_standalone: self.apm_config.error_tracking_standalone_enabled(),
        };

        Ok(Box::new(sampler))
    }
}

impl MemoryBounds for V1TraceSamplerConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<V1TraceSampler>("component struct");
    }
}

pub struct V1TraceSampler {
    priority_sampler: V1PrioritySampler,
    no_priority_sampler: V1NoPrioritySampler,
    rare_sampler: V1RareSampler,
    error_token_bucket: Option<TokenBucket>,
    error_sampling_enabled: bool,
    error_tracking_standalone: bool,
}

impl V1TraceSampler {
    /// Implements `runSamplersV1` / `traceSamplingV1` from the Go Trace Agent.
    ///
    /// Returns `true` if the trace should be forwarded, `false` if it should be
    /// removed from the buffer entirely. In ETS mode the trace is always forwarded
    /// (with `dropped_trace` set to reflect whether it was a kept or dropped trace).
    fn process_trace(
        &mut self,
        now: SystemTime,
        trace: &mut Trace,
        tracer_env: &str,
        client_dropped_p0s_weight: f64,
    ) -> bool {
        if trace.spans().is_empty() {
            return false;
        }

        // ── Error Tracking Standalone (ETS) ────────────────────────────────────
        if self.error_tracking_standalone {
            let has_error = trace.spans().iter().any(|s| s.error() != 0);
            let keep = has_error
                && self
                    .error_token_bucket
                    .as_mut()
                    .map(|b| b.allow())
                    .unwrap_or(true);
            trace.dropped_trace = !keep;
            return true;
        }

        // ── Rare sampler runs unconditionally before any keep/drop decision ─────
        let rare = self.rare_sampler.sample(trace.spans());

        // ── Manual/user drop: hard drop, no overrides possible ─────────────────
        // TODO: implement the full isManualUserDropV1 check from the Go agent.
        let priority = trace.priority.unwrap_or(PRIORITY_NONE);
        if priority < 0 {
            trace.dropped_trace = true;
            return false;
        }

        // ── Rare sampler override ───────────────────────────────────────────────
        if rare {
            trace.priority = Some(PRIORITY_AUTO_KEEP);
            trace.dropped_trace = false;
            debug!(trace_id_low = trace.trace_id_low, "Keeping V1 trace chunk: rare sampler override.");
            return true;
        }

        // ── Priority / NoPriority path ──────────────────────────────────────────
        let has_priority = trace.priority.is_some();

        let root_idx = find_root_span_idx(trace.spans());

        let keep = if has_priority {
            let spans = trace.spans_mut();
            let root = &mut spans[root_idx];
            self.priority_sampler.sample(now, priority, root, tracer_env, client_dropped_p0s_weight)
        } else {
            self.no_priority_sampler.sample()
        };

        if keep {
            // Normalize PRIORITY_NONE so the encoder never writes an undefined priority.
            if trace.priority.is_none() {
                trace.priority = Some(PRIORITY_AUTO_KEEP);
            }
            trace.dropped_trace = false;
            debug!(
                trace_id_low = trace.trace_id_low,
                priority = trace.priority,
                has_priority,
                "Keeping V1 trace chunk: priority/no-priority sampler."
            );
            return true;
        }

        // ── Error sampler as final override ────────────────────────────────────
        if self.error_sampling_enabled && trace.spans().iter().any(|s| s.error() != 0) {
            if let Some(ref mut bucket) = self.error_token_bucket {
                if bucket.allow() {
                    trace.priority = Some(PRIORITY_AUTO_KEEP);
                    trace.dropped_trace = false;
                    debug!(trace_id_low = trace.trace_id_low, "Keeping V1 trace chunk: error sampler override.");
                    return true;
                }
            }
        }

        // Normalize PRIORITY_NONE on the drop path too.
        if trace.priority.is_none() {
            trace.priority = Some(0); // PRIORITY_AUTO_DROP
        }
        debug!(
            trace_id_low = trace.trace_id_low,
            priority = trace.priority,
            "Dropping V1 trace chunk."
        );
        false
    }
}

impl SynchronousTransform for V1TraceSampler {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        let now = SystemTime::now();
        let mut kept = 0u32;
        let mut dropped = 0u32;
        buffer.remove_if(|event| match event {
            Event::Trace(trace) => {
                let tracer_env = trace.env.clone();
                let weight = trace.client_dropped_p0s_weight;
                let remove = !self.process_trace(now, trace, tracer_env.as_ref(), weight);
                if remove {
                    dropped += 1;
                } else {
                    kept += 1;
                }
                remove
            }
            _ => false,
        });
        if kept + dropped > 0 {
            debug!(kept, dropped, "V1 trace sampler processed buffer.");
        }
    }
}

/// Find the index of the root span (parent_id == 0). Falls back to the last span.
fn find_root_span_idx(spans: &[saluki_core::data_model::event::trace::Span]) -> usize {
    let len = spans.len();

    // Fast path: scan from the end (tracers often report root last).
    for i in (0..len).rev() {
        if spans[i].parent_id() == 0 {
            return i;
        }
    }

    // Build parent→child map and remove entries whose parent exists in the trace.
    let mut parent_to_child: std::collections::HashMap<u64, usize> = spans
        .iter()
        .enumerate()
        .map(|(i, s)| (s.parent_id(), i))
        .collect();
    for span in spans {
        parent_to_child.remove(&span.span_id());
    }
    if let Some((&_, &idx)) = parent_to_child.iter().next() {
        return idx;
    }

    len - 1
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::Trace;
    use stringtheory::MetaString;

    use super::*;
    use crate::sources::apm::sampling_rates::V1SamplingRatesHandle;

    fn make_sampler() -> V1TraceSampler {
        V1TraceSampler {
            priority_sampler: V1PrioritySampler::new(
                MetaString::from_static("prod"),
                10.0,
                1.0,
                V1SamplingRatesHandle::new(),
            ),
            no_priority_sampler: V1NoPrioritySampler::new(10.0),
            rare_sampler: V1RareSampler::new(false, 5.0, Duration::from_secs(300), 200),
            error_token_bucket: Some(TokenBucket::new(10.0, 100)),
            error_sampling_enabled: true,
            error_tracking_standalone: false,
        }
    }

    fn make_span(parent_id: u64, error: bool) -> saluki_core::data_model::event::trace::Span {
        saluki_core::data_model::event::trace::Span::new(
            "svc", "op", "res", "web", 0, 1, parent_id, 0, 1000, if error { 1 } else { 0 },
        )
    }

    fn make_trace(priority: i32, spans: Vec<saluki_core::data_model::event::trace::Span>) -> Trace {
        let mut trace = Trace::new(spans);
        if priority == PRIORITY_NONE {
            trace.priority = None;
        } else {
            trace.priority = Some(priority);
        }
        trace
    }

    fn process(sampler: &mut V1TraceSampler, trace: &mut Trace) -> bool {
        sampler.process_trace(SystemTime::now(), trace, "prod", 0.0)
    }

    // ── Basic keep/drop ─────────────────────────────────────────────────────

    #[test]
    fn empty_chunk_is_dropped() {
        let mut s = make_sampler();
        let mut trace = make_trace(0, vec![]);
        assert!(!process(&mut s, &mut trace));
    }

    #[test]
    fn user_drop_is_hard_dropped() {
        let mut s = make_sampler();
        let mut trace = make_trace(-1, vec![make_span(0, false)]);
        assert!(!process(&mut s, &mut trace));
        assert!(trace.dropped_trace);
    }

    #[test]
    fn auto_keep_is_forwarded() {
        let mut s = make_sampler();
        let mut trace = make_trace(1, vec![make_span(0, false)]);
        assert!(process(&mut s, &mut trace));
        assert!(!trace.dropped_trace);
    }

    #[test]
    fn user_keep_is_forwarded() {
        let mut s = make_sampler();
        let mut trace = make_trace(2, vec![make_span(0, false)]);
        assert!(process(&mut s, &mut trace));
        assert!(!trace.dropped_trace);
    }

    #[test]
    fn auto_drop_with_error_is_kept_by_error_sampler() {
        let mut s = make_sampler();
        let mut trace = make_trace(0, vec![make_span(0, true)]);
        assert!(process(&mut s, &mut trace));
        assert_eq!(trace.priority, Some(PRIORITY_AUTO_KEEP));
        assert!(!trace.dropped_trace);
    }

    #[test]
    fn auto_drop_without_error_no_rare_is_dropped() {
        let mut s = V1TraceSampler {
            error_token_bucket: None,
            error_sampling_enabled: false,
            ..make_sampler()
        };
        let mut trace = make_trace(0, vec![make_span(0, false)]);
        assert!(!process(&mut s, &mut trace));
    }

    // ── Rare sampler ────────────────────────────────────────────────────────

    #[test]
    fn rare_sampler_overrides_auto_drop_first_occurrence() {
        let mut s = V1TraceSampler {
            rare_sampler: V1RareSampler::new(true, 1000.0, Duration::from_secs(300), 200),
            error_token_bucket: None,
            error_sampling_enabled: false,
            ..make_sampler()
        };
        let mut trace = make_trace(0, vec![make_span(0, false)]);
        assert!(process(&mut s, &mut trace));
        assert_eq!(trace.priority, Some(PRIORITY_AUTO_KEEP));
    }

    #[test]
    fn rare_sampler_runs_before_drop_decision() {
        let mut s = V1TraceSampler {
            rare_sampler: V1RareSampler::new(true, 1000.0, Duration::from_secs(300), 200),
            error_token_bucket: None,
            error_sampling_enabled: false,
            ..make_sampler()
        };
        let mut trace = make_trace(0, vec![make_span(0, false)]);
        assert!(process(&mut s, &mut trace), "rare should keep first occurrence");

        let mut trace2 = make_trace(0, vec![make_span(0, false)]);
        assert!(!process(&mut s, &mut trace2), "rare should not repeat-sample within TTL");
    }

    // ── PriorityNone path ───────────────────────────────────────────────────

    #[test]
    fn priority_none_goes_to_no_priority_sampler() {
        let mut s = V1TraceSampler {
            priority_sampler: V1PrioritySampler::new(
                MetaString::from_static("prod"),
                0.0,
                1.0,
                V1SamplingRatesHandle::new(),
            ),
            no_priority_sampler: V1NoPrioritySampler::new(10000.0),
            rare_sampler: V1RareSampler::new(false, 5.0, Duration::from_secs(300), 200),
            error_token_bucket: None,
            error_sampling_enabled: false,
            error_tracking_standalone: false,
        };
        let mut trace = make_trace(PRIORITY_NONE, vec![make_span(0, false)]);
        let result = process(&mut s, &mut trace);
        let _ = result;
    }

    // ── ETS mode ────────────────────────────────────────────────────────────

    #[test]
    fn ets_keeps_error_trace() {
        let mut s = V1TraceSampler {
            error_tracking_standalone: true,
            error_token_bucket: Some(TokenBucket::new(10.0, 100)),
            ..make_sampler()
        };
        let mut trace = make_trace(0, vec![make_span(0, true)]);
        assert!(process(&mut s, &mut trace));
        assert!(!trace.dropped_trace);
    }

    #[test]
    fn ets_drops_non_error_trace_but_forwards_it() {
        let mut s = V1TraceSampler {
            error_tracking_standalone: true,
            error_token_bucket: Some(TokenBucket::new(10.0, 100)),
            ..make_sampler()
        };
        let mut trace = make_trace(1, vec![make_span(0, false)]);
        assert!(process(&mut s, &mut trace));
        assert!(trace.dropped_trace, "non-error ETS trace must have dropped_trace=true");
    }
}

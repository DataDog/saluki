//! V1 trace sampling transform.
//!
//! Simplified sampler for APM v1 traces. Because sampling decisions are pre-made by the tracer
//! and carried in the chunk's `priority` field, this transform is substantially simpler than the
//! OTLP-path `TraceSampler`:
//!
//! - `priority < 0` (UserDrop): hard drop — no override possible.
//! - `priority > 0` (UserKeep / AutoKeep): forward as-is.
//! - `priority == 0` (AutoDrop): check the rare sampler and error sampler for overrides.
//!
//! The final sampling decision is written back to `chunk.priority` and `chunk.dropped_trace`.
//! There is no separate `TraceSampling` metadata struct — V1 chunks are self-contained.

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::rate::TokenBucket;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{trace::v1::V1TraceChunk, Event},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use std::time::Duration;
use tracing::debug;

mod rare_sampler;
use self::rare_sampler::V1RareSampler;

use crate::common::datadog::apm::ApmConfig;

const PRIORITY_AUTO_DROP: i32 = 0;
const PRIORITY_AUTO_KEEP: i32 = 1;

// Burst capacity for the error sampler token bucket.
const ERROR_SAMPLER_BURST: usize = 100;

/// Configuration for the V1 trace sampler transform.
#[derive(Debug)]
pub struct V1TraceSamplerConfiguration {
    apm_config: ApmConfig,
}

impl V1TraceSamplerConfiguration {
    /// Creates a new `V1TraceSamplerConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self { apm_config })
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

        let sampler = V1TraceSampler {
            error_token_bucket,
            rare_sampler: V1RareSampler::new(
                self.apm_config.rare_sampler_enabled(),
                self.apm_config.rare_sampler_tps(),
                Duration::from_secs_f64(self.apm_config.rare_sampler_cooldown_period_secs()),
                self.apm_config.rare_sampler_cardinality(),
            ),
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
    error_token_bucket: Option<TokenBucket>,
    rare_sampler: V1RareSampler,
}

impl V1TraceSampler {
    /// Evaluates a chunk against all configured samplers and writes the sampling decision back.
    ///
    /// Returns `true` if the chunk should be forwarded downstream, `false` if it should be
    /// removed from the buffer.
    fn process_chunk(&mut self, chunk: &mut V1TraceChunk) -> bool {
        if chunk.spans.is_empty() {
            return false;
        }

        // UserDrop: hard drop, no override possible.
        if chunk.priority < 0 {
            return false;
        }

        // UserKeep or AutoKeep from the tracer: forward as-is.
        if chunk.priority > PRIORITY_AUTO_DROP {
            chunk.dropped_trace = false;
            return true;
        }

        // AutoDrop (priority == 0): check rare sampler first, then error sampler.
        if self.rare_sampler.sample(chunk) {
            chunk.priority = PRIORITY_AUTO_KEEP;
            chunk.dropped_trace = false;
            return true;
        }

        let has_error = chunk.spans.iter().any(|s| s.error);
        if has_error {
            if let Some(ref mut bucket) = self.error_token_bucket {
                if bucket.allow() {
                    chunk.priority = PRIORITY_AUTO_KEEP;
                    chunk.dropped_trace = false;
                    return true;
                }
            }
        }

        debug!(
            trace_id_low = chunk.trace_id_low,
            "Dropping V1 trace chunk with priority {}", chunk.priority
        );
        false
    }
}

impl SynchronousTransform for V1TraceSampler {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        buffer.remove_if(|event| match event {
            Event::V1Trace(trace) => !self.process_chunk(&mut trace.chunk),
            _ => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::v1::{V1Span, V1TraceChunk};
    use stringtheory::MetaString;

    use super::*;
    use crate::transforms::v1_trace_sampler::rare_sampler::V1RareSampler;

    fn make_sampler() -> V1TraceSampler {
        V1TraceSampler {
            error_token_bucket: Some(TokenBucket::new(10.0, 100)),
            rare_sampler: V1RareSampler::new(false, 5.0, Duration::from_secs(300), 200),
        }
    }

    fn make_span(parent_id: u64, error: bool) -> V1Span {
        V1Span {
            service: MetaString::from_static("svc"),
            name: MetaString::from_static("op"),
            resource: MetaString::from_static("res"),
            span_id: 1,
            parent_id,
            start: 0,
            duration: 1000,
            error,
            attributes: Vec::new(),
            span_type: MetaString::from_static("web"),
            links: Vec::new(),
            events: Vec::new(),
            env: MetaString::default(),
            version: MetaString::default(),
            component: MetaString::default(),
            kind: 0,
        }
    }

    fn make_chunk(priority: i32, spans: Vec<V1Span>) -> V1TraceChunk {
        V1TraceChunk {
            priority,
            origin: MetaString::default(),
            attributes: Vec::new(),
            spans,
            dropped_trace: false,
            trace_id_high: 0,
            trace_id_low: 1,
            sampling_mechanism: 0,
        }
    }

    #[test]
    fn empty_chunk_is_dropped() {
        let mut sampler = make_sampler();
        let mut chunk = make_chunk(0, vec![]);
        assert!(!sampler.process_chunk(&mut chunk));
    }

    #[test]
    fn user_drop_is_hard_dropped() {
        let mut sampler = make_sampler();
        let mut chunk = make_chunk(-1, vec![make_span(0, false)]);
        assert!(!sampler.process_chunk(&mut chunk));
    }

    #[test]
    fn auto_keep_is_forwarded() {
        let mut sampler = make_sampler();
        let mut chunk = make_chunk(1, vec![make_span(0, false)]);
        assert!(sampler.process_chunk(&mut chunk));
        assert!(!chunk.dropped_trace);
    }

    #[test]
    fn user_keep_is_forwarded() {
        let mut sampler = make_sampler();
        let mut chunk = make_chunk(2, vec![make_span(0, false)]);
        assert!(sampler.process_chunk(&mut chunk));
        assert!(!chunk.dropped_trace);
    }

    #[test]
    fn auto_drop_with_error_is_kept() {
        let mut sampler = make_sampler();
        let mut chunk = make_chunk(0, vec![make_span(0, true)]);
        assert!(sampler.process_chunk(&mut chunk));
        assert_eq!(chunk.priority, PRIORITY_AUTO_KEEP);
        assert!(!chunk.dropped_trace);
    }

    #[test]
    fn auto_drop_without_error_is_dropped() {
        let mut sampler = V1TraceSampler {
            error_token_bucket: None,
            rare_sampler: V1RareSampler::new(false, 5.0, Duration::from_secs(300), 200),
        };
        let mut chunk = make_chunk(0, vec![make_span(0, false)]);
        assert!(!sampler.process_chunk(&mut chunk));
    }

    #[test]
    fn rare_sampler_overrides_auto_drop() {
        let mut sampler = V1TraceSampler {
            error_token_bucket: None,
            rare_sampler: V1RareSampler::new(true, 1000.0, Duration::from_secs(300), 200),
        };
        let mut chunk = make_chunk(0, vec![make_span(0, false)]);
        // First occurrence of this signature should be kept.
        assert!(sampler.process_chunk(&mut chunk));
        assert_eq!(chunk.priority, PRIORITY_AUTO_KEEP);
    }
}

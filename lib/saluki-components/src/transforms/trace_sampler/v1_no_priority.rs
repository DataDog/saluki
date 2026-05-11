//! V1 no-priority sampler.
//!
//! Used for V1 trace chunks where the tracer did not set a sampling priority
//! (the sentinel value `i8::MIN as i32 = -128`). This situation is uncommon —
//! modern DD tracers always send a priority — so a simple token-bucket is
//! sufficient for the initial implementation.
//!
//! TODO: Replace with a full score-sampler integration (weighted signature
//! counting + per-signature rate computation) matching `NoPrioritySampler.SampleV1`
//! from `pkg/trace/sampler/scoresampler.go`.

use saluki_common::rate::TokenBucket;

const NO_PRIORITY_BURST: usize = 100;

/// Token-bucket sampler for V1 chunks without a tracer-set priority.
pub(super) struct V1NoPrioritySampler {
    bucket: TokenBucket,
}

impl V1NoPrioritySampler {
    pub(super) fn new(target_tps: f64) -> Self {
        Self {
            bucket: TokenBucket::new(target_tps, NO_PRIORITY_BURST),
        }
    }

    /// Returns `true` if the chunk should be kept.
    pub(super) fn sample(&mut self) -> bool {
        self.bucket.allow()
    }
}

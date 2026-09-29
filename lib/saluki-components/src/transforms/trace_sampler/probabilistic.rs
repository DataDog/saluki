//! Probabilistic sampling.

use super::signature::{fnv1a_32_continue, fnv1a_32_start};

// The trace ID is hashed into one of 16384 buckets, and a trace is kept when its bucket number is
// below the sampling rate scaled to the same range.
const NUM_PROBABILISTIC_BUCKETS: u32 = 0x4000;
const BITMASK_HASH_BUCKETS: u32 = NUM_PROBABILISTIC_BUCKETS - 1;

/// Sampling-rate span attribute written to the root span of kept traces.
pub(super) const PROB_RATE_KEY: &str = "_dd.prob_sr";

/// Bucketed probabilistic sampler.
pub(super) struct ProbabilisticSampler {
    /// FNV-1a-32 state with the seed folded in. The seed prefix is constant per instance, so it is
    /// hashed once at construction rather than on every trace.
    hash_state: u32,
    full_trace_id_mode: bool,
}

impl ProbabilisticSampler {
    pub(super) fn new(hash_seed: u32, full_trace_id_mode: bool) -> Self {
        Self {
            hash_state: fnv1a_32_start(&hash_seed.to_le_bytes()),
            full_trace_id_mode,
        }
    }

    /// Deterministically samples a trace from its trace ID.
    pub(super) fn sample(&self, trace_id_high: u64, trace_id_low: u64, sampling_rate: f64) -> bool {
        // The rate bounds decide without hashing: the highest bucket, 16383, clears the 16384
        // cutoff a rate of 1.0 scales to, and no bucket clears a cutoff of 0.
        if sampling_rate >= 1.0 {
            return true;
        }
        if sampling_rate <= 0.0 {
            return false;
        }

        // The hash input is the 16-byte big-endian trace ID. Legacy mode hashes only the low
        // half, zero-padded, so a trace keeps its bucket when the upper bits are lost in
        // propagation; full mode hashes both halves, staying aligned with samplers that always
        // decide on 128-bit IDs.
        let trace_id = if self.full_trace_id_mode {
            (u128::from(trace_id_high) << 64) | u128::from(trace_id_low)
        } else {
            u128::from(trace_id_low) << 64
        };

        let hash = fnv1a_32_continue(self.hash_state, &trace_id.to_be_bytes());
        let scaled_sampling_percentage = (sampling_rate * NUM_PROBABILISTIC_BUCKETS as f64) as u32;
        (hash & BITMASK_HASH_BUCKETS) < scaled_sampling_percentage
    }
}

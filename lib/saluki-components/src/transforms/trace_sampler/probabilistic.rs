//! Probabilistic sampling.

use super::signature::fnv1a_32;

// Knuth multiplicative hashing factor for deterministic sampling.
//
// This constant is shared across datadog-agent, Datadog libraries, and OpenTelemetry.
// It is currently unused, but kept to mirror the upstream implementation for future work.
#[allow(dead_code)]
const KNUTH_FACTOR: u64 = 1111111111111111111;

// Probabilistic sampler constants (matching datadog-agent's bucketed sampler).
// These constants exist to match the behavior of the OTEL probabilistic sampler.
// See: https://github.com/open-telemetry/opentelemetry-collector-contrib/.../probabilisticsamplerprocessor/tracesprocessor.go#L38-L42
const NUM_PROBABILISTIC_BUCKETS: u32 = 0x4000;
const BITMASK_HASH_BUCKETS: u32 = NUM_PROBABILISTIC_BUCKETS - 1;

/// `probRateKey` indicates the percentage sampling rate configured for the probabilistic sampler.
pub(super) const PROB_RATE_KEY: &str = "_dd.prob_sr";

/// Probabilistic sampler.
pub(super) struct ProbabilisticSampler;

impl ProbabilisticSampler {
    /// Deterministically sample a trace based on its trace ID.
    ///
    /// This mirrors the behavior of the Datadog Agent's bucketed probabilistic sampler.
    pub(super) fn sample(trace_id: u64, sampling_rate: f64) -> bool {
        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/probabilistic.go#L62
        // we take in a trace id (randomly generated) and hash/mask it to get a number between 0 and 0x3FFF and compare it to the sampling rate.
        // TODO: add full trace id mode (off by default)

        // to match the agent behaviour, we need to make the array 16 bytes long, this is used for full trace id mode
        // but we require it now to match the hash.
        let mut tid = [0u8; 16];
        tid[..8].copy_from_slice(&trace_id.to_be_bytes());

        // Match the datadog-agent bucketed probabilistic sampler behavior.
        // (Fixed zero hash seed; equivalent to the agent's default when unset.)
        // TODO: make the hash seed configurable
        let hash_seed = [0u8; 4];
        let hash = fnv1a_32(&hash_seed, &tid);
        let scaled_sampling_percentage = (sampling_rate * NUM_PROBABILISTIC_BUCKETS as f64) as u32;
        // bitMaskHashBuckets = 0x3FFF (binary: 0011111111111111 = 14 bits set
        // so we keep the lower 14 bits of the hash.
        (hash & BITMASK_HASH_BUCKETS) < scaled_sampling_percentage
    }
}

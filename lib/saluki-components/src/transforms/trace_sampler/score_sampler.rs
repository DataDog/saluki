use std::time::SystemTime;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{Span, Trace};
use stringtheory::MetaString;

use super::signature::{compute_signature_with_root_and_env, Signature};
use crate::transforms::trace_sampler::core_sampler::Sampler;

// Metric keys for sampling rates
const KEY_SAMPLING_RATE_GLOBAL: &str = "_sample_rate";
const KEY_SAMPLING_RATE_PRE_SAMPLER: &str = "_dd1.sr.rapre";

// ScoreSampler-specific rate keys
pub(super) const ERRORS_RATE_KEY: &str = "_dd.errors_sr";

// shrinkCardinality is the max Signature cardinality before shrinking
const SHRINK_CARDINALITY: usize = 200;

// Constants for deterministic sampling
const MAX_TRACE_ID: u64 = u64::MAX;
const MAX_TRACE_ID_FLOAT: f64 = MAX_TRACE_ID as f64;
// Using a prime number for better distribution
const SAMPLER_HASHER: u64 = 1111111111111111111;

/// ScoreSampler for traces
///
/// ScoreSampler samples pieces of traces by computing a signature based on spans (service, name, rsc, http.status, error.type)
/// scoring it and applying a rate.
/// The rates are applied on the TraceID to maximize the number of chunks with errors caught for the same traceID.
/// For a set traceID: P(chunk1 kept and chunk2 kept) = min(P(chunk1 kept), P(chunk2 kept))
///
/// # Missing
///
/// TODO: Add SampleV1 method for legacy trace format support
/// TODO: Add NoPrioritySampler implementation
pub struct ScoreSampler {
    sampler: Sampler,
    sampling_rate_key: &'static str,
    disabled: bool,
    // When shrinking, the shrink allowlist represents the currently active signatures while new ones get collapsed
    shrink_allow_list: Option<FastHashMap<Signature, f64>>,
}

impl ScoreSampler {
    /// Create a new ScoreSampler with the given sampling rate key and target TPS.
    pub fn new(sampling_rate_key: &'static str, disabled: bool, target_tps: f64, extra_sample_rate: f64) -> Self {
        Self {
            sampler: Sampler::new(extra_sample_rate, target_tps),
            sampling_rate_key,
            disabled,
            shrink_allow_list: None,
        }
    }

    /// Sample counts an incoming trace and tells if it is a sample which has to be kept
    pub fn sample(&mut self, now: SystemTime, trace: &mut Trace, root_idx: usize) -> bool {
        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/scoresampler.go#L71
        if self.disabled {
            return false;
        }

        let spans_len = trace.spans().len();
        if spans_len == 0 || root_idx >= spans_len {
            return false;
        }

        // Compute signature before mutably borrowing the root span.
        let signature = compute_signature_with_root_and_env(trace, root_idx);
        let signature = self.shrink(signature);

        // Update sampler state by counting this trace
        let weight = {
            let spans = trace.spans();
            let root = &spans[root_idx];
            weight_root(root)
        };
        self.sampler.count_weighted_sig(now, &signature, weight);

        // Get the sampling rate for this signature
        let rate = self.sampler.get_signature_sample_rate(&signature);

        // Apply the sampling decision
        let root = &mut trace.spans_mut()[root_idx];
        self.apply_sample_rate(root, rate)
    }

    /// Apply the sampling rate to determine if the trace should be kept.
    fn apply_sample_rate(&self, root: &mut Span, rate: f64) -> bool {
        let initial_rate = get_global_rate(root);
        let new_rate = initial_rate * rate;
        let trace_id = root.trace_id();
        let sampled = sample_by_rate(trace_id, new_rate);

        if sampled {
            self.set_sampling_rate_metric(root, rate);
        }

        sampled
    }

    /// Shrink limits the number of signatures stored in the sampler.
    /// After a cardinality above shrinkCardinality/2 is reached
    /// signatures are spread uniformly on a fixed set of values.
    /// This ensures that ScoreSamplers are memory capped.
    /// When the shrink is triggered, previously active signatures
    /// stay unaffected. New signatures may share the same TPS computation.
    fn shrink(&mut self, sig: Signature) -> Signature {
        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/scoresampler.go#L151
        if self.sampler.size() < (SHRINK_CARDINALITY / 2) as i64 {
            self.shrink_allow_list = None;
            return sig;
        }

        if self.shrink_allow_list.is_none() {
            let (rates, _) = self.sampler.get_all_signature_sample_rates();
            self.shrink_allow_list = Some(rates);
        }

        if let Some(ref map) = self.shrink_allow_list {
            if map.contains_key(&sig) {
                return sig;
            }
        }

        // Map to a limited set of signatures to bound cardinality and force
        // new signatures to share the same bucket and TPS computation.
        Signature(sig.0 % (SHRINK_CARDINALITY as u64 / 2))
    }

    /// Set the sampling rate metric on a span.
    pub fn set_sampling_rate_metric(&self, span: &mut Span, rate: f64) {
        span.metrics_mut()
            .insert(MetaString::from(self.sampling_rate_key), rate);
    }
}

/// Calculate the weight from the span's global rate and presampler rate.
fn weight_root(span: &Span) -> f32 {
    let client_rate = span
        .metrics()
        .get(KEY_SAMPLING_RATE_GLOBAL)
        .copied()
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);

    let pre_sampler_rate = span
        .metrics()
        .get(KEY_SAMPLING_RATE_PRE_SAMPLER)
        .copied()
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);

    (1.0 / (pre_sampler_rate * client_rate)) as f32
}

/// Get the cumulative sample rate of the trace to which this span belongs.
fn get_global_rate(span: &Span) -> f64 {
    span.metrics().get(KEY_SAMPLING_RATE_GLOBAL).copied().unwrap_or(1.0)
}

/// SampleByRate returns whether to keep a trace, based on its ID and a sampling rate.
/// This assumes that trace IDs are nearly uniformly distributed.
fn sample_by_rate(trace_id: u64, rate: f64) -> bool {
    // logic taken from here: https://github.com/DataDog/datadog-agent/blob/angel/support-tail-beginning-wildcard/pkg/trace/sampler/sampler.go#L94
    if rate < 1.0 {
        trace_id.wrapping_mul(SAMPLER_HASHER) < (rate * MAX_TRACE_ID_FLOAT) as u64
    } else {
        true
    }
}

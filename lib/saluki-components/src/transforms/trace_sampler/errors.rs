//! Error sampling.
//!
//! The error sampler catches traces containing spans with errors, ensuring
//! error visibility even at low sampling rates.

use std::time::SystemTime;

use saluki_core::data_model::event::trace::Trace;

use super::score_sampler::{ScoreSampler, ERRORS_RATE_KEY};

/// Error sampler for traces.
///
/// Wraps a ScoreSampler configured specifically for error sampling.
/// This ensures traces with errors are caught even when the main sampler
/// would drop them.
pub(super) struct ErrorsSampler {
    score_sampler: ScoreSampler,
}

impl ErrorsSampler {
    /// Create a new ErrorsSampler with the given configuration.
    pub(super) fn new(error_tps: f64, extra_sample_rate: f64) -> Self {
        let disabled = error_tps == 0.0;
        Self {
            score_sampler: ScoreSampler::new(ERRORS_RATE_KEY, disabled, error_tps, extra_sample_rate),
        }
    }

    /// This method should be called when a trace contains errors and needs to be
    /// evaluated by the error sampler.
    pub(super) fn sample_error(&mut self, now: SystemTime, trace: &mut Trace, root_idx: usize) -> bool {
        // Use the score sampler to make the sampling decision
        self.score_sampler.sample(now, trace, root_idx)
    }
}

#[cfg(test)]
mod tests {
    // logic for these tests are taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/scoresampler_test.go#L23
    use std::time::{Duration, SystemTime};

    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{Span, Trace};
    use stringtheory::MetaString;

    use super::*;
    use crate::transforms::trace_sampler::signature::{compute_signature_with_root_and_env, Signature};

    const BUCKET_DURATION: Duration = Duration::from_secs(5);

    /// Create a test ErrorsSampler with the given TPS configuration.
    fn get_test_errors_sampler(tps: f64) -> ErrorsSampler {
        // No extra fixed sampling, no maximum TPS
        ErrorsSampler::new(tps, 1.0)
    }

    /// Create a test trace with deterministic IDs.
    fn get_test_trace(trace_id: u64) -> (Trace, usize) {
        // Root span
        let root = Span::new(
            MetaString::from("mcnulty"),
            MetaString::from("GET /api"),
            MetaString::from("resource"),
            MetaString::from("web"),
            trace_id,
            1,       // span_id
            0,       // parent_id
            42,      // start
            1000000, // duration
            0,       // error
        );

        // Child span
        let child = Span::new(
            MetaString::from("mcnulty"),
            MetaString::from("SELECT * FROM users"),
            MetaString::from("resource"),
            MetaString::from("sql"),
            trace_id,
            2,      // span_id
            1,      // parent_id
            100,    // start
            200000, // duration
            0,      // error
        );

        let trace = Trace::new(vec![root, child], TagSet::default());
        (trace, 0) // Root is at index 0
    }

    /// Create a test trace with error in root span.
    fn get_test_trace_with_error(trace_id: u64) -> (Trace, usize) {
        // Root span with error
        let root = Span::new(
            MetaString::from("mcnulty"),
            MetaString::from("GET /api"),
            MetaString::from("resource"),
            MetaString::from("web"),
            trace_id,
            1,       // span_id
            0,       // parent_id
            42,      // start
            1000000, // duration
            1,       // error = 1
        );

        // Child span
        let child = Span::new(
            MetaString::from("mcnulty"),
            MetaString::from("SELECT * FROM users"),
            MetaString::from("resource"),
            MetaString::from("sql"),
            trace_id,
            2,      // span_id
            1,      // parent_id
            100,    // start
            200000, // duration
            0,      // error
        );

        let trace = Trace::new(vec![root, child], TagSet::default());
        (trace, 0) // Root is at index 0
    }

    #[test]
    fn test_shrink() {
        // Test that shrink preserves first signatures and collapses later ones.
        // The shrink logic activates when size() >= SHRINK_CARDINALITY/2 (100).
        // When it activates, it builds an allow-list from the current `rates` map.
        // Since `shrink` runs before `count_weighted_sig` in the sample flow, we must
        // advance time one iteration BEFORE hitting the threshold so that `update_rates`
        // runs and populates `rates` with the first batch of signatures.
        let mut sampler = get_test_errors_sampler(10.0);
        let test_time = SystemTime::now();
        let shrink_cardinality = ScoreSampler::test_shrink_cardinality();
        let threshold = shrink_cardinality / 2; // 100

        let mut sigs = Vec::new();
        // Generate 3*shrinkCardinality signatures with different services
        for i in 0..(3 * shrink_cardinality) {
            let (mut trace, root_idx) = get_test_trace(3);
            let spans = trace.spans_mut();
            // modify the non root span to create unique signatures
            spans[1] = spans[1]
                .clone()
                .with_service(MetaString::from(format!("service_{}", i + 1000)));

            let signature = compute_signature_with_root_and_env(&trace, root_idx);
            sigs.push(signature);

            // Advance time at threshold-1 so update_rates runs BEFORE shrink activates.
            // This populates rates with (threshold-1) signatures.
            let sample_time = if i >= threshold - 1 {
                test_time + BUCKET_DURATION
            } else {
                test_time
            };
            sampler.sample_error(sample_time, &mut trace, root_idx);
        }

        // Verify first (threshold-1) signatures are preserved (they're in the allow-list)
        let threshold = shrink_cardinality / 2;
        for (i, sig) in sigs.iter().enumerate().take(threshold - 1) {
            assert_eq!(
                *sig,
                sampler.score_sampler.test_shrink(*sig),
                "Signature at index {} should be preserved",
                i
            );
        }

        // Verify signatures from 2*shrinkCardinality onwards are shrunk
        for (i, sig) in sigs
            .iter()
            .enumerate()
            .skip(2 * shrink_cardinality)
            .take(shrink_cardinality - 1)
        {
            let expected = Signature(sig.0 % (threshold as u64));
            assert_eq!(
                expected,
                sampler.score_sampler.test_shrink(*sig),
                "Signature at index {} should be shrunk",
                i
            );
        }

        // Final size should be bounded by shrink_cardinality
        let size = sampler.score_sampler.test_size();
        assert!(
            size <= shrink_cardinality as i64,
            "Size {} should be <= {}",
            size,
            shrink_cardinality
        );
    }

    #[test]
    fn test_disable() {
        // Create a disabled sampler (TPS = 0)
        let mut sampler = get_test_errors_sampler(0.0);

        // The sampler should never sample anything
        for i in 0..100 {
            let (mut trace, root_idx) = get_test_trace_with_error(i);
            let sampled = sampler.sample_error(SystemTime::now(), &mut trace, root_idx);
            assert!(!sampled, "Disabled sampler should never sample (iteration {})", i);
        }
    }

    #[test]
    fn test_target_tps() {
        // Test the effectiveness of the targetTPS option
        // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/scoresampler_test.go#L102
        let target_tps = 10.0;
        let mut sampler = get_test_errors_sampler(target_tps);

        let generated_tps = 200.0;
        let init_periods = 2;
        let periods = 10;

        let period_seconds = BUCKET_DURATION.as_secs() as f64;
        let traces_per_period = (generated_tps * period_seconds) as usize;

        let mut sampled_count = 0;
        let mut test_time = SystemTime::now();

        for period in 0..(init_periods + periods) {
            test_time += BUCKET_DURATION;

            for i in 0..traces_per_period {
                let (mut trace, root_idx) = get_test_trace_with_error((period * traces_per_period + i) as u64);
                let sampled = sampler.sample_error(test_time, &mut trace, root_idx);

                // Once we got into the stable regime, count the samples
                if period >= init_periods && sampled {
                    sampled_count += 1;
                }
            }
        }

        // We should keep approximately the right percentage of traces
        let expected_ratio = target_tps / generated_tps;
        let actual_ratio = sampled_count as f64 / (traces_per_period as f64 * periods as f64);

        assert!(
            (actual_ratio - expected_ratio).abs() / expected_ratio < 0.2,
            "Expected ratio {:.4}, got {:.4} (sampled {} out of {})",
            expected_ratio,
            actual_ratio,
            sampled_count,
            traces_per_period * periods
        );

        // We should have a throughput of sampled traces around targetTPS
        let actual_tps = sampled_count as f64 / (periods as f64 * BUCKET_DURATION.as_secs() as f64);
        assert!(
            (actual_tps - target_tps).abs() / target_tps < 0.2,
            "Expected TPS {:.2}, got {:.2}",
            target_tps,
            actual_tps
        );
    }
}

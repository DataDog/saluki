//! Error sampling.
//!
//! The error sampler catches traces containing spans with errors, ensuring
//! error visibility even at low sampling rates.

#![allow(dead_code)]

use std::time::SystemTime;

use saluki_core::data_model::event::trace::{Span, Trace};

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

    ///
    /// This method should be called when a trace contains errors and needs to be
    /// evaluated by the error sampler.
    pub(super) fn sample_error(&mut self, now: SystemTime, trace: &mut Trace, root_idx: usize) -> bool {
        // Use the score sampler to make the sampling decision
        self.score_sampler.sample(now, trace, root_idx)
    }

    /// Set the error sampling rate metric on a span.
    pub(super) fn set_sampling_rate_metric(&self, span: &mut Span, rate: f64) {
        self.score_sampler.set_sampling_rate_metric(span, rate);
    }

    /// Get the target TPS for error sampling.
    pub(super) fn get_target_tps(&self) -> f64 {
        self.score_sampler.get_target_tps()
    }

    /// Update the target TPS for error sampling.
    pub(super) fn update_target_tps(&mut self, target_tps: f64) {
        self.score_sampler.update_target_tps(target_tps);
    }
}

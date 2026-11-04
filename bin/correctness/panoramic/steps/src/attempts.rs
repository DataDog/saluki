//! Outcome and diagnostics retained across polling attempts.

use std::fmt;

use panoramic_core::record::AssertionStatus;

/// Tracks whether any attempt reached the product, and the latest diagnostic.
#[derive(Debug, Default)]
pub(crate) struct Attempts {
    reached_product: bool,
    last: Option<String>,
}

impl Attempts {
    /// Records an unsuccessful product answer.
    pub(crate) fn observed(&mut self, observation: impl fmt::Display) {
        self.reached_product = true;
        self.last = Some(observation.to_string());
    }

    /// Records a failure in the observation mechanism.
    pub(crate) fn errored(&mut self, error: impl fmt::Display) {
        self.last = Some(error.to_string());
    }

    /// Classifies a timeout as failed if any attempt reached the product.
    pub(crate) fn status(&self) -> AssertionStatus {
        if self.reached_product {
            AssertionStatus::Failed
        } else {
            AssertionStatus::Errored
        }
    }

    /// Appends the latest attempt to the timeout summary.
    pub(crate) fn explain_expired(&self, summary: &str) -> String {
        match &self.last {
            Some(last) => format!("{summary}; last attempt: {last}"),
            None => summary.to_owned(),
        }
    }
}

//! What each side of a comparison must hold, checked before the sides are compared.
//!
//! Two sides that hold nothing are equal, and so are two sides that hold the same unrelated records. A case lists what
//! each side must hold. The analysis checks each side against the list first, and names the side and the expectation
//! that the side leaves unmet.

use std::num::NonZeroUsize;

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tracing::error;

use crate::steps::intake::metrics::NormalizedMetrics;
use crate::steps::intake::MetricCriteria;

/// What an unmet expectation calls the side that holds the baseline's data.
pub(super) const BASELINE: &str = "baseline";

/// What an unmet expectation calls the side that holds the comparison's data.
pub(super) const COMPARISON: &str = "comparison";

/// Most series with the expected name that an unmet expectation lists.
const MAX_LISTED: usize = 3;

/// What each side must hold of metrics, set in the `metrics` section of a `compare` step.
///
/// Every field is optional. A series count is the number of metric contexts, by type, that remain after the
/// internal-name filter, so internal telemetry never counts toward it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsExpectations {
    /// Fewest series that each side holds after the internal-name filter.
    pub min_series: Option<NonZeroUsize>,

    /// Criteria that one series of each side must meet, one entry for each. A criterion names a metric, and it can add
    /// tags, a type and a value, as `intake_has_metric` does.
    pub series: Vec<MetricCriteria>,
}

impl MetricsExpectations {
    /// Checks that the section sets something to check.
    ///
    /// # Errors
    ///
    /// If the section is empty, an error is returned.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_series.is_none() && self.series.is_empty() {
            return Err("'metrics' sets no expectation: set 'min_series' or 'series'".to_string());
        }
        Ok(())
    }

    /// Checks both sides, which the internal-name filter has already cleaned.
    ///
    /// # Errors
    ///
    /// If a side leaves an expectation unmet, an error is returned with one line of details for each unmet
    /// expectation. Each line names the side.
    pub fn check(
        &self, baseline: &NormalizedMetrics, comparison: &NormalizedMetrics,
    ) -> Result<(), (GenericError, Vec<String>)> {
        require_met(self.unmet(BASELINE, baseline), self.unmet(COMPARISON, comparison))
    }

    /// Returns a line for each expectation that the series of `side` leave unmet.
    fn unmet(&self, side: &str, series: &NormalizedMetrics) -> Vec<String> {
        let mut unmet = Vec::new();

        if let Some(min_series) = self.min_series.filter(|min| series.len() < min.get()) {
            unmet.push(format!(
                "The {} holds {} series after the internal-name filter, but `min_series` is {}.",
                side,
                series.len(),
                min_series
            ));
        }

        for (idx, criteria) in self.series.iter().enumerate() {
            if !criteria.is_met_by(series) {
                unmet.push(format!(
                    "The {} holds no series that meets `series[{}]` ({}): {}.",
                    side,
                    idx,
                    criteria.describe(),
                    describe_named(criteria, series)
                ));
            }
        }

        unmet
    }
}

/// Says why no series meets `criteria`: no series has its name, or the series with its name fail.
fn describe_named(criteria: &MetricCriteria, series: &NormalizedMetrics) -> String {
    let named = series
        .metrics()
        .iter()
        .filter(|metric| metric.context().name() == criteria.name())
        .collect::<Vec<_>>();
    if named.is_empty() {
        return "no series has that name".to_string();
    }

    let failures = named
        .iter()
        .take(MAX_LISTED)
        .map(|metric| {
            let failure = criteria
                .mismatch(metric)
                .map(|mismatch| criteria.describe_mismatch(mismatch, metric))
                .unwrap_or_default();
            format!("{} ({}) fails {}", metric.context(), metric.value_type(), failure)
        })
        .collect::<Vec<_>>();
    format!(
        "{} series have that name: {}{}",
        named.len(),
        failures.join("; "),
        if named.len() > MAX_LISTED { "; and more" } else { "" }
    )
}

/// Passes when neither side leaves an expectation unmet.
///
/// # Errors
///
/// If a side leaves an expectation unmet, an error is returned with the lines of both sides as its details.
pub(super) fn require_met(baseline: Vec<String>, comparison: Vec<String>) -> Result<(), (GenericError, Vec<String>)> {
    if baseline.is_empty() && comparison.is_empty() {
        return Ok(());
    }

    let summary = format!(
        "Expectations are unmet: {} on the baseline and {} on the comparison.",
        baseline.len(),
        comparison.len()
    );
    error!("{}", summary);
    let details = baseline.into_iter().chain(comparison).collect::<Vec<_>>();
    for line in &details {
        error!("{}", line);
    }

    Err((generic_error!("{}", summary), details))
}

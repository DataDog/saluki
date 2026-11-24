//! What each side of a comparison must hold, checked before the sides are compared.
//!
//! Two sides that hold nothing are equal, and so are two sides that hold the same unrelated records. A case lists what
//! each side must hold. The analysis checks each side against the list first, and names the side and the expectation
//! that the side leaves unmet.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use stele::Span;
use tracing::error;

use crate::steps::intake::metrics::NormalizedMetrics;
use crate::steps::intake::MetricCriteria;

/// What an unmet expectation calls the side that holds the baseline's data.
pub(super) const BASELINE: &str = "baseline";

/// What an unmet expectation calls the side that holds the comparison's data.
pub(super) const COMPARISON: &str = "comparison";

/// Most series with the expected name, or span names of the expected service, that an unmet expectation lists.
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

/// What each side must hold of traces. The `traces` section of a `compare` step sets these fields.
///
/// Every field is optional. A trace is a distinct trace ID among the spans of a side.
#[derive(Clone, Debug, Default)]
pub struct TracesExpectations {
    /// Fewest traces that each side holds.
    pub min_traces: Option<NonZeroUsize>,

    /// Fewest spans that each side holds.
    pub min_spans: Option<NonZeroUsize>,

    /// Spans that each side must hold, one entry for each. A span of the side must meet every field of an entry.
    pub spans: Vec<ExpectedSpan>,

    /// Fewest groups of APM stats that each side holds. A group is one aggregation key, whatever its buckets.
    ///
    /// It applies only when the step compares stats.
    pub min_stats_groups: Option<NonZeroUsize>,
}

/// What one side of a trace comparison holds.
pub struct HeldTraces<'a> {
    /// The spans of the side, by span ID.
    pub spans: &'a HashMap<u64, Span>,

    /// The number of distinct trace IDs among the spans.
    pub traces: usize,

    /// The number of groups of APM stats.
    pub stats_groups: usize,
}

impl TracesExpectations {
    /// Checks both sides.
    ///
    /// # Errors
    ///
    /// If a side leaves an expectation unmet, an error is returned with one line of details for each unmet
    /// expectation. Each line names the side.
    pub fn check(
        &self, baseline: &HeldTraces<'_>, comparison: &HeldTraces<'_>,
    ) -> Result<(), (GenericError, Vec<String>)> {
        require_met(self.unmet(BASELINE, baseline), self.unmet(COMPARISON, comparison))
    }

    /// Returns a line for each expectation that `held` leaves unmet.
    fn unmet(&self, side: &str, held: &HeldTraces<'_>) -> Vec<String> {
        let mut unmet = Vec::new();

        let counts = [
            ("traces", "min_traces", held.traces, self.min_traces),
            ("spans", "min_spans", held.spans.len(), self.min_spans),
            (
                "stats groups",
                "min_stats_groups",
                held.stats_groups,
                self.min_stats_groups,
            ),
        ];
        for (what, key, found, min) in counts {
            if let Some(min) = min.filter(|min| found < min.get()) {
                unmet.push(format!(
                    "The {} holds {} {}, but `{}` is {}.",
                    side, found, what, key, min
                ));
            }
        }

        for (idx, expected) in self.spans.iter().enumerate() {
            if !held.spans.values().any(|span| expected.is_met_by(span)) {
                unmet.push(format!(
                    "The {} holds no span that meets `spans[{}]` ({}): {}.",
                    side,
                    idx,
                    expected.describe(),
                    expected.describe_service(held.spans)
                ));
            }
        }

        unmet
    }
}

/// A span that a side must hold, by its service and its operation name or its resource.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "ExpectedSpanFields")]
pub struct ExpectedSpan {
    service: String,
    name: Option<String>,
    resource: Option<String>,
}

/// The fields of [`ExpectedSpan`], as a case writes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedSpanFields {
    service: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

impl TryFrom<ExpectedSpanFields> for ExpectedSpan {
    type Error = String;

    fn try_from(fields: ExpectedSpanFields) -> Result<Self, Self::Error> {
        if fields.name.is_none() && fields.resource.is_none() {
            return Err(format!(
                "a span of service '{}' needs a `name` or a `resource`: any span of the service would meet it",
                fields.service
            ));
        }

        Ok(Self {
            service: fields.service,
            name: fields.name,
            resource: fields.resource,
        })
    }
}

impl ExpectedSpan {
    /// Returns whether `span` has the service, and the name and the resource that the expectation sets.
    fn is_met_by(&self, span: &Span) -> bool {
        span.service() == self.service
            && self.name.as_deref().is_none_or(|name| span.name() == name)
            && self
                .resource
                .as_deref()
                .is_none_or(|resource| span.resource() == resource)
    }

    /// Writes the expectation as `service=.., name=.., resource=..`, leaving out what it doesn't set.
    fn describe(&self) -> String {
        let mut parts = vec![format!("service={}", self.service)];
        if let Some(name) = &self.name {
            parts.push(format!("name={}", name));
        }
        if let Some(resource) = &self.resource {
            parts.push(format!("resource={}", resource));
        }
        parts.join(", ")
    }

    /// Says what the spans of the expected service hold, to show how close a side came.
    fn describe_service(&self, spans: &HashMap<u64, Span>) -> String {
        let names = spans
            .values()
            .filter(|span| span.service() == self.service)
            .map(Span::name)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return "no span has that service".to_string();
        }

        let distinct = names.iter().copied().collect::<BTreeSet<_>>();
        let listed = distinct.iter().take(MAX_LISTED).copied().collect::<Vec<_>>();
        format!(
            "{} span(s) have that service, with the names {}{}",
            names.len(),
            listed.join(", "),
            if distinct.len() > MAX_LISTED { ", and more" } else { "" }
        )
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

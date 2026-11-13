//! What a metric must look like to count as received by an intake.

use serde::Deserialize;
use stele::MetricValue;

use super::metrics::{MetricType, NormalizedMetric, NormalizedMetricContext, NormalizedMetrics};

/// How the expected tags of a [`MetricCriteria`] must relate to an observed context's normalized tags.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TagMatch {
    /// Every expected tag is present; extra tags are allowed.
    #[default]
    Subset,
    /// The context's tag set equals the expected set after sorting and deduplication.
    Exact,
}

/// The first criterion an observed context with the expected name fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mismatch {
    Tags,
    Type,
    Value,
}

/// A name, tags, and optionally a type and a value, that one normalized metric context must meet by itself.
///
/// Contexts are never combined: a criterion met by one context and another met by a second do not add up.
pub struct MetricCriteria {
    /// Expected name and tags, normalized like an observed context.
    expected: NormalizedMetricContext,
    tags_match: TagMatch,
    metric_type: Option<MetricType>,
    value: Option<f64>,
}

impl MetricCriteria {
    /// Creates the criteria. Expected tags are sorted and deduplicated like observed ones.
    ///
    /// # Errors
    ///
    /// If `value` is combined with a sketch `metric_type`, which no observed metric can match, an error is returned.
    pub fn new(
        name: String, tags: Vec<String>, tags_match: TagMatch, metric_type: Option<MetricType>, value: Option<f64>,
    ) -> Result<Self, String> {
        if value.is_some() && metric_type == Some(MetricType::Sketch) {
            return Err(format!(
                "`{}` can't combine `value` with `metric_type: sketch`: a sketch has no single value to compare",
                name
            ));
        }

        Ok(Self {
            expected: NormalizedMetricContext::new(name, tags),
            tags_match,
            metric_type,
            value,
        })
    }

    /// The expected name.
    pub fn name(&self) -> &str {
        self.expected.name()
    }

    /// Returns whether one context in `metrics` has the expected name and meets every other criterion.
    pub fn is_met_by(&self, metrics: &NormalizedMetrics) -> bool {
        metrics
            .metrics()
            .iter()
            .any(|metric| metric.context().name() == self.expected.name() && self.mismatch(metric).is_none())
    }

    /// Returns the first criterion `metric` fails, checking tags, then type, then value. The name isn't checked.
    pub fn mismatch(&self, metric: &NormalizedMetric) -> Option<Mismatch> {
        let observed_tags = metric.context().tags();
        let tags_hold = match self.tags_match {
            TagMatch::Subset => self.expected.tags().iter().all(|tag| observed_tags.contains(tag)),
            TagMatch::Exact => self.expected.tags() == observed_tags,
        };
        if !tags_hold {
            return Some(Mismatch::Tags);
        }

        if self.metric_type.is_some_and(|expected| expected != metric.value_type()) {
            return Some(Mismatch::Type);
        }

        if self
            .value
            .is_some_and(|expected| !value_matches(metric.normalized_value(), expected))
        {
            return Some(Mismatch::Value);
        }

        None
    }

    /// Explains how `metric` fails `mismatch`.
    pub fn describe_mismatch(&self, mismatch: Mismatch, metric: &NormalizedMetric) -> String {
        match mismatch {
            Mismatch::Tags => {
                let observed_tags = metric.context().tags();
                let missing = self
                    .expected
                    .tags()
                    .iter()
                    .filter(|tag| !observed_tags.contains(tag))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing [{}]", missing.join(", ")));
                }
                if self.tags_match == TagMatch::Exact {
                    let unexpected = observed_tags
                        .iter()
                        .filter(|tag| !self.expected.tags().contains(tag))
                        .cloned()
                        .collect::<Vec<_>>();
                    if !unexpected.is_empty() {
                        parts.push(format!("unexpected [{}]", unexpected.join(", ")));
                    }
                }
                format!("tags: {}", parts.join(", "))
            }
            Mismatch::Type => format!(
                "type: expected {}",
                self.metric_type.map(|t| t.to_string()).unwrap_or_default()
            ),
            Mismatch::Value => format!("value: expected {}", self.value.unwrap_or_default()),
        }
    }

    /// Writes the criteria as `name=..., metric_type=..., value=..., tags=[..], tags_match=...`.
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("name={}", self.expected.name())];
        if let Some(metric_type) = self.metric_type {
            parts.push(format!("metric_type={}", metric_type));
        }
        if let Some(value) = self.value {
            parts.push(format!("value={}", value));
        }
        // An empty exact set still constrains the context, so it is always shown.
        if !self.expected.tags().is_empty() || self.tags_match == TagMatch::Exact {
            parts.push(format!("tags=[{}]", self.expected.tags().join(", ")));
        }
        parts.push(
            match self.tags_match {
                TagMatch::Subset => "tags_match=subset",
                TagMatch::Exact => "tags_match=exact",
            }
            .to_string(),
        );
        parts.join(", ")
    }
}

/// Returns whether `observed` equals `expected`, read as a value of the observed type.
///
/// A rate is compared at its own interval. A sketch has no single value, so it never matches.
fn value_matches(observed: &MetricValue, expected: f64) -> bool {
    // Compare through `MetricValue` so float comparison stays owned by stele.
    match observed {
        MetricValue::Count { .. } => *observed == MetricValue::Count { value: expected },
        MetricValue::Gauge { .. } => *observed == MetricValue::Gauge { value: expected },
        MetricValue::Rate { interval, .. } => {
            *observed
                == MetricValue::Rate {
                    interval: *interval,
                    value: expected,
                }
        }
        MetricValue::Sketch { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use stele::Metric;

    use super::*;
    use crate::steps::intake::fixtures::{count, gauge, metric, rate};

    fn normalized(metrics: &[Metric]) -> NormalizedMetrics {
        NormalizedMetrics::from_stele_metrics(metrics).0
    }

    fn criteria(
        name: &str, metric_type: Option<MetricType>, value: Option<f64>, tags: &[&str], tags_match: TagMatch,
    ) -> MetricCriteria {
        MetricCriteria::new(
            name.to_string(),
            tags.iter().map(|t| t.to_string()).collect(),
            tags_match,
            metric_type,
            value,
        )
        .expect("criteria should build")
    }

    fn subset(name: &str, metric_type: Option<MetricType>, value: Option<f64>, tags: &[&str]) -> MetricCriteria {
        criteria(name, metric_type, value, tags, TagMatch::Subset)
    }

    fn exact(name: &str, tags: &[&str]) -> MetricCriteria {
        criteria(name, None, None, tags, TagMatch::Exact)
    }

    #[test]
    fn name_only_criteria_matches_on_name_alone() {
        let observed = normalized(&[metric("some.counter", &["env:test"], &[count(3.0)])]);

        assert!(subset("some.counter", None, None, &[]).is_met_by(&observed));
        assert!(!subset("other.counter", None, None, &[]).is_met_by(&observed));
    }

    #[test]
    fn type_and_value_must_hold_for_the_same_normalized_context() {
        let observed = normalized(&[
            metric("some.metric", &[], &[count(3.0)]),
            metric("some.metric", &[], &[gauge(7.0)]),
        ]);
        let met = |metric_type, value| subset("some.metric", Some(metric_type), Some(value), &[]).is_met_by(&observed);

        assert!(met(MetricType::Count, 3.0));
        assert!(met(MetricType::Gauge, 7.0));

        // Each criterion holds for one context, but no single context satisfies both.
        assert!(!met(MetricType::Count, 7.0));
        assert!(!met(MetricType::Gauge, 3.0));

        // The same tags with a different type are separate contexts, so their values are never summed.
        assert!(!subset("some.metric", None, Some(10.0), &[]).is_met_by(&observed));
    }

    #[test]
    fn a_value_split_across_flushes_matches_its_total_not_a_partial_sum() {
        let both_flushes = normalized(&[
            metric("some.counter", &[], &[rate(0.3)]),
            metric("some.counter", &[], &[rate(0.3)]),
        ]);
        let first_flush = normalized(&[metric("some.counter", &[], &[rate(0.3)])]);
        let total = subset("some.counter", Some(MetricType::Rate), Some(0.6), &[]);
        let partial = subset("some.counter", Some(MetricType::Rate), Some(0.3), &[]);

        assert!(total.is_met_by(&both_flushes));
        assert!(!partial.is_met_by(&both_flushes));
        assert!(!total.is_met_by(&first_flush));
    }

    #[test]
    fn contexts_that_each_fall_short_are_not_summed() {
        let observed = normalized(&[
            metric("some.counter", &["shard:a"], &[rate(0.3)]),
            metric("some.counter", &["shard:b"], &[rate(0.3)]),
        ]);

        assert!(!subset("some.counter", None, Some(0.6), &[]).is_met_by(&observed));
        assert!(subset("some.counter", None, Some(0.3), &[]).is_met_by(&observed));
    }

    #[test]
    fn subset_and_exact_tag_matching() {
        let tagged = normalized(&[metric("m", &["source:s", "host:h"], &[gauge(1.0)])]);
        let untagged = normalized(&[metric("m", &[], &[gauge(1.0)])]);

        // Subset allows extra tags, including the host tag.
        assert!(subset("m", None, None, &["source:s"]).is_met_by(&tagged));
        assert!(subset("m", None, None, &["host:h"]).is_met_by(&tagged));
        assert!(!subset("m", None, None, &["source:other"]).is_met_by(&tagged));

        // Exact treats the host as a tag like any other.
        assert!(!exact("m", &["source:s"]).is_met_by(&tagged));
        assert!(exact("m", &["host:h", "source:s"]).is_met_by(&tagged));

        // Expected tags are sorted and deduplicated like observed ones.
        assert!(exact("m", &["source:s", "host:h", "source:s"]).is_met_by(&tagged));

        // An empty set matches any context under subset, but only an untagged one under exact.
        assert!(subset("m", None, None, &[]).is_met_by(&tagged));
        assert!(!exact("m", &[]).is_met_by(&tagged));
        assert!(exact("m", &[]).is_met_by(&untagged));
    }

    #[test]
    fn a_value_combined_with_a_sketch_type_is_rejected() {
        let build = |metric_type, value| {
            MetricCriteria::new(
                "some.sketch".to_string(),
                Vec::new(),
                TagMatch::Subset,
                metric_type,
                value,
            )
        };
        let Err(error) = build(Some(MetricType::Sketch), Some(1.0)) else {
            panic!("value with a sketch type should be rejected");
        };
        assert!(
            error.contains("can't combine `value` with `metric_type: sketch`"),
            "unexpected error: {}",
            error
        );

        // Each one alone is valid.
        assert!(build(Some(MetricType::Sketch), None).is_ok());
        assert!(build(None, Some(1.0)).is_ok());
    }
}

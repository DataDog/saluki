use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_json::{json, Value};
use stele::{Metric, MetricValue};
use tracing::{error, info, warn};

use super::expectations::MetricsExpectations;
use super::join_failures;
use super::ratchet::{DiffKind, FieldDiff, KnownDifference, Ratchet};
use crate::steps::intake::metrics::{
    get_formatted_metric_value, MetricType, NormalizedMetric, NormalizedMetricContext, NormalizedMetrics,
};
use crate::steps::intake::{CollectedData, MetricCriteria};

/// A metric context and type pair, used for comparing metrics between different sets.
type ContextTypePair<'a> = (&'a NormalizedMetricContext, MetricType);

/// The path of a series whose type differs between the sides, the only difference a metrics known difference covers.
const TYPE_PATH: &str = "type";

/// The field of the view of a series that a metrics known difference must scope its entry to.
const NAME_FIELD: &str = "name";

/// Options for metrics analysis, set in the `metrics` section of a `compare` step.
///
/// The expectations (`min_series` and `series`) say what each side must hold. The step checks them before it compares
/// the sides, and a known difference never excuses an unmet expectation.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsOptions {
    /// Fewest series that each side holds after the internal-name filter.
    pub min_series: Option<NonZeroUsize>,

    /// Criteria that one series of each side must meet, one entry for each.
    pub series: Vec<MetricCriteria>,

    /// Series whose type is expected to differ between the sides, each tagged with the gap and the issue that explain
    /// it.
    ///
    /// A covered series is left out of the comparison on both sides. An entry that matches nothing fails the analysis,
    /// so that fixing a gap requires removing its entries.
    pub known_differences: Vec<KnownDifference>,
}

impl MetricsOptions {
    /// Checks that the section sets an expectation, and that each known difference names an issue and covers the type
    /// of named series only.
    ///
    /// # Errors
    ///
    /// If the section sets no expectation, or if an entry names no issue, has a path other than `type` or a kind other
    /// than `value`, or is not scoped to series names with `when: {name: [...]}`, an error is returned.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_series.is_none() && self.series.is_empty() {
            return Err("'metrics' sets no expectation: set 'min_series' or 'series'".to_string());
        }
        for entry in &self.known_differences {
            if entry.issue.is_none() {
                return Err(format!("known difference '{}' names no 'issue'", entry.gap));
            }
            if let Some(path) = entry.paths.iter().find(|path| *path != TYPE_PATH) {
                return Err(format!(
                    "known difference '{}' has the path '{}': a metrics known difference covers only the '{}' path",
                    entry.gap, path, TYPE_PATH
                ));
            }
            if entry.kinds.iter().any(|kind| *kind != DiffKind::Value) {
                return Err(format!(
                    "known difference '{}' has a kind other than 'value': a type difference is always a 'value' \
                     difference",
                    entry.gap
                ));
            }
            if entry.when.keys().ne([NAME_FIELD]) {
                return Err(format!(
                    "known difference '{}' must name its series with `when: {{{}: [...]}}`, and nothing else",
                    entry.gap, NAME_FIELD
                ));
            }
        }
        Ok(())
    }

    /// Returns what each side must hold.
    pub fn expectations(&self) -> MetricsExpectations {
        MetricsExpectations {
            min_series: self.min_series,
            series: self.series.clone(),
        }
    }
}

/// Analyzes metrics for correctness.
pub struct MetricsAnalyzer {
    baseline_metrics: NormalizedMetrics,
    comparison_metrics: NormalizedMetrics,
    options: Option<MetricsOptions>,
}

impl MetricsAnalyzer {
    /// Creates a new `MetricsAnalyzer` instance with the given baseline/comparison data.
    ///
    /// When `options` are set, each side must meet their expectations. The sides are compared whether they meet them or
    /// not.
    pub fn new(
        baseline_data: &CollectedData, comparison_data: &CollectedData, options: Option<MetricsOptions>,
    ) -> Result<Self, GenericError> {
        let baseline_metrics = normalize(baseline_data.metrics(), options.is_some())
            .error_context("Failed to normalize baseline metrics.")?;

        let comparison_metrics = normalize(comparison_data.metrics(), options.is_some())
            .error_context("Failed to normalize comparison metrics.")?;

        Ok(Self {
            baseline_metrics,
            comparison_metrics,
            options,
        })
    }

    /// Analyzes the raw metrics from both the baseline and comparison targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details and the full list of mismatches.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        let mut baseline_metrics = self.baseline_metrics;
        let mut comparison_metrics = self.comparison_metrics;

        info!(
            "Analyzing {} unfiltered metrics from baseline target, and {} unfiltered metrics from comparison target.",
            baseline_metrics.len(),
            comparison_metrics.len()
        );

        // Filter out internal telemetry metrics.
        filter_internal_telemetry_metrics(&mut baseline_metrics, &mut comparison_metrics);

        // Each side must hold what the case expects: two sides that hold only internal telemetry are equal once it's
        // filtered out. An unmet expectation does not stop the comparison, so the failure reports both, with the
        // expectations first.
        let unmet = self.options.as_ref().and_then(|options| {
            options
                .expectations()
                .check(&baseline_metrics, &comparison_metrics)
                .err()
        });

        // A known difference takes a series whose type differs out of both sides, so that the other series are still
        // compared by value.
        let known_differences = self
            .options
            .map(|options| options.known_differences)
            .unwrap_or_default();
        let mut ratchet = Ratchet::new(&known_differences);
        remove_known_type_differences(&mut baseline_metrics, &mut comparison_metrics, &mut ratchet);

        // Every difference is reported: an unmet expectation, or a series on one side only, does not stop the
        // comparison of the values of the series that both sides hold. A known difference that matched nothing is
        // stale: its gap is fixed, or the entry is wrong.
        join_failures([
            unmet,
            compare_metric_contexts(&baseline_metrics, &comparison_metrics).err(),
            compare_metric_values(&baseline_metrics, &comparison_metrics).err(),
            stale_failure(ratchet.stale_entries()),
        ])
    }
}

/// Removes from both sides each series whose type differs between them, when a known difference covers it.
///
/// A series has a type difference when each side holds its context with exactly one type, and the two differ. A side
/// that holds the context with another type as well has an unexpected series, which stays and fails the comparison.
/// The difference is at `type`, of kind `value`, and the view that an entry's `when` matches holds the series `name`.
fn remove_known_type_differences(
    baseline_metrics: &mut NormalizedMetrics, comparison_metrics: &mut NormalizedMetrics, ratchet: &mut Ratchet<'_>,
) {
    let (baseline_only_pairs, comparison_only_pairs) = context_differences(baseline_metrics, comparison_metrics);
    let is_single_type = |metrics: &NormalizedMetrics, context: &NormalizedMetricContext| {
        metrics
            .metrics()
            .iter()
            .filter(|metric| metric.context() == context)
            .map(|metric| metric.value_type())
            .collect::<HashSet<_>>()
            .len()
            == 1
    };
    let mut known = HashSet::new();
    for (context, baseline_type) in &baseline_only_pairs {
        if !is_single_type(baseline_metrics, context) || !is_single_type(comparison_metrics, context) {
            continue;
        }
        let view = BTreeMap::from([(NAME_FIELD.to_string(), json!(context.name()))]);
        for (_, comparison_type) in comparison_only_pairs.iter().filter(|(other, _)| other == context) {
            let diff = FieldDiff {
                path: TYPE_PATH.to_string(),
                kind: DiffKind::Value,
                baseline: Some(Value::String(baseline_type.to_string())),
                comparison: Some(Value::String(comparison_type.to_string())),
            };
            if ratchet.allows(&diff, &view) {
                info!(
                    "Known difference: '{}' is a {} in the baseline and a {} in the comparison.",
                    context, baseline_type, comparison_type
                );
                known.insert((*context).clone());
            }
        }
    }

    baseline_metrics.remove_matching(|metric| known.contains(metric.context()));
    comparison_metrics.remove_matching(|metric| known.contains(metric.context()));
}

/// Returns a failure that lists the `stale` known differences, if there are any.
fn stale_failure(stale: Vec<String>) -> Option<(GenericError, Vec<String>)> {
    if stale.is_empty() {
        return None;
    }
    for line in &stale {
        error!("{}", line);
    }
    Some((
        generic_error!("{} known difference path(s) no longer occur.", stale.len()),
        stale,
    ))
}

const SAMPLE_MISMATCH_LIMIT: usize = 5;

/// Normalizes the metrics of one side.
///
/// A side that holds no metrics is an error, unless the case sets expectations: they report what the side lacks.
fn normalize(metrics: &[Metric], has_expectations: bool) -> Result<NormalizedMetrics, GenericError> {
    if metrics.is_empty() && has_expectations {
        return Ok(NormalizedMetrics::from_stele_metrics(metrics).0);
    }
    NormalizedMetrics::try_from_stele_metrics(metrics)
}

/// Returns the differences, if any, between two sets of normalized metrics.
///
/// Compares both context and type, so metrics with the same name but different types
/// (for example, Count vs Rate) are treated as different metrics.
///
/// Returns two vectors, the first containing (context, type) pairs that are only present in the left set,
/// and the second containing (context, type) pairs that are only present in the right set. Both vectors are
/// sorted by context, and then type, in ascending order.
/// If there are no differences, both vectors will be empty.
fn context_differences<'a>(
    left: &'a NormalizedMetrics, right: &'a NormalizedMetrics,
) -> (Vec<ContextTypePair<'a>>, Vec<ContextTypePair<'a>>) {
    let left_pairs = left
        .metrics()
        .iter()
        .map(|m| (m.context(), m.value_type()))
        .collect::<HashSet<_>>();
    let right_pairs = right
        .metrics()
        .iter()
        .map(|m| (m.context(), m.value_type()))
        .collect::<HashSet<_>>();

    let mut left_only = left_pairs.difference(&right_pairs).copied().collect::<Vec<_>>();
    let mut right_only = right_pairs.difference(&left_pairs).copied().collect::<Vec<_>>();
    left_only.sort_unstable();
    right_only.sort_unstable();

    (left_only, right_only)
}

/// Compares the unique set of (context, type) pairs emitted by the baseline and comparison targets.
///
/// # Errors
///
/// If either target emitted a pair the other one didn't, an error is returned along with details listing every
/// mismatched pair, grouped into a baseline-only section and a comparison-only section.
fn compare_metric_contexts(
    baseline_metrics: &NormalizedMetrics, comparison_metrics: &NormalizedMetrics,
) -> Result<(), (GenericError, Vec<String>)> {
    let (baseline_only_pairs, comparison_only_pairs) = context_differences(baseline_metrics, comparison_metrics);

    if baseline_only_pairs.is_empty() && comparison_only_pairs.is_empty() {
        return Ok(());
    }

    // The details are the only machine-readable record of _which_ metrics differed, so they carry both directions in
    // full, even when the inline copy in `result.json` ends up capped.
    let mut details = Vec::with_capacity(baseline_only_pairs.len() + comparison_only_pairs.len() + 3);
    details.push(format!(
        "Mismatch in metrics pairs: {} only in baseline, {} only in comparison.",
        baseline_only_pairs.len(),
        comparison_only_pairs.len()
    ));

    details.push(format!(
        "Metrics in baseline but not in comparison ({}):",
        baseline_only_pairs.len()
    ));
    for (context, metric_type) in baseline_only_pairs {
        details.push(format!("  - {} (type: {})", context, metric_type));
    }

    details.push(format!(
        "Metrics in comparison but not in baseline ({}):",
        comparison_only_pairs.len()
    ));
    for (context, metric_type) in comparison_only_pairs {
        details.push(format!("  - {} (type: {})", context, metric_type));
    }

    error!("Mismatch in unique metrics between baseline and comparison!");
    for detail in &details {
        error!("{}", detail);
    }

    Err((
        generic_error!("Mismatch in metrics pairs between baseline and comparison."),
        details,
    ))
}

fn compare_metric_values(
    baseline_metrics: &NormalizedMetrics, comparison_metrics: &NormalizedMetrics,
) -> Result<(), (GenericError, Vec<String>)> {
    let mut mismatched_count = 0;
    let mut samples: Vec<String> = Vec::new();
    let mut all_details: Vec<String> = Vec::new();

    // Only the series that both sides hold are compared by value. `compare_metric_contexts` reports the others.
    let comparison_by_pair = comparison_metrics
        .metrics()
        .iter()
        .map(|metric| ((metric.context(), metric.value_type()), metric))
        .collect::<HashMap<_, _>>();
    for baseline_metric in baseline_metrics.metrics() {
        let Some(comparison_metric) =
            comparison_by_pair.get(&(baseline_metric.context(), baseline_metric.value_type()))
        else {
            continue;
        };
        let baseline_value = baseline_metric.normalized_value();
        let comparison_value = comparison_metric.normalized_value();

        if baseline_value != comparison_value {
            mismatched_count += 1;

            error!("Found mismatched metric '{}':", baseline_metric.context());
            warn!(
                "  Baseline: {}",
                get_formatted_metric_values(baseline_metric, baseline_value)
            );
            warn!(
                "  Comparison: {}",
                get_formatted_metric_values(comparison_metric, comparison_value)
            );

            let detail = format!(
                "  {}\n    baseline:    {}\n    comparison:  {}",
                baseline_metric.context(),
                get_formatted_metric_values(baseline_metric, baseline_value),
                get_formatted_metric_values(comparison_metric, comparison_value),
            );
            all_details.push(detail.clone());
            if samples.len() < SAMPLE_MISMATCH_LIMIT {
                samples.push(detail);
            }
        }
    }

    if mismatched_count == 0 {
        Ok(())
    } else {
        let mut msg = format!(
            "{} metrics from baseline and comparison did not match.",
            mismatched_count
        );
        msg.push_str(&format!("\n  (showing {} of {})", samples.len(), mismatched_count));
        for sample in samples {
            msg.push('\n');
            msg.push_str(&sample);
        }
        Err((generic_error!("{}", msg), all_details))
    }
}

fn filter_internal_telemetry_metrics(
    baseline_metrics: &mut NormalizedMetrics, comparison_metrics: &mut NormalizedMetrics,
) {
    let baseline_filtered_metrics = baseline_metrics.remove_matching(is_internal_telemetry);
    let comparison_filtered_metrics = comparison_metrics.remove_matching(is_internal_telemetry);

    info!(
        "Filtered {} internal telemetry metric(s) from baseline, and {} internal telemetry metric(s) from comparison.",
        baseline_filtered_metrics.len(),
        comparison_filtered_metrics.len()
    );
}

fn is_internal_telemetry(metric: &NormalizedMetric) -> bool {
    let name = metric.context().name();
    name.starts_with("datadog.")
        || name.starts_with("n_o_i_n_d_e_x")
        || name.starts_with("system.")
        || name.starts_with("docker.")
        || name.starts_with("container.")
        || name == "ntp.offset"
}

fn get_formatted_metric_values(metric: &NormalizedMetric, value: &MetricValue) -> String {
    let collapsed_value = get_formatted_metric_value(value);

    let mut raw_values = Vec::new();
    for (ts, raw_value) in metric.raw_values() {
        raw_values.push(format!("({} => {})", ts, get_formatted_metric_value(raw_value)));
    }

    format!("{} (raw: {})", collapsed_value, raw_values.join(", "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use stele::Metric;

    use super::{compare_metric_contexts, MetricsAnalyzer, MetricsOptions};
    use crate::steps::intake::metrics::NormalizedMetrics;

    fn metrics(entries: &[(&str, &[&str], &str)]) -> NormalizedMetrics {
        valued(
            &entries
                .iter()
                .map(|(name, tags, mtype)| (*name, *tags, *mtype, 1.0))
                .collect::<Vec<_>>(),
        )
    }

    fn valued(entries: &[(&str, &[&str], &str, f64)]) -> NormalizedMetrics {
        let metrics = entries
            .iter()
            .map(|(name, tags, mtype, value)| {
                serde_json::from_value::<Metric>(json!({
                    "context": {
                        "name": name,
                        "tags": tags,
                    },
                    "values": [[10, {"mtype": mtype, "value": value, "interval": 10}]],
                }))
                .expect("metric should deserialize")
            })
            .collect::<Vec<_>>();

        NormalizedMetrics::try_from_stele_metrics(&metrics).expect("metrics should normalize")
    }

    /// The `metrics` section of a case that expects `known.series` to be a count in the baseline and a gauge in the
    /// comparison.
    const KNOWN_TYPE_DIFFERENCE: &str = "{min_series: 1, known_differences: [{gap: gap/as-gauge, issue: 1, \
        paths: [type], when: {name: [known.series]}, reason: a gauge in the comparison}]}";

    fn analyze(
        baseline: NormalizedMetrics, comparison: NormalizedMetrics, options: &str,
    ) -> Result<(), (String, Vec<String>)> {
        let options = serde_saphyr::from_str::<MetricsOptions>(options).expect("options should parse");
        options.validate().expect("options should be valid");
        MetricsAnalyzer {
            baseline_metrics: baseline,
            comparison_metrics: comparison,
            options: Some(options),
        }
        .run_analysis()
        .map_err(|(error, details)| (error.to_string(), details))
    }

    /// The baseline of the cases below: the series with the known difference as a count, and a second series.
    fn baseline() -> NormalizedMetrics {
        valued(&[
            ("known.series", &[], "Count", 30.0),
            ("value.series", &[], "Count", 15.0),
        ])
    }

    #[test]
    fn a_known_type_difference_leaves_the_other_series_compared() {
        let matching = valued(&[
            ("known.series", &[], "Gauge", 3.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        assert_eq!(analyze(baseline(), matching, KNOWN_TYPE_DIFFERENCE), Ok(()));

        // The second series changes, and the known difference of the first one does not hide it.
        let regressed = valued(&[
            ("known.series", &[], "Gauge", 3.0),
            ("value.series", &[], "Count", 16.0),
        ]);
        let (error, details) = analyze(baseline(), regressed, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert!(
            error.contains("1 metrics from baseline and comparison did not match"),
            "{}",
            error
        );
        assert_eq!(details.len(), 1, "{:?}", details);
        assert!(details[0].contains("value.series"), "{}", details[0]);
        assert!(details[0].contains("baseline:    count(15)"), "{}", details[0]);
        assert!(details[0].contains("comparison:  count(16)"), "{}", details[0]);
    }

    #[test]
    fn a_known_type_difference_covers_only_the_named_series() {
        let comparison = valued(&[
            ("known.series", &[], "Gauge", 3.0),
            ("value.series", &[], "Gauge", 15.0),
        ]);

        let (error, details) = analyze(baseline(), comparison, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert_eq!(error, "Mismatch in metrics pairs between baseline and comparison.");
        assert!(
            details.contains(&"  - value.series (type: count)".to_string()),
            "{:?}",
            details
        );
        assert!(
            !details.iter().any(|line| line.contains("known.series")),
            "{:?}",
            details
        );
    }

    #[test]
    fn a_known_type_difference_keeps_an_unexpected_type_of_the_series() {
        // The comparison also holds a rate of the series, and the known difference does not hide it.
        let extra_in_comparison = valued(&[
            ("known.series", &[], "Gauge", 3.0),
            ("known.series", &[], "Rate", 3.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        let (error, details) = analyze(baseline(), extra_in_comparison, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert!(
            error.starts_with("Mismatch in metrics pairs between baseline and comparison."),
            "{}",
            error
        );
        assert!(
            details.contains(&"  - known.series (type: rate)".to_string()),
            "{:?}",
            details
        );

        // The same holds when the baseline holds the extra type.
        let extra_in_baseline = valued(&[
            ("known.series", &[], "Count", 30.0),
            ("known.series", &[], "Rate", 30.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        let comparison = valued(&[
            ("known.series", &[], "Gauge", 3.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        let (error, details) = analyze(extra_in_baseline, comparison, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert!(
            error.starts_with("Mismatch in metrics pairs between baseline and comparison."),
            "{}",
            error
        );
        assert!(
            details.contains(&"  - known.series (type: rate)".to_string()),
            "{:?}",
            details
        );
    }

    #[test]
    fn a_known_type_difference_that_no_longer_occurs_is_stale() {
        let fixed = valued(&[
            ("known.series", &[], "Count", 30.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        let (error, details) = analyze(baseline(), fixed, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert_eq!(error, "1 known difference path(s) no longer occur.");
        assert!(details[0].contains("gap/as-gauge (#1)"), "{:?}", details);

        // A series that one side lost is not a type difference: the entry is stale, and the loss still fails.
        let lost = valued(&[("value.series", &[], "Count", 15.0)]);
        let (error, details) = analyze(baseline(), lost, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert_eq!(
            error,
            "Mismatch in metrics pairs between baseline and comparison.\n1 known difference path(s) no longer occur."
        );
        assert!(
            details.contains(&"  - known.series (type: count)".to_string()),
            "{:?}",
            details
        );
        assert!(
            details.iter().any(|line| line.contains("gap/as-gauge (#1)")),
            "{:?}",
            details
        );
    }

    #[test]
    fn a_series_on_one_side_only_leaves_the_other_series_compared() {
        let baseline = valued(&[
            ("known.series", &[], "Count", 30.0),
            ("lost.series", &[], "Count", 1.0),
            ("value.series", &[], "Count", 15.0),
        ]);
        let comparison = valued(&[
            ("extra.series", &[], "Count", 1.0),
            ("known.series", &[], "Gauge", 3.0),
            ("value.series", &[], "Count", 16.0),
        ]);

        let (error, details) = analyze(baseline, comparison, KNOWN_TYPE_DIFFERENCE).unwrap_err();
        assert!(
            error.starts_with(
                "Mismatch in metrics pairs between baseline and comparison.\n1 metrics from baseline and comparison \
                 did not match."
            ),
            "{}",
            error
        );
        for expected in ["  - lost.series (type: count)", "  - extra.series (type: count)"] {
            assert!(details.contains(&expected.to_string()), "{:?}", details);
        }
        assert!(
            details
                .iter()
                .any(|line| line.contains("value.series") && line.contains("comparison:  count(16)")),
            "{:?}",
            details
        );
        assert!(
            !details.iter().any(|line| line.contains("known.series")),
            "{:?}",
            details
        );
    }

    #[test]
    fn metrics_known_differences_name_an_issue_and_a_series() {
        let validate = |entry: &str| {
            let options = format!("{{min_series: 1, known_differences: [{}]}}", entry);
            serde_saphyr::from_str::<MetricsOptions>(&options)
                .expect("options should parse")
                .validate()
        };

        assert!(validate("{gap: g, issue: 1, paths: [type], when: {name: [m]}, reason: r}").is_ok());
        assert!(validate("{gap: g, issue: 1, paths: [type], kinds: [value], when: {name: [m]}, reason: r}").is_ok());
        let cases = [
            (
                "{gap: g, paths: [type], when: {name: [m]}, reason: r}",
                "names no 'issue'",
            ),
            (
                "{gap: g, issue: 1, paths: [value], when: {name: [m]}, reason: r}",
                "covers only the 'type' path",
            ),
            (
                "{gap: g, issue: 1, paths: [type], kinds: [missing], when: {name: [m]}, reason: r}",
                "a kind other than 'value'",
            ),
            ("{gap: g, issue: 1, paths: [type], reason: r}", "when: {name: [...]}"),
            (
                "{gap: g, issue: 1, paths: [type], when: {name: [m], tags: [t]}, reason: r}",
                "when: {name: [...]}",
            ),
        ];
        for (entry, expected) in cases {
            let error = validate(entry).unwrap_err();
            assert!(error.contains(expected), "{}: {}", entry, error);
        }
    }

    #[test]
    fn context_mismatch_details_list_every_pair_in_both_directions() {
        let baseline = metrics(&[
            ("type.flip", &["env:prod"], "Count"),
            ("shared.metric", &["env:prod"], "Count"),
            ("only.in.baseline", &["env:prod"], "Count"),
        ]);
        let comparison = metrics(&[
            ("only.in.comparison", &["env:prod"], "Count"),
            ("shared.metric", &["env:prod"], "Count"),
            ("type.flip", &["env:prod"], "Gauge"),
        ]);

        let (error, details) =
            compare_metric_contexts(&baseline, &comparison).expect_err("context mismatch should be an error");

        assert_eq!(
            error.to_string(),
            "Mismatch in metrics pairs between baseline and comparison."
        );
        assert_eq!(
            details,
            vec![
                "Mismatch in metrics pairs: 2 only in baseline, 2 only in comparison.",
                "Metrics in baseline but not in comparison (2):",
                "  - only.in.baseline[env:prod] (type: count)",
                "  - type.flip[env:prod] (type: count)",
                "Metrics in comparison but not in baseline (2):",
                "  - only.in.comparison[env:prod] (type: count)",
                "  - type.flip[env:prod] (type: gauge)",
            ]
        );
    }

    #[test]
    fn matching_contexts_yield_no_mismatch() {
        let baseline = metrics(&[("shared.metric", &["env:prod"], "Count")]);
        let comparison = metrics(&[("shared.metric", &["env:prod"], "Count")]);

        assert!(compare_metric_contexts(&baseline, &comparison).is_ok());
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::Value;

/// A pair of Core Agent `/debug/vars` snapshots surrounding the test workload.
pub struct ExpvarSnapshots {
    pub before: Value,
    pub after: Value,
}

#[derive(Clone, Copy)]
enum NumericValue {
    Integer(i128),
    Float(f64),
}

impl PartialEq for NumericValue {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Integer(value), Self::Integer(other)) => value == other,
            (Self::Float(value), Self::Float(other)) => value == other,
            (Self::Integer(value), Self::Float(other)) | (Self::Float(other), Self::Integer(value)) => {
                other.is_finite() && other.fract() == 0.0 && other as i128 == value && value as f64 == other
            }
        }
    }
}

impl NumericValue {
    fn subtract(self, other: Self) -> Self {
        match (self, other) {
            (Self::Integer(value), Self::Integer(other)) => Self::Integer(value - other),
            (value, other) => Self::Float(value.as_f64() - other.as_f64()),
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Float(value) => value,
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Self::Integer(value) => value == 0,
            Self::Float(value) => value == 0.0,
        }
    }
}

#[derive(Clone, Copy)]
enum Observation {
    Missing,
    Number(NumericValue),
    Other,
}

#[derive(Clone, Copy)]
struct Activity {
    delta: Option<NumericValue>,
    appeared: bool,
    disappeared: bool,
    shape_mismatch: bool,
}

impl Activity {
    fn between(before: Observation, after: Observation) -> Self {
        match (before, after) {
            (Observation::Number(before), Observation::Number(after)) => Self {
                delta: Some(after.subtract(before)),
                appeared: false,
                disappeared: false,
                shape_mismatch: false,
            },
            (Observation::Missing, Observation::Number(after)) => Self {
                delta: Some(after),
                appeared: true,
                disappeared: false,
                shape_mismatch: false,
            },
            (Observation::Number(_), Observation::Missing) => Self {
                delta: None,
                appeared: false,
                disappeared: true,
                shape_mismatch: false,
            },
            (Observation::Number(_), Observation::Other) | (Observation::Other, Observation::Number(_)) => Self {
                delta: None,
                appeared: false,
                disappeared: false,
                shape_mismatch: true,
            },
            _ => Self {
                delta: Some(NumericValue::Integer(0)),
                appeared: false,
                disappeared: false,
                shape_mismatch: false,
            },
        }
    }

    fn is_active(self) -> bool {
        self.delta.is_some_and(|delta| !delta.is_zero()) || self.appeared || self.disappeared || self.shape_mismatch
    }
}

struct PathComparison {
    path: String,
    baseline_before: Observation,
    baseline_after: Observation,
    baseline_activity: Activity,
    comparison_before: Observation,
    comparison_after: Observation,
    comparison_activity: Activity,
    classification: &'static str,
    matches: bool,
}

/// Comparison of every numeric expvar path observed across two targets.
pub struct ExpvarComparisonReport {
    rows: Vec<PathComparison>,
}

impl ExpvarComparisonReport {
    /// Returns whether all observed numeric expvar paths have matching workload deltas.
    pub fn matches(&self) -> bool {
        self.rows.iter().all(|row| row.matches)
    }

    /// Returns the number of paths that were numeric in at least one snapshot.
    pub fn total_numeric_paths(&self) -> usize {
        self.rows.len()
    }

    /// Returns the number of paths whose value or shape changed during the workload.
    pub fn active_paths(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.baseline_activity.is_active() || row.comparison_activity.is_active())
            .count()
    }

    /// Renders a deterministic table of active and incompatible paths.
    pub fn details(&self) -> String {
        let mut output = String::from(
            "path | baseline before | baseline after | baseline delta | comparison before | comparison after | comparison delta | classification\n",
        );

        for row in self
            .rows
            .iter()
            .filter(|row| row.baseline_activity.is_active() || row.comparison_activity.is_active() || !row.matches)
        {
            let _ = writeln!(
                output,
                "{} | {} | {} | {} | {} | {} | {} | {}",
                row.path,
                format_observation(row.baseline_before),
                format_observation(row.baseline_after),
                format_delta(row.baseline_activity.delta),
                format_observation(row.comparison_before),
                format_observation(row.comparison_after),
                format_delta(row.comparison_activity.delta),
                row.classification,
            );
        }

        output
    }

    /// Returns a compact count of paths in each classification.
    pub fn summary(&self) -> String {
        let mut counts = BTreeMap::<&str, usize>::new();
        for row in &self.rows {
            *counts.entry(row.classification).or_default() += 1;
        }

        let classifications = counts
            .into_iter()
            .map(|(classification, count)| format!("{classification}={count}"))
            .collect::<Vec<_>>()
            .join(", ");

        format!(
            "Compared {} numeric expvar paths; {} changed during the workload. {}",
            self.total_numeric_paths(),
            self.active_paths(),
            classifications
        )
    }
}

/// Compares all numeric JSON leaves using each target's before/after delta.
pub fn compare_snapshots(baseline: &ExpvarSnapshots, comparison: &ExpvarSnapshots) -> ExpvarComparisonReport {
    let baseline_before = flatten_leaves(&baseline.before);
    let baseline_after = flatten_leaves(&baseline.after);
    let comparison_before = flatten_leaves(&comparison.before);
    let comparison_after = flatten_leaves(&comparison.after);

    let paths = baseline_before
        .keys()
        .chain(baseline_after.keys())
        .chain(comparison_before.keys())
        .chain(comparison_after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    let rows = paths
        .into_iter()
        .filter_map(|path| {
            let baseline_before_value = observe(&baseline_before, &path);
            let baseline_after_value = observe(&baseline_after, &path);
            let comparison_before_value = observe(&comparison_before, &path);
            let comparison_after_value = observe(&comparison_after, &path);

            if ![
                baseline_before_value,
                baseline_after_value,
                comparison_before_value,
                comparison_after_value,
            ]
            .iter()
            .any(|value| matches!(value, Observation::Number(_)))
            {
                return None;
            }

            let baseline_activity = Activity::between(baseline_before_value, baseline_after_value);
            let comparison_activity = Activity::between(comparison_before_value, comparison_after_value);
            let (classification, matches) = classify(baseline_activity, comparison_activity);

            Some(PathComparison {
                path,
                baseline_before: baseline_before_value,
                baseline_after: baseline_after_value,
                baseline_activity,
                comparison_before: comparison_before_value,
                comparison_after: comparison_after_value,
                comparison_activity,
                classification,
                matches,
            })
        })
        .collect();

    ExpvarComparisonReport { rows }
}

fn classify(baseline: Activity, comparison: Activity) -> (&'static str, bool) {
    if baseline.shape_mismatch || comparison.shape_mismatch {
        return ("shape_mismatch", false);
    }
    if baseline.disappeared || comparison.disappeared {
        let matches = baseline.disappeared == comparison.disappeared;
        return (if matches { "disappeared_equally" } else { "disappeared" }, matches);
    }
    if baseline.appeared || comparison.appeared {
        let matches = baseline.appeared == comparison.appeared && baseline.delta == comparison.delta;
        return (
            if matches {
                "appeared_equally"
            } else {
                "appeared_unequally"
            },
            matches,
        );
    }

    let baseline = baseline.delta.unwrap_or(NumericValue::Integer(0));
    let comparison = comparison.delta.unwrap_or(NumericValue::Integer(0));
    if baseline == comparison {
        ("equal_activity", true)
    } else if !baseline.is_zero() && comparison.is_zero() {
        ("baseline_only_activity", false)
    } else if baseline.is_zero() && !comparison.is_zero() {
        ("adp_only_activity", false)
    } else {
        ("unequal_activity", false)
    }
}

fn observe(values: &BTreeMap<String, Observation>, path: &str) -> Observation {
    values.get(path).copied().unwrap_or(Observation::Missing)
}

fn flatten_leaves(value: &Value) -> BTreeMap<String, Observation> {
    let mut values = BTreeMap::new();
    flatten_value(value, "", &mut values);
    values
}

fn flatten_value(value: &Value, path: &str, values: &mut BTreeMap<String, Observation>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                flatten_value(value, &format!("{path}/{escaped}"), values);
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                flatten_value(value, &format!("{path}/{index}"), values);
            }
        }
        Value::Number(number) => {
            let number = if let Some(value) = number.as_i64() {
                NumericValue::Integer(i128::from(value))
            } else if let Some(value) = number.as_u64() {
                NumericValue::Integer(i128::from(value))
            } else if let Some(value) = number.as_f64() {
                NumericValue::Float(value)
            } else {
                values.insert(path.to_string(), Observation::Other);
                return;
            };
            values.insert(path.to_string(), Observation::Number(number));
        }
        _ => {
            values.insert(path.to_string(), Observation::Other);
        }
    }
}

fn format_observation(observation: Observation) -> String {
    match observation {
        Observation::Missing => "<missing>".to_string(),
        Observation::Number(value) => format_number(value),
        Observation::Other => "<non-numeric>".to_string(),
    }
}

fn format_delta(delta: Option<NumericValue>) -> String {
    delta.map(format_number).unwrap_or_else(|| "<n/a>".to_string())
}

fn format_number(value: NumericValue) -> String {
    match value {
        NumericValue::Integer(value) => value.to_string(),
        NumericValue::Float(value) if value.fract() == 0.0 => format!("{value:.0}"),
        NumericValue::Float(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{compare_snapshots, ExpvarSnapshots};
    use crate::correctness::analysis::AnalysisRunner;

    fn snapshots(before: Value, after: Value) -> ExpvarSnapshots {
        ExpvarSnapshots { before, after }
    }

    #[test]
    fn compares_before_after_deltas_instead_of_absolute_values() {
        let baseline = snapshots(
            json!({"dogstatsd": {"MetricPackets": 10}}),
            json!({"dogstatsd": {"MetricPackets": 25}}),
        );
        let comparison = snapshots(
            json!({"dogstatsd": {"MetricPackets": 100}}),
            json!({"dogstatsd": {"MetricPackets": 115}}),
        );

        let report = compare_snapshots(&baseline, &comparison);

        assert!(report.matches());
        assert_eq!(report.total_numeric_paths(), 1);
        assert_eq!(report.active_paths(), 1);
        assert!(report.summary().contains("equal_activity=1"));
    }

    #[test]
    fn reports_every_changed_numeric_path_in_stable_order() {
        let baseline = snapshots(
            json!({
                "dogstatsd": {"MetricPackets": 1},
                "dogstatsd-udp": {"Packets": 2},
                "unchanged": 9,
            }),
            json!({
                "dogstatsd": {"MetricPackets": 11},
                "dogstatsd-udp": {"Packets": 22},
                "unchanged": 9,
            }),
        );
        let comparison = snapshots(
            json!({
                "dogstatsd": {"MetricPackets": 1},
                "dogstatsd-udp": {"Packets": 2},
                "unchanged": 9,
            }),
            json!({
                "dogstatsd": {"MetricPackets": 1},
                "dogstatsd-udp": {"Packets": 7},
                "unchanged": 9,
            }),
        );

        let report = compare_snapshots(&baseline, &comparison);
        let details = report.details();

        assert!(!report.matches());
        assert_eq!(report.total_numeric_paths(), 3);
        assert_eq!(report.active_paths(), 2);
        assert!(details.contains("/dogstatsd/MetricPackets | 1 | 11 | 10 | 1 | 1 | 0 | baseline_only_activity"));
        assert!(details.contains("/dogstatsd-udp/Packets | 2 | 22 | 20 | 2 | 7 | 5 | unequal_activity"));
        assert!(!details.contains("/unchanged"));
        assert!(
            details.find("/dogstatsd-udp/Packets").unwrap() < details.find("/dogstatsd/MetricPackets").unwrap(),
            "paths should be sorted lexicographically"
        );
    }

    #[test]
    fn traverses_arrays_and_escapes_json_pointer_paths() {
        let baseline = snapshots(
            json!({"a/b": [{"~count": 1}], "ignored": "1"}),
            json!({"a/b": [{"~count": 4}], "ignored": "9"}),
        );
        let comparison = snapshots(
            json!({"a/b": [{"~count": 1}], "ignored": true}),
            json!({"a/b": [{"~count": 4}], "ignored": false}),
        );

        let report = compare_snapshots(&baseline, &comparison);

        assert!(report.matches());
        assert_eq!(report.total_numeric_paths(), 1);
        assert!(report.details().contains("/a~1b/0/~0count"));
        assert!(!report.details().contains("ignored"));
    }

    #[test]
    fn compares_equal_integer_and_floating_point_deltas_numerically() {
        let baseline = snapshots(json!({"value": 0}), json!({"value": 1}));
        let comparison = snapshots(json!({"value": 0.5}), json!({"value": 1.5}));

        let report = compare_snapshots(&baseline, &comparison);

        assert!(report.matches());
        assert!(report.details().contains("equal_activity"));
    }

    #[test]
    fn preserves_integer_deltas_larger_than_f64_can_represent_exactly() {
        let baseline = snapshots(
            json!({"timestamp": 1_785_359_179_812_154_112_u64}),
            json!({"timestamp": 1_785_359_179_812_154_113_u64}),
        );
        let comparison = snapshots(
            json!({"timestamp": 1_785_359_179_812_154_112_u64}),
            json!({"timestamp": 1_785_359_179_812_154_114_u64}),
        );

        let report = compare_snapshots(&baseline, &comparison);

        assert!(!report.matches());
        assert!(report.details().contains(
            "/timestamp | 1785359179812154112 | 1785359179812154113 | 1 | 1785359179812154112 | 1785359179812154114 | 2 | unequal_activity"
        ));
    }

    #[test]
    fn treats_a_numeric_path_that_appears_after_load_as_starting_at_zero() {
        let baseline = snapshots(json!({}), json!({"lazy": {"counter": 8}}));
        let comparison = snapshots(json!({}), json!({"lazy": {"counter": 3}}));

        let report = compare_snapshots(&baseline, &comparison);

        assert!(!report.matches());
        assert!(report
            .details()
            .contains("/lazy/counter | <missing> | 8 | 8 | <missing> | 3 | 3 | appeared_unequally"));
    }

    #[test]
    fn analysis_runner_returns_the_expvar_inventory_as_failure_details() {
        let baseline = snapshots(json!({"counter": 0}), json!({"counter": 5}));
        let comparison = snapshots(json!({"counter": 0}), json!({"counter": 0}));

        let (error, details) = AnalysisRunner::new_expvars(baseline, comparison)
            .run_analysis()
            .expect_err("the mismatched expvar delta should fail analysis");

        assert!(error.to_string().contains("numeric expvar paths"));
        assert_eq!(details.len(), 1);
        assert!(details[0].contains("/counter"));
        assert!(details[0].contains("baseline_only_activity"));
    }

    #[test]
    fn reports_numeric_shape_changes_without_coercion() {
        let baseline = snapshots(json!({"value": 1}), json!({"value": "gone"}));
        let comparison = snapshots(json!({"value": 1}), json!({"value": 2}));

        let report = compare_snapshots(&baseline, &comparison);

        assert!(!report.matches());
        assert!(report.details().contains("/value"));
        assert!(report.details().contains("shape_mismatch"));
    }
}

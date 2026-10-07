//! The shared data model: observations, series views, anomalies, and log context.
//!
//! These types cross the boundaries between the engine, the store, the detectors, the extractors, and the
//! scorer, so they are kept plain and allocation-light. They mirror the Agent's Go
//! `observer/def/types.go`; where the Go field is a pointer (`*float64`, `*QueryHandle`,
//! `*AnomalyDebugInfo`) the Rust field is an `Option`, preserving the null-versus-zero distinction.

use std::collections::BTreeMap;

use crate::identity::{NamespaceId, QueryHandle, SeriesRef};

/// The source type of an anomaly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnomalyType {
    /// A metric-based anomaly produced by a detector.
    Metric,
    /// A log-based anomaly emitted directly by a log observer, bypassing metric extraction.
    Log,
}

impl AnomalyType {
    /// Returns the wire label used by the Go model (`metric` or `log`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metric => "metric",
            Self::Log => "log",
        }
    }
}

/// A single observed metric sample, before it is stored.
///
/// The host is carried separately from the tags. An unset host (`None`) is distinct from an explicitly
/// empty host (`Some(String::new())`).
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSample {
    /// Metric name.
    pub name: String,
    /// Sample value.
    pub value: f64,
    /// Host dimension, separate from tags. `None` means unset.
    pub host: Option<String>,
    /// The sample's tags, exactly as resolved by the pipeline.
    pub tags: Vec<String>,
    /// Sample timestamp, in whole Unix seconds.
    pub timestamp_sec: i64,
    /// The data stream the sample came from (for example `parquet` during offline replay).
    pub source: String,
}

/// A single observed log line.
///
/// Unlike metric samples, a log's hostname is a plain string: logs have no host-versus-tag lifting step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogObservation {
    /// The raw log message.
    pub message: String,
    /// The log status/level, as recorded.
    pub status: String,
    /// The log's tags.
    pub tags: Vec<String>,
    /// The hostname the log was attributed to.
    pub hostname: String,
    /// Agent ingestion timestamp, in Unix milliseconds.
    pub timestamp_ms: i64,
    /// The data stream the log came from.
    pub source: String,
}

/// A single read-time point of a series: a bucket second and its aggregated value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// The bucket second this point represents.
    pub second: i64,
    /// The value after applying the requested aggregate.
    pub value: f64,
}

/// A materialized view of a series and its points, as returned by a [`crate::traits::StorageView`].
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    /// Namespace of the producing component.
    pub namespace: NamespaceId,
    /// Base metric name.
    pub name: String,
    /// Host dimension, separate from tags.
    pub host: Option<String>,
    /// Series-level tags.
    pub tags: Vec<String>,
    /// The points in the requested range.
    pub points: Vec<Point>,
}

/// Compact metadata describing a live series, without its points.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SeriesMeta {
    /// The storage ref of the series.
    pub series_ref: SeriesRef,
    /// Namespace of the producing component.
    pub namespace: NamespaceId,
    /// Base metric name.
    pub name: String,
    /// Host dimension, separate from tags.
    pub host: Option<String>,
    /// Series-level tags.
    pub tags: Vec<String>,
}

/// Describes the origin of a synthesized metric, such as a log pattern.
///
/// The Go model stores `SplitTags` as a possibly-nil map; here an empty map means "no split tags apply".
/// A map is used instead of a sorted vector so that lookup by tag key is natural, and deterministic
/// iteration order is preserved for diagnostics and export.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetricContext {
    /// The normalized pattern that generated this metric (for example a log signature).
    pub pattern: String,
    /// A recent raw input that matched the pattern.
    pub example: String,
    /// The component or data stream the signal originated from.
    pub source: String,
    /// The tag-group key/value pairs that scoped the sub-clusterer which produced this metric.
    pub split_tags: BTreeMap<String, String>,
}

/// A timeseries value derived from log analysis.
///
/// The store keeps sum/count summaries, so aggregation is specified at read time; an extractor emits raw
/// values. `context` corresponds to the Go `MetricOutput.HasContext`/`Context` pair: `None` means no
/// context is attached.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualMetric {
    /// Derived metric name.
    pub name: String,
    /// Derived value.
    pub value: f64,
    /// Host dimension of the derived metric.
    pub host: Option<String>,
    /// Tags of the derived metric.
    pub tags: Vec<String>,
    /// Optional origin context.
    pub context: Option<MetricContext>,
}

/// Which BOCPD condition opened an anomaly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BocpdTrigger {
    /// The trigger is not known (Go zero value).
    Unknown,
    /// The change-point probability crossed its threshold.
    ChangePointProbability,
    /// The short-run probability mass crossed its threshold.
    ShortRunMass,
}

/// Detector-specific evidence explaining why an anomaly was detected.
///
/// Each variant carries exactly the fields the corresponding Go detector populates in
/// `AnomalyDebugInfo`. Fields the detector does not set are simply absent, matching the Go zero values that
/// would otherwise be serialized.
#[derive(Clone, Debug, PartialEq)]
pub enum AnomalyEvidence {
    /// Evidence from the Bayesian online change-point detector.
    Bocpd {
        /// Mean of the baseline window.
        baseline_mean: f64,
        /// Standard deviation of the baseline window.
        baseline_stddev: f64,
        /// The threshold that was crossed.
        threshold: f64,
        /// The value at detection time.
        current_value: f64,
        /// How many standard deviations the current value is from the baseline.
        deviation_sigma: f64,
        /// Which BOCPD condition triggered the anomaly.
        trigger: BocpdTrigger,
        /// The change-point probability at detection time.
        change_point_prob: f64,
        /// The short-run probability mass at detection time.
        short_run_mass: f64,
        /// The configured short-run length used by the detector.
        short_run_length: usize,
    },
    /// Evidence from the Mann-Whitney scan detector.
    ScanMw {
        /// Median of the pre-change segment.
        baseline_median: f64,
        /// MAD-scaled deviation of the pre-change segment.
        baseline_mad: f64,
        /// Median of the post-change segment.
        current_value: f64,
        /// How many sigmas the change represents.
        deviation_sigma: f64,
        /// The best p-value found by the scan.
        p_value: f64,
        /// The effect size of the change.
        effect_size: f64,
    },
    /// Evidence from the Welch-t scan detector.
    ScanWelch {
        /// Median of the pre-change segment.
        baseline_median: f64,
        /// MAD-scaled deviation of the pre-change segment.
        baseline_mad: f64,
        /// Median of the post-change segment.
        current_value: f64,
        /// How many sigmas the change represents.
        deviation_sigma: f64,
        /// The p-value of the subsequent Mann-Whitney verification.
        p_value: f64,
        /// The effect size of the change.
        effect_size: f64,
        /// The absolute Welch t statistic of the maximal split.
        test_statistic: f64,
    },
    /// Evidence from the Holt-residual detector.
    HoltResidual {
        /// Median of the residual baseline window.
        baseline_median: f64,
        /// MAD-scaled deviation of the residual baseline window.
        baseline_mad: f64,
        /// The value at detection time.
        current_value: f64,
        /// How many sigmas from baseline (absolute).
        deviation_sigma: f64,
        /// The z-score threshold that was crossed.
        threshold: f64,
        /// The forecast residual model value.
        forecast: f64,
        /// The residual at detection time.
        residual: f64,
        /// The Holt level component.
        holt_level: f64,
        /// The Holt trend component.
        holt_trend: f64,
        /// The deviation expressed in MADs.
        value_mads: f64,
    },
    /// Evidence from the Tukey-biweight detector.
    TukeyBiweight {
        /// The robust location estimate.
        baseline_median: f64,
        /// The robust scale estimate.
        baseline_mad: f64,
        /// The latest value at detection time.
        current_value: f64,
        /// The absolute robust z-score.
        deviation_sigma: f64,
        /// The baseline window size used by the detector.
        sample_count: usize,
        /// The signed robust z-score.
        z_score: f64,
    },
}

/// A detected anomaly event.
#[derive(Clone, Debug, PartialEq)]
pub struct Anomaly {
    /// The source type of the anomaly.
    pub anomaly_type: AnomalyType,
    /// The fully resolved identity of the affected series.
    pub series: crate::identity::SeriesDescriptor,
    /// The storage handle for the series, when the anomaly came from stored data. Detector outputs must set
    /// this; standalone scorer inputs may leave it `None`.
    pub series_ref: Option<QueryHandle>,
    /// The name of the detector that produced the anomaly.
    pub detector_name: String,
    /// Optional origin context, for log-derived series enrichment.
    pub context: Option<MetricContext>,
    /// Data-time timestamp (whole Unix seconds) at which the anomaly was detected.
    pub timestamp_sec: i64,
    /// Optional confidence/severity score. `None` is not the same as `Some(0.0)`.
    pub score: Option<f64>,
    /// The median interval between consecutive points of the source series, in seconds. Zero when unknown.
    pub sampling_interval_sec: i64,
    /// Optional detector-specific evidence. `None` when the detector provides none.
    pub evidence: Option<AnomalyEvidence>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Aggregate, SeriesDescriptor, SeriesRef};

    #[test]
    fn anomaly_score_distinguishes_null_from_zero() {
        let series = SeriesDescriptor::new("parquet", "metric.name", None, Vec::new(), Aggregate::None);

        let unscored = Anomaly {
            anomaly_type: AnomalyType::Metric,
            series: series.clone(),
            series_ref: Some(QueryHandle::new(SeriesRef::new(1), Aggregate::Average)),
            detector_name: "bocpd".to_string(),
            context: None,
            timestamp_sec: 1_700_000_000,
            score: None,
            sampling_interval_sec: 0,
            evidence: None,
        };
        let zero_scored = Anomaly {
            score: Some(0.0),
            ..unscored.clone()
        };

        assert_ne!(unscored, zero_scored);
        assert_eq!(unscored.score, None);
        assert_eq!(zero_scored.score, Some(0.0));
    }

    #[test]
    fn aggregate_labels_match_go() {
        assert_eq!(Aggregate::None.as_str(), "none");
        assert_eq!(Aggregate::Average.as_str(), "avg");
        assert_eq!(Aggregate::Sum.as_str(), "sum");
        assert_eq!(Aggregate::Count.as_str(), "count");
        assert_eq!(Aggregate::Average.ordinal(), 1);
    }
}

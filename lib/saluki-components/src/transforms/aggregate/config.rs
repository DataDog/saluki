use saluki_event::metric::HistogramSummary;
use serde::Deserialize;
use stringtheory::MetaString;

/// A histogram statistic to calculate.
#[derive(Clone)]
pub enum HistogramStatistic {
    /// Total count of values in the histogram.
    Count,

    /// Sum of all values in the histogram.
    Sum,

    /// Minimum value in the histogram.
    Minimum,

    /// Maximum value in the histogram.
    Maximum,

    /// Average value in the histogram.
    Average,

    /// Median value in the histogram.
    Median,

    /// Calculate the given percentile from the histogram.
    Percentile {
        /// Quantile value to calculate.
        q: f64,

        /// Suffix to append to the metric name, representing the percentile.
        ///
        /// For example, a percentile of 95% (quantile value of 0.95) would have a suffix of `95percentile`.
        suffix: MetaString,
    },
}

impl HistogramStatistic {
    /// Returns the suffix to be used for metrics representing this statistic.
    pub fn suffix(&self) -> &str {
        match self {
            HistogramStatistic::Count => "count",
            HistogramStatistic::Sum => "sum",
            HistogramStatistic::Minimum => "min",
            HistogramStatistic::Maximum => "max",
            HistogramStatistic::Average => "avg",
            HistogramStatistic::Median => "median",
            HistogramStatistic::Percentile { suffix, .. } => suffix,
        }
    }

    /// Returns `true` if this statistics should be represented as a rate.
    pub fn is_rate_statistic(&self) -> bool {
        matches!(self, HistogramStatistic::Count)
    }

    /// Returns the value of this statistic from the given histogram summary.
    pub fn value_from_histogram(&self, summary: &HistogramSummary<'_>) -> f64 {
        match self {
            HistogramStatistic::Count => summary.count() as f64,
            HistogramStatistic::Sum => summary.sum(),
            HistogramStatistic::Minimum => summary.min().unwrap_or(0.0),
            HistogramStatistic::Maximum => summary.max().unwrap_or(0.0),
            HistogramStatistic::Average => summary.avg(),
            HistogramStatistic::Median => summary.median().unwrap_or(0.0),
            HistogramStatistic::Percentile { q, .. } => summary.quantile(*q).unwrap_or(0.0),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawHistogramConfiguration {
    /// Aggregates to calculate over histograms.
    ///
    /// Available aggregates: `count`, `sum`, `min`, `max`, `average`, and `median`
    ///
    /// The metric name generated for each aggregate will be in the form of `<original metric name>.<aggregate>`.
    histogram_aggregates: Vec<String>,

    /// Percentiles to calculate over histograms.
    ///
    /// Percentiles are expressed in quantile form: 95% becomes 0.95, and so on. Any floating-point number between
    /// 0.0 and 1.0 (inclusive) is allowed, but values that extend beyond two decimal places (i.e. 0.999) will be
    /// truncated.
    ///
    /// The metric name generated for each percentile will be in the form of `<original metric
    /// name>.<stripped_q>percentile`, where `stripped_q` represents the quantile multiplied by 100. For example, a
    /// percentile of 0.95 would be represented as `95percentile`, while a percentile of 0.05 would be represented as
    /// `5percentile`.
    histogram_percentiles: Vec<String>,

    /// Whether to copy histograms to distributions.
    ///
    /// Emits a copy of each histogram as a distribution, potentially with
    /// a prefixed version of the histogram's name, based on the value of
    /// `copy_histogram_to_distribution_prefix`.
    ///
    /// Defaults to `false`.
    histogram_copy_to_distribution: bool,

    /// Prefix to append to the name of distributions copied from histograms.
    ///
    /// Defaults to an empty string (no prefixing).
    histogram_copy_to_distribution_prefix: String,
}

impl Default for RawHistogramConfiguration {
    fn default() -> Self {
        Self {
            histogram_aggregates: vec!["max".into(), "median".into(), "avg".into(), "count".into()],
            histogram_percentiles: vec!["0.95".into()],
            histogram_copy_to_distribution: false,
            histogram_copy_to_distribution_prefix: "".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(try_from = "RawHistogramConfiguration")]
pub struct HistogramConfiguration {
    statistics: Vec<HistogramStatistic>,
    copy_to_distribution: bool,
    copy_to_distribution_prefix: String,
}

impl HistogramConfiguration {
    #[cfg(test)]
    pub fn from_statistics(
        statistics: &[HistogramStatistic], copy_to_distribution: bool, copy_to_distribution_prefix: String,
    ) -> Self {
        Self {
            statistics: statistics.to_vec(),
            copy_to_distribution,
            copy_to_distribution_prefix,
        }
    }

    /// Returns the configured aggregate statistics to calculate.
    pub fn statistics(&self) -> &[HistogramStatistic] {
        &self.statistics
    }

    /// Returns `true` if histograms should be copied to distributions.
    pub fn copy_to_distribution(&self) -> bool {
        self.copy_to_distribution
    }

    /// Returns the prefix to append to the distributions copied from histograms.
    pub fn copy_to_distribution_prefix(&self) -> &str {
        &self.copy_to_distribution_prefix
    }
}

impl Default for HistogramConfiguration {
    fn default() -> Self {
        Self {
            statistics: vec![
                HistogramStatistic::Maximum,
                HistogramStatistic::Median,
                HistogramStatistic::Average,
                HistogramStatistic::Count,
                HistogramStatistic::Percentile {
                    q: 0.95,
                    suffix: "95percentile".into(),
                },
            ],
            copy_to_distribution: false,
            copy_to_distribution_prefix: "".into(),
        }
    }
}

impl TryFrom<RawHistogramConfiguration> for HistogramConfiguration {
    type Error = String;

    fn try_from(raw: RawHistogramConfiguration) -> Result<Self, Self::Error> {
        let mut statistics = Vec::new();

        for aggregate in raw.histogram_aggregates {
            match aggregate.as_str() {
                "count" => statistics.push(HistogramStatistic::Count),
                "sum" => statistics.push(HistogramStatistic::Sum),
                "min" => statistics.push(HistogramStatistic::Minimum),
                "max" => statistics.push(HistogramStatistic::Maximum),
                "avg" => statistics.push(HistogramStatistic::Average),
                "median" => statistics.push(HistogramStatistic::Median),
                _ => return Err(format!("Unknown histogram aggregate: {}", aggregate)),
            }
        }

        for faux_percentile in raw.histogram_percentiles {
            let quantile = faux_percentile
                .parse::<f64>()
                .map_err(|_| format!("Invalid percentile: {}", faux_percentile))?;
            if !(0.0..=1.0).contains(&quantile) {
                return Err(format!("Percentile out of range: {}", faux_percentile));
            }

            // Truncate the quantile value and then generate the string representation for the metric name suffix.
            //
            // We do the weird multiplication and division when we generate the suffix because you can end up with
            // rounding errors, such that you might get `28` when doing `(0.29 * 100) as u32`.
            let quantile = (quantile * 100.0).trunc() / 100.0;
            let suffix = format!("{}percentile", ((quantile * 1000.0) as u32 / 10)).into();
            statistics.push(HistogramStatistic::Percentile { q: quantile, suffix });
        }

        Ok(Self {
            statistics,
            copy_to_distribution: raw.histogram_copy_to_distribution,
            copy_to_distribution_prefix: raw.histogram_copy_to_distribution_prefix,
        })
    }
}

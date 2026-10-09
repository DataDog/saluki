//! The statistics aggregation computes over histograms.

use saluki_core::data_model::event::metric::HistogramSummary;
use stringtheory::MetaString;

use crate::params::{HistogramAggregate, HistogramParams, InvalidHistogramStatistic};

/// A histogram statistic to calculate.
#[derive(Clone, Debug, PartialEq)]
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
            HistogramStatistic::Percentile { q, .. } => {
                saluki_antithesis::always_ge!(*q, 0.0, "histogram percentile quantile at or above zero");
                saluki_antithesis::always_le!(*q, 1.0, "histogram percentile quantile at or below one");
                summary.quantile(*q).unwrap_or(0.0)
            }
        }
    }
}

/// Statistics to calculate over histograms, and how to copy them to distributions.
#[derive(Clone, Debug, PartialEq)]
pub struct HistogramConfiguration {
    statistics: Vec<HistogramStatistic>,
    copy_to_distribution: bool,
    copy_to_distribution_prefix: String,
}

impl HistogramConfiguration {
    /// Creates a new `HistogramConfiguration` from the aggregates and percentiles to calculate.
    ///
    /// Aggregates name a statistic to compute over each histogram: `count`, `sum`, `min`, `max`, `avg`, or `median`.
    /// Percentiles are expressed in quantile form, between `0.0` and `1.0` inclusive: 95% becomes `0.95`. Quantiles
    /// that extend beyond two decimal places are rounded to the nearest whole percentile. See
    /// [`HistogramParams::from_names`].
    ///
    /// # Errors
    ///
    /// Returns an error if an aggregate names an unsupported statistic, or if a percentile is not a number between
    /// `0.0` and `1.0`.
    pub fn try_new(
        aggregates: &[String], percentiles: &[String], copy_to_distribution: bool, copy_to_distribution_prefix: String,
    ) -> Result<Self, InvalidHistogramStatistic> {
        let params = HistogramParams::from_names(
            aggregates,
            percentiles,
            copy_to_distribution,
            copy_to_distribution_prefix,
        )?;
        Ok(Self::from_params(&params))
    }

    /// Creates a configuration from histogram parameters.
    ///
    /// Statistics are computed in the order the parameters list them: aggregates first, then percentiles. A percentile
    /// `p` computes the quantile `p / 100` and is emitted with the suffix `<p>percentile`.
    pub fn from_params(params: &HistogramParams) -> Self {
        let mut statistics = Vec::with_capacity(params.aggregates.len() + params.percentiles.len());

        for aggregate in &params.aggregates {
            statistics.push(match aggregate {
                HistogramAggregate::Count => HistogramStatistic::Count,
                HistogramAggregate::Sum => HistogramStatistic::Sum,
                HistogramAggregate::Min => HistogramStatistic::Minimum,
                HistogramAggregate::Max => HistogramStatistic::Maximum,
                HistogramAggregate::Avg => HistogramStatistic::Average,
                HistogramAggregate::Median => HistogramStatistic::Median,
            });
        }

        for percentile in &params.percentiles {
            let quantile = f64::from(*percentile) / 100.0;
            let suffix = format!("{}percentile", percentile).into();
            statistics.push(HistogramStatistic::Percentile { q: quantile, suffix });
        }

        Self {
            statistics,
            copy_to_distribution: params.copy_to_distribution,
            copy_to_distribution_prefix: params.copy_to_distribution_prefix.clone(),
        }
    }

    /// Creates a configuration from already-parsed statistics, for tests that exercise histogram aggregation rather
    /// than configuration.
    #[cfg(any(test, feature = "test-util"))]
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

/// Fixture values for tests that exercise histogram aggregation rather than configuration.
#[cfg(any(test, feature = "test-util"))]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `try_new` documents two rejection branches: an aggregate that names no supported statistic, and a percentile
    /// that is not a quantile between `0.0` and `1.0`.
    #[test]
    fn unsupported_aggregates_and_percentiles_are_rejected() {
        let cases: [(&[&str], &[&str], &str); 5] = [
            (&["p99"], &[], "Unknown histogram aggregate: p99"),
            (&[], &["abc"], "Invalid percentile: abc"),
            (&[], &["1.1"], "Percentile out of range: 1.1"),
            (&[], &["-0.1"], "Percentile out of range: -0.1"),
            // Aggregates are checked before percentiles.
            (&["max", "p50"], &["2"], "Unknown histogram aggregate: p50"),
        ];
        for (aggregates, percentiles, expected) in cases {
            let aggregates = aggregates.iter().map(|name| name.to_string()).collect::<Vec<_>>();
            let percentiles = percentiles.iter().map(|name| name.to_string()).collect::<Vec<_>>();
            let error =
                HistogramConfiguration::try_new(&aggregates, &percentiles, false, String::new()).expect_err(expected);
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn statistics_follow_the_parameters_in_order() {
        let params = HistogramParams {
            aggregates: vec![
                HistogramAggregate::Max,
                HistogramAggregate::Count,
                HistogramAggregate::Max,
            ],
            percentiles: vec![95, 0, 100],
            copy_to_distribution: true,
            copy_to_distribution_prefix: "dist.".to_string(),
        };
        let config = HistogramConfiguration::from_params(&params);

        let suffixes = config
            .statistics()
            .iter()
            .map(HistogramStatistic::suffix)
            .collect::<Vec<_>>();
        assert_eq!(
            suffixes,
            ["max", "count", "max", "95percentile", "0percentile", "100percentile"]
        );
        assert_eq!(params.suffixes().collect::<Vec<_>>(), suffixes);
        assert_eq!(
            config.statistics()[3],
            HistogramStatistic::Percentile {
                q: 0.95,
                suffix: "95percentile".into(),
            }
        );
        assert!(config.copy_to_distribution());
        assert_eq!(config.copy_to_distribution_prefix(), "dist.");
    }

    #[test]
    fn percentile_suffixes_match_agent_rounding() {
        let config =
            HistogramConfiguration::try_new(&[], &["0.299".to_string(), "0.73".to_string()], false, String::new())
                .unwrap();

        assert_eq!(
            config.statistics(),
            &[
                HistogramStatistic::Percentile {
                    q: 0.30,
                    suffix: "30percentile".into(),
                },
                HistogramStatistic::Percentile {
                    q: 0.73,
                    suffix: "73percentile".into(),
                },
            ]
        );
    }
}

use agent_data_plane_config::shared::HistogramEncoding;
use saluki_core::data_model::event::metric::HistogramSummary;
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;

/// A histogram statistic to calculate.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
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

#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
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

    /// Builds a histogram configuration from the shared histogram encoding settings.
    ///
    /// Unknown aggregates or out-of-range percentiles are rejected. Percentiles that extend beyond
    /// two decimal places are rounded to the nearest whole percentile to match the Datadog Agent.
    pub fn from_encoding(encoding: &HistogramEncoding) -> Result<Self, GenericError> {
        let mut statistics = Vec::new();

        for aggregate in &encoding.aggregates {
            match aggregate.as_str() {
                "count" => statistics.push(HistogramStatistic::Count),
                "sum" => statistics.push(HistogramStatistic::Sum),
                "min" => statistics.push(HistogramStatistic::Minimum),
                "max" => statistics.push(HistogramStatistic::Maximum),
                "avg" => statistics.push(HistogramStatistic::Average),
                "median" => statistics.push(HistogramStatistic::Median),
                _ => return Err(generic_error!("Unknown histogram aggregate: {}", aggregate)),
            }
        }

        for faux_percentile in &encoding.percentiles {
            let quantile = faux_percentile
                .parse::<f64>()
                .map_err(|_| generic_error!("Invalid percentile: {}", faux_percentile))?;
            if !(0.0..=1.0).contains(&quantile) {
                return Err(generic_error!("Percentile out of range: {}", faux_percentile));
            }

            let percentile = (quantile * 100.0 + 0.5) as u32;
            let quantile = f64::from(percentile) / 100.0;
            let suffix = format!("{}percentile", percentile).into();
            statistics.push(HistogramStatistic::Percentile { q: quantile, suffix });
        }

        Ok(Self {
            statistics,
            copy_to_distribution: encoding.copy_to_distribution,
            copy_to_distribution_prefix: encoding.copy_to_distribution_prefix.clone(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_suffixes_match_agent_rounding() {
        let encoding = HistogramEncoding {
            aggregates: Vec::new(),
            percentiles: vec!["0.299".to_string(), "0.73".to_string()],
            copy_to_distribution: false,
            copy_to_distribution_prefix: String::new(),
        };

        let config = HistogramConfiguration::from_encoding(&encoding).unwrap();

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

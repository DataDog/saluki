use serde::Deserialize;
use stringtheory::MetaString;

/// A distribution statistic to calculate.
#[derive(Clone)]
pub enum DistributionStatistic {
    /// Total count of values in the distribution.
    Count,

    /// Sum of all values in the distribution.
    Sum,

    /// Minimum value in the distribution.
    Minimum,

    /// Maximum value in the distribution.
    Maximum,

    /// Average value in the distribution.
    Average,

    /// Median value in the distribution.
    Median,

    /// Calculate the given percentile from the distribution.
    Percentile {
        /// Quantile value to calculate.
        q: f64,

        /// Suffix to append to the metric name, representing the percentile.
        ///
        /// For example, a percentile of 95% (quantile value of 0.95) would have a suffix of `95percentile`.
        suffix: MetaString,
    },
}

impl DistributionStatistic {
    /// Returns the suffix to be used for metrics representing this statistic.
    pub fn suffix(&self) -> &str {
        match self {
            DistributionStatistic::Count => "count",
            DistributionStatistic::Sum => "sum",
            DistributionStatistic::Minimum => "min",
            DistributionStatistic::Maximum => "max",
            DistributionStatistic::Average => "avg",
            DistributionStatistic::Median => "median",
            DistributionStatistic::Percentile { suffix, .. } => suffix,
        }
    }

    /// Returns `true` if this statistics should be represented as a rate.
    pub fn is_rate_statistic(&self) -> bool {
        matches!(self, DistributionStatistic::Count)
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
}

impl Default for RawHistogramConfiguration {
    fn default() -> Self {
        Self {
            histogram_aggregates: vec!["max".into(), "median".into(), "avg".into(), "count".into()],
            histogram_percentiles: vec!["0.95".into()],
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(try_from = "RawHistogramConfiguration")]
pub struct HistogramConfiguration {
    statistics: Vec<DistributionStatistic>,
}

impl HistogramConfiguration {
    #[cfg(test)]
    pub fn from_statistics(statistics: &[DistributionStatistic]) -> Self {
        Self {
            statistics: statistics.to_vec(),
        }
    }

    /// Returns the configured distributions statistics to calculate.
    pub fn statistics(&self) -> &[DistributionStatistic] {
        &self.statistics
    }
}

impl Default for HistogramConfiguration {
    fn default() -> Self {
        Self {
            statistics: vec![
                DistributionStatistic::Maximum,
                DistributionStatistic::Median,
                DistributionStatistic::Average,
                DistributionStatistic::Count,
                DistributionStatistic::Percentile {
                    q: 0.95,
                    suffix: "95percentile".into(),
                },
            ],
        }
    }
}

impl TryFrom<RawHistogramConfiguration> for HistogramConfiguration {
    type Error = String;

    fn try_from(raw: RawHistogramConfiguration) -> Result<Self, Self::Error> {
        let mut statistics = Vec::new();

        for aggregate in raw.histogram_aggregates {
            match aggregate.as_str() {
                "count" => statistics.push(DistributionStatistic::Count),
                "sum" => statistics.push(DistributionStatistic::Sum),
                "min" => statistics.push(DistributionStatistic::Minimum),
                "max" => statistics.push(DistributionStatistic::Maximum),
                "avg" => statistics.push(DistributionStatistic::Average),
                "median" => statistics.push(DistributionStatistic::Median),
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
            statistics.push(DistributionStatistic::Percentile { q: quantile, suffix });
        }

        Ok(Self { statistics })
    }
}

use std::{error::Error, fmt};

/// The largest percentile a histogram statistic can compute.
pub const MAX_PERCENTILE: u8 = 100;

/// An aggregate statistic computed over the samples of a histogram.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HistogramAggregate {
    /// The number of samples, emitted as a rate.
    Count,

    /// The sum of the samples.
    Sum,

    /// The smallest sample.
    Min,

    /// The largest sample.
    Max,

    /// The mean of the samples.
    Avg,

    /// The median sample.
    Median,
}

impl HistogramAggregate {
    /// Every aggregate, in declaration order.
    pub const ALL: [Self; 6] = [Self::Count, Self::Sum, Self::Min, Self::Max, Self::Avg, Self::Median];

    /// Returns the aggregate's name, which is also the suffix of the series it is emitted as.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
            Self::Median => "median",
        }
    }

    /// Returns the aggregate with the given name, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|aggregate| aggregate.as_str() == name)
    }
}

impl fmt::Display for HistogramAggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A histogram statistic that configuration names, but that is not one aggregation can compute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidHistogramStatistic {
    /// The aggregate name is not one of `count`, `sum`, `min`, `max`, `avg`, or `median`.
    UnknownAggregate(String),

    /// The percentile is not a number.
    InvalidPercentile(String),

    /// The percentile is a number outside `0.0` to `1.0`.
    PercentileOutOfRange(String),
}

impl fmt::Display for InvalidHistogramStatistic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAggregate(name) => write!(f, "Unknown histogram aggregate: {}", name),
            Self::InvalidPercentile(percentile) => write!(f, "Invalid percentile: {}", percentile),
            Self::PercentileOutOfRange(percentile) => write!(f, "Percentile out of range: {}", percentile),
        }
    }
}

impl Error for InvalidHistogramStatistic {}

/// The statistics aggregation computes for each histogram, and whether it also emits each histogram as a distribution.
///
/// When aggregation flushes a histogram named `<name>`, it emits one series per statistic, named
/// `<name>.<suffix>`: first one per aggregate, in order, then one per percentile, in order. A percentile `p` has the
/// suffix `<p>percentile`, such as `95percentile`. The count is emitted as a rate and every other statistic as a gauge.
///
/// Each statistic is listed at most once: a repeated statistic would emit the same series twice per flush.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistogramParams {
    /// The aggregates to compute, in emission order.
    pub aggregates: Vec<HistogramAggregate>,

    /// The percentiles to compute, from 0 to [`MAX_PERCENTILE`], in emission order.
    ///
    /// Percentiles are whole numbers: a configured quantile such as `0.955` is rounded to the nearest whole percentile
    /// before it reaches these parameters.
    pub percentiles: Vec<u8>,

    /// Whether each histogram is also emitted as a distribution, before its statistics.
    pub copy_to_distribution: bool,

    /// The prefix prepended to the name of each distribution copy. Empty means the copy keeps the histogram's name.
    pub copy_to_distribution_prefix: String,
}

impl HistogramParams {
    /// Creates histogram parameters from statistics as configuration spells them.
    ///
    /// `aggregates` are aggregate names (see [`HistogramAggregate::from_name`]). `quantiles` are percentiles in
    /// quantile form, between `0.0` and `1.0` inclusive, such as `0.95` for the 95 percentile; each is rounded to the
    /// nearest whole percentile, halves rounding up, so `0.955` becomes `96`. Statistics are kept in the order given,
    /// including repeats, which these parameters otherwise rule out: the caller decides what to do with them.
    ///
    /// # Errors
    ///
    /// Returns an error for the first aggregate that names no statistic, and otherwise for the first quantile that is
    /// not a number between `0.0` and `1.0`.
    pub fn from_names<A, Q>(
        aggregates: &[A], quantiles: &[Q], copy_to_distribution: bool, copy_to_distribution_prefix: String,
    ) -> Result<Self, InvalidHistogramStatistic>
    where
        A: AsRef<str>,
        Q: AsRef<str>,
    {
        let aggregates = aggregates
            .iter()
            .map(|name| {
                HistogramAggregate::from_name(name.as_ref())
                    .ok_or_else(|| InvalidHistogramStatistic::UnknownAggregate(name.as_ref().to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut percentiles = Vec::with_capacity(quantiles.len());
        for quantile in quantiles {
            let quantile = quantile.as_ref();
            let value = quantile
                .parse::<f64>()
                .map_err(|_| InvalidHistogramStatistic::InvalidPercentile(quantile.to_string()))?;
            if !(0.0..=1.0).contains(&value) {
                return Err(InvalidHistogramStatistic::PercentileOutOfRange(quantile.to_string()));
            }
            // The quantile is at most 1.0, so the percentile is at most 100 and fits.
            percentiles.push((value * 100.0 + 0.5) as u8);
        }

        Ok(Self {
            aggregates,
            percentiles,
            copy_to_distribution,
            copy_to_distribution_prefix,
        })
    }

    /// Returns the suffixes of the series emitted for each histogram, in emission order.
    pub fn suffixes(&self) -> impl Iterator<Item = String> + '_ {
        self.aggregates
            .iter()
            .map(|aggregate| aggregate.as_str().to_string())
            .chain(
                self.percentiles
                    .iter()
                    .map(|percentile| format!("{}percentile", percentile)),
            )
    }

    /// Returns the number of series emitted for each histogram per bucket: one per statistic, plus the distribution
    /// copy when enabled.
    pub fn series_per_histogram(&self) -> usize {
        self.aggregates.len() + self.percentiles.len() + usize::from(self.copy_to_distribution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_follow_aggregates_then_percentiles() {
        let params = HistogramParams {
            aggregates: vec![
                HistogramAggregate::Max,
                HistogramAggregate::Median,
                HistogramAggregate::Avg,
                HistogramAggregate::Count,
            ],
            percentiles: vec![95, 5],
            copy_to_distribution: false,
            copy_to_distribution_prefix: String::new(),
        };
        assert_eq!(
            params.suffixes().collect::<Vec<_>>(),
            vec!["max", "median", "avg", "count", "95percentile", "5percentile"]
        );
        assert_eq!(params.series_per_histogram(), 6);

        let copying = HistogramParams {
            copy_to_distribution: true,
            ..params
        };
        assert_eq!(copying.series_per_histogram(), 7);
    }

    #[test]
    fn statistics_parse_from_their_configured_names() {
        let params = HistogramParams::from_names(
            &["max", "median", "max"],
            &["0.95", "0.955", "0.299", "0", "1", "0.004", "0.005"],
            true,
            "dist.".to_string(),
        )
        .unwrap();
        assert_eq!(
            params,
            HistogramParams {
                aggregates: vec![
                    HistogramAggregate::Max,
                    HistogramAggregate::Median,
                    HistogramAggregate::Max
                ],
                percentiles: vec![95, 96, 30, 0, 100, 0, 1],
                copy_to_distribution: true,
                copy_to_distribution_prefix: "dist.".to_string(),
            }
        );

        let cases: [(&[&str], &[&str], InvalidHistogramStatistic); 4] = [
            (
                &["max", "p99"],
                &["abc"],
                InvalidHistogramStatistic::UnknownAggregate("p99".to_string()),
            ),
            (
                &[],
                &["0.5", "abc"],
                InvalidHistogramStatistic::InvalidPercentile("abc".to_string()),
            ),
            (
                &[],
                &["1.1"],
                InvalidHistogramStatistic::PercentileOutOfRange("1.1".to_string()),
            ),
            (
                &[],
                &["NaN"],
                InvalidHistogramStatistic::PercentileOutOfRange("NaN".to_string()),
            ),
        ];
        for (aggregates, quantiles, expected) in cases {
            assert_eq!(
                HistogramParams::from_names(aggregates, quantiles, false, String::new()),
                Err(expected)
            );
        }
    }

    #[test]
    fn aggregates_are_named_by_their_suffix() {
        for aggregate in HistogramAggregate::ALL {
            assert_eq!(HistogramAggregate::from_name(aggregate.as_str()), Some(aggregate));
        }
        assert_eq!(HistogramAggregate::from_name("p95"), None);
        assert_eq!(HistogramAggregate::Avg.to_string(), "avg");
    }
}

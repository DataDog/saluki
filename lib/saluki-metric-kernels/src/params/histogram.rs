use std::fmt;

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
    fn aggregates_are_named_by_their_suffix() {
        for aggregate in HistogramAggregate::ALL {
            assert_eq!(HistogramAggregate::from_name(aggregate.as_str()), Some(aggregate));
        }
        assert_eq!(HistogramAggregate::from_name("p95"), None);
        assert_eq!(HistogramAggregate::Avg.to_string(), "avg");
    }
}

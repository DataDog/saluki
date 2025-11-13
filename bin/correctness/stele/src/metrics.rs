use std::fmt;

use datadog_protos::metrics::{MetricPayload, MetricType, SketchPayload};
use ddsketch_agent::DDSketch;
use float_cmp::ApproxEqRatio as _;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

/// A metric's unique identifier.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MetricContext {
    name: String,
    tags: Vec<String>,
}

impl MetricContext {
    /// Returns the name of the context.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tags of the context.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Consumes this context, returning the name and tags.
    pub fn into_parts(self) -> (String, Vec<String>) {
        (self.name, self.tags)
    }
}

impl fmt::Display for MetricContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;

        if !self.tags.is_empty() {
            write!(f, " {{{}}}", self.tags.join(", "))?;
        }

        Ok(())
    }
}

/// A simplified metric representation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Metric {
    context: MetricContext,
    values: Vec<(u64, MetricValue)>,
}

impl Metric {
    /// Returns the context of the metric.
    pub fn context(&self) -> &MetricContext {
        &self.context
    }

    /// Returns the values associated with the metric.
    pub fn values(&self) -> &[(u64, MetricValue)] {
        &self.values
    }
}

/// A metric value.
///
/// # Equality
///
/// `MetricValue` implements `PartialEq` and `Eq`, the majority of which involves comparing floating-point (`f64`)
/// numbers. Comparing floating-point numbers for equality is inherently tricky ([this][bitbanging_io] is just one blog
/// post/article out of thousands on the subject). In the equality implementation for `MetricValue`, we use a
/// ratio-based approach.
///
/// This means that when comparing two floating-point numbers, we look at their _ratio_ to one another, with an upper
/// bound on the allowed difference. For example, if we compare 99 to 100, there's a difference of 1% (`1 - (99/100) =
/// 0.01 = 1%`), while the difference between 99.999 and 100 is only 0.001% (`1 - (99.999/100) = 0.00001 = 0.001%`). As
/// most comparisons are expected to be close, only differing by a few ULPs (units in the last place) due to slight
/// differences in how floating-point numbers are implemented between Go and Rust, this approach is sufficient to
/// compensate for the inherent imprecision while not falling victim to relying on ULPs or epsilon directly, whose
/// applicability depends on the number range being compared.
///
/// Specifically, we compare floating-point numbers using a ratio of `0.00000001` (0.0000001%), meaning the smaller of
/// the two values being compared must be within 99.999999% to 100% of the larger number, which is sufficiently precise
/// for our concerns.
///
/// [bitbanging_io]: https://bitbashing.io/comparing-floats.html
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mtype")]
pub enum MetricValue {
    /// A count.
    Count {
        /// The value of the count.
        value: f64,
    },

    /// A rate.
    ///
    /// Rates are per-second adjusted counts. For example, a count that increased by 100 over 10 seconds would be
    /// represented as a rate with an interval of 10 (seconds) and a value of 10 (`100 / 10 = 10`).
    Rate {
        /// The interval of the rate, in seconds.
        interval: u64,

        /// The per-second value of the rate.
        value: f64,
    },

    /// A gauge.
    Gauge {
        /// The value of the gauge.
        value: f64,
    },

    /// A sketch.
    Sketch {
        /// The sketch data.
        sketch: DDSketch,
    },
}

impl PartialEq for MetricValue {
    fn eq(&self, other: &Self) -> bool {
        // When comparing two values, the smaller value cannot deviate by more than 0.0000001% of the larger value.
        const RATIO_ERROR: f64 = 0.00000001;

        match (self, other) {
            (MetricValue::Count { value: value_a }, MetricValue::Count { value: value_b }) => {
                value_a.approx_eq_ratio(value_b, RATIO_ERROR)
            }
            (
                MetricValue::Rate {
                    interval: interval_a,
                    value: value_a,
                },
                MetricValue::Rate {
                    interval: interval_b,
                    value: value_b,
                },
            ) => interval_a == interval_b && value_a.approx_eq_ratio(value_b, RATIO_ERROR),
            (MetricValue::Gauge { value: value_a }, MetricValue::Gauge { value: value_b }) => {
                value_a.approx_eq_ratio(value_b, RATIO_ERROR)
            }
            (MetricValue::Sketch { sketch: sketch_a }, MetricValue::Sketch { sketch: sketch_b }) => {
                approx_eq_ratio_optional(sketch_a.min(), sketch_b.min(), RATIO_ERROR)
                    && approx_eq_ratio_optional(sketch_a.max(), sketch_b.max(), RATIO_ERROR)
                    && approx_eq_ratio_optional(sketch_a.avg(), sketch_b.avg(), RATIO_ERROR)
                    && approx_eq_ratio_optional(sketch_a.sum(), sketch_b.sum(), RATIO_ERROR)
                    && sketch_a.count() == sketch_b.count()
                    && sketch_a.bin_count() == sketch_b.bin_count()
            }
            _ => false,
        }
    }
}

impl Eq for MetricValue {}

impl Metric {
    /// Attempts to parse metrics from a series payload.
    ///
    /// # Errors
    ///
    /// If the metric payload contains invalid data, an error will be returned.
    pub fn try_from_series(payload: MetricPayload) -> Result<Vec<Self>, GenericError> {
        let mut metrics = Vec::new();

        for series in payload.series {
            let name = series.metric().to_string();
            let tags = series.tags().iter().map(|tag| tag.to_string()).collect();
            let mut values = Vec::new();

            match series.type_() {
                MetricType::UNSPECIFIED => {
                    return Err(generic_error!("Received metric series with UNSPECIFIED type."));
                }
                MetricType::COUNT => {
                    for point in series.points {
                        let timestamp = u64::try_from(point.timestamp)
                            .map_err(|_| generic_error!("Invalid timestamp for point: {}", point.timestamp))?;
                        values.push((timestamp, MetricValue::Count { value: point.value }));
                    }
                }
                MetricType::RATE => {
                    for point in series.points {
                        let timestamp = u64::try_from(point.timestamp)
                            .map_err(|_| generic_error!("Invalid timestamp for point: {}", point.timestamp))?;
                        values.push((
                            timestamp,
                            MetricValue::Rate {
                                interval: series.interval as u64,
                                value: point.value,
                            },
                        ));
                    }
                }
                MetricType::GAUGE => {
                    for point in series.points {
                        let timestamp = u64::try_from(point.timestamp)
                            .map_err(|_| generic_error!("Invalid timestamp for point: {}", point.timestamp))?;
                        values.push((timestamp, MetricValue::Gauge { value: point.value }));
                    }
                }
            }

            metrics.push(Metric {
                context: MetricContext { name, tags },
                values,
            })
        }

        Ok(metrics)
    }

    /// Attempts to parse metrics from a sketch payload.
    ///
    /// # Errors
    ///
    /// If the sketch payload contains invalid data, an error will be returned.
    pub fn try_from_sketch(payload: SketchPayload) -> Result<Vec<Self>, GenericError> {
        let mut metrics = Vec::new();

        for sketch in payload.sketches {
            let name = sketch.metric().to_string();
            let tags = sketch.tags().iter().map(|tag| tag.to_string()).collect();
            let mut values = Vec::new();

            for dogsketch in sketch.dogsketches {
                let timestamp = u64::try_from(dogsketch.ts)
                    .map_err(|_| generic_error!("Invalid timestamp for sketch: {}", dogsketch.ts))?;
                let sketch = DDSketch::try_from(dogsketch)
                    .map_err(|e| generic_error!("Failed to convert DogSketch to DDSketch: {}", e))?;
                values.push((timestamp, MetricValue::Sketch { sketch }));
            }

            metrics.push(Metric {
                context: MetricContext { name, tags },
                values,
            })
        }

        Ok(metrics)
    }
}

fn approx_eq_ratio_optional(a: Option<f64>, b: Option<f64>, ratio: f64) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.approx_eq_ratio(&b, ratio),
        (None, None) => true,
        _ => false,
    }
}

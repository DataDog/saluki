use std::{collections::HashSet, fmt, time::Duration};

use ddsketch_agent::DDSketch;

/// A metric value.
#[derive(Clone, Debug)]
pub enum MetricValue {
    /// A counter.
    ///
    /// Counters generally represent a monotonically increasing value, such as the number of requests received.
    Counter {
        /// Counter value.
        value: f64,
    },

    /// A rate.
    ///
    /// Rates define the rate of change over a given interval, in seconds.
    ///
    /// For example, a rate with a value of 1.5 and an interval of 10 seconds would indicate that the value increases by
    /// 15 every 10 seconds.
    Rate {
        /// Normalized per-second value.
        value: f64,

        /// Interval over which the rate is calculated.
        interval: Duration,
    },

    /// A gauge.
    ///
    /// Gauges represent the latest value of a quantity, such as the current number of active connections. This value
    /// can go up or down, but gauges do not track the individual changes to the value, only the latest value.
    Gauge {
        /// Gauge value.
        value: f64,
    },

    /// A set.
    ///
    /// Sets represent a collection of unique values, such as the unique IP addresses that have connected to a service.
    Set {
        /// Unique values in the set.
        values: HashSet<String>,
    },

    /// A distribution.
    ///
    /// Distributions represent the distribution of a quantity, such as the response times for a service. By tracking
    /// each individual measurement, statistics can be derived over the sample set to provide insight, such as minimum
    /// and maximum value, how many of the values are above or below a specific threshold, and more.
    Distribution {
        /// The internal sketch representing the distribution.
        sketch: DDSketch,
    },
}

impl MetricValue {
    /// Returns the type of the metric value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Counter { .. } => "counter",
            Self::Rate { .. } => "rate",
            Self::Gauge { .. } => "gauge",
            Self::Set { .. } => "set",
            Self::Distribution { .. } => "distribution",
        }
    }

    /// Creates a distribution from a single value.
    pub fn distribution_from_value(value: f64) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert(value);
        Self::Distribution { sketch }
    }

    /// Creates a distribution from multiple values.
    pub fn distribution_from_values(values: &[f64]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);
        Self::Distribution { sketch }
    }

    /// Creates a distribution from values in an iterator.
    pub fn distribution_from_iter<I, E>(iter: I) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let mut sketch = DDSketch::default();
        for value in iter {
            sketch.insert(value?);
        }
        Ok(Self::Distribution { sketch })
    }

    /// Merges another metric value into this one.
    ///
    /// If both `self` and `other` are the same metric type, their values will be merged appropriately. If the metric
    /// types are different, the incoming value will override the existing value.
    ///
    /// For rates, the interval of both rates must match to be merged. For gauges, the incoming value will override the
    /// existing value.
    pub fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Counter { value: a }, Self::Counter { value: b }) => *a += b,
            (Self::Rate { value: a, interval: i1 }, Self::Rate { value: b, interval: i2 }) if *i1 == i2 => *a += b,
            (Self::Gauge { value: a }, Self::Gauge { value: b }) => *a = b,
            (Self::Set { values: a }, Self::Set { values: b }) => {
                a.extend(b);
            }
            (Self::Distribution { sketch: sketch_a }, Self::Distribution { sketch: sketch_b }) => {
                sketch_a.merge(&sketch_b)
            }

            // Just override with whatever the incoming value is.
            (dest, src) => drop(std::mem::replace(dest, src)),
        }
    }
}

impl PartialEq for MetricValue {
    fn eq(&self, other: &Self) -> bool {
        use float_eq::FloatEq as _;

        match (self, other) {
            (Self::Counter { value: l_value }, Self::Counter { value: r_value }) => l_value.eq_ulps(r_value, &1),
            (
                Self::Rate {
                    value: l_value,
                    interval: l_interval,
                },
                Self::Rate {
                    value: r_value,
                    interval: r_interval,
                },
            ) => l_value.eq_ulps(r_value, &1) && l_interval == r_interval,
            (Self::Gauge { value: l_value }, Self::Gauge { value: r_value }) => l_value.eq_ulps(r_value, &1),
            (Self::Set { values: l_values }, Self::Set { values: r_values }) => l_values == r_values,
            (Self::Distribution { sketch: l_sketch }, Self::Distribution { sketch: r_sketch }) => l_sketch == r_sketch,
            _ => false,
        }
    }
}

impl Eq for MetricValue {}

impl fmt::Display for MetricValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counter { value } => write!(f, "counter<{}>", value),
            Self::Rate { value, interval } => write!(f, "rate<{} over {:?}>", value, interval),
            Self::Gauge { value } => write!(f, "gauge<{}>", value),
            Self::Set { values } => write!(f, "set<{:?}>", values),
            Self::Distribution { sketch } => write!(f, "distribution<{:?}>", sketch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_counter() {
        let mut a = MetricValue::Counter { value: 1.0 };
        let b = MetricValue::Counter { value: 2.0 };

        a.merge(b);

        assert_eq!(a, MetricValue::Counter { value: 3.0 });
    }

    #[test]
    fn merge_rate_same_interval() {
        let mut a = MetricValue::Rate {
            value: 1.0,
            interval: Duration::from_secs(1),
        };
        let b = MetricValue::Rate {
            value: 2.0,
            interval: Duration::from_secs(1),
        };

        a.merge(b);

        assert_eq!(
            a,
            MetricValue::Rate {
                value: 3.0,
                interval: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn merge_rate_different_interval() {
        let mut a = MetricValue::Rate {
            value: 1.0,
            interval: Duration::from_secs(1),
        };
        let b = MetricValue::Rate {
            value: 2.0,
            interval: Duration::from_secs(2),
        };

        a.merge(b);

        assert_eq!(
            a,
            MetricValue::Rate {
                value: 2.0,
                interval: Duration::from_secs(2),
            }
        );
    }

    #[test]
    fn merge_gauge() {
        let mut a = MetricValue::Gauge { value: 1.0 };
        let b = MetricValue::Gauge { value: 2.0 };

        a.merge(b);

        assert_eq!(a, MetricValue::Gauge { value: 2.0 });
    }

    #[test]
    fn merge_set() {
        let mut a = MetricValue::Set {
            values: vec!["a".to_string(), "b".to_string()].into_iter().collect(),
        };
        let b = MetricValue::Set {
            values: vec!["b".to_string(), "c".to_string()].into_iter().collect(),
        };

        a.merge(b);

        assert_eq!(
            a,
            MetricValue::Set {
                values: vec!["a".to_string(), "b".to_string(), "c".to_string()]
                    .into_iter()
                    .collect()
            }
        );
    }

    #[test]
    fn merge_distribution() {
        let mut sketch_a = DDSketch::default();
        sketch_a.insert(1.0);
        let mut a = MetricValue::Distribution { sketch: sketch_a };

        let mut sketch_b = DDSketch::default();
        sketch_b.insert(2.0);
        let b = MetricValue::Distribution { sketch: sketch_b };

        a.merge(b);

        assert_eq!(
            a,
            MetricValue::Distribution {
                sketch: {
                    let mut sketch = DDSketch::default();
                    sketch.insert(1.0);
                    sketch.insert(2.0);
                    sketch
                }
            }
        );
    }

    #[test]
    fn merge_different_type() {
        let mut a = MetricValue::Counter { value: 1.0 };
        let b = MetricValue::Gauge { value: 2.0 };

        a.merge(b);

        assert_eq!(a, MetricValue::Gauge { value: 2.0 });
    }
}

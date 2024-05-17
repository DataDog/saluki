use std::{collections::HashSet, fmt, time::Duration};

use ddsketch_agent::{DDSketch, Sketch};

#[derive(Clone, Debug)]
pub enum MetricValue {
    Counter { value: f64 },
    Rate { value: f64, interval: Duration },
    Gauge { value: f64 },
    Set { values: HashSet<String> },
    Distribution { sketch: DDSketch },
}

impl MetricValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Counter { .. } => "counter",
            Self::Rate { .. } => "rate",
            Self::Gauge { .. } => "gauge",
            Self::Set { .. } => "set",
            Self::Distribution { .. } => "distribution",
        }
    }

    pub fn distribution_from_value(value: f64) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert(value);
        Self::Distribution { sketch }
    }

    pub fn distribution_from_values(values: &[f64]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);
        Self::Distribution { sketch }
    }

    pub fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Counter { value: a }, Self::Counter { value: b }) => *a += b,
            (Self::Rate { value: a, interval: i1 }, Self::Rate { value: b, interval: i2 }) if *i1 == i2 => *a += b,
            (Self::Gauge { value: a }, Self::Gauge { value: b }) => *a = b,
            (Self::Set { values: a }, Self::Set { values: b }) => {
                a.extend(b);
            }
            (Self::Distribution { sketch: sketch_a }, Self::Distribution { sketch: mut sketch_b }) => {
                sketch_a.merge(&mut sketch_b)
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

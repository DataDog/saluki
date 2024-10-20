use std::fmt;

use datadog_protos::metrics::{MetricPayload, MetricType, SketchPayload};
use ddsketch_agent::DDSketch;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

// A metric's unique identifier.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MetricContext {
    name: String,
    tags: Vec<String>,
}

impl MetricContext {
    fn normalize(&mut self) {
        self.tags.sort();
        self.tags.dedup();
    }

    /// Returns the name of the context.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tags of the context.
    pub fn tags(&self) -> &[String] {
        &self.tags
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
    /// Normalizes the metric data to ensure it is in a consistent state for analysis.
    ///
    /// This includes, but is not limited to, sorting the tags to match the behavior of the normal Datadog metrics
    /// intake, etc.
    pub fn normalize(&mut self) {
        self.context.normalize();

        // Sort values by timestamp, in ascending order, and then merge duplicates.
        self.values.sort_by(|a, b| a.0.cmp(&b.0));
        self.values.dedup_by(|a, b| {
            if a.0 == b.0 {
                // We merge `a` into `b` if the timestamps are equal, because `a` is the one that gets removed if they match.
                match (&mut a.1, &mut b.1) {
                    (MetricValue::Count { value: value_a }, MetricValue::Count { value: value_b }) => *value_b += *value_a,
                    (MetricValue::Rate { interval: interval_a, value: value_a }, MetricValue::Rate { interval: interval_b, value: value_b }) => {
                        assert_eq!(*interval_a, *interval_b, "Rate intervals should be identical.");
                        *value_b += *value_a;
                    },
                    (MetricValue::Gauge { value: value_a }, MetricValue::Gauge { value: value_b }) => *value_b = *value_a,
                    (MetricValue::Sketch { sketch: sketch_a }, MetricValue::Sketch { sketch: sketch_b }) => sketch_b.merge(sketch_a),
                    _ => panic!("Metric types should be identical."),
                }
                true
            } else {
                false
            }
        });
    }

    /// Returns the context of the metric.
    pub fn context(&self) -> &MetricContext {
        &self.context
    }

    /// Returns the values associated with the metric.
    pub fn values(&self) -> &[(u64, MetricValue)] {
        &self.values
    }

    /// Merges the values of another metric into this one.
    ///
    /// Values are normalized after merging.
    pub fn merge_values(&mut self, other: &Metric) {
        self.values.extend_from_slice(&other.values);
        self.normalize();
    }

    /// Anchors the timestamps of metric values.
    ///
    /// This will adjust the timestamps such that the first/lowest timestamp is 0, and all other timestamps are offset
    /// relative to that. For example, with two timestamps -- 10010 and 10030 -- the first timestamp of 10010 would
    /// become 0, and the second timestamp of 10030 would become 20.
    pub fn anchor_values(&mut self) {
        self.normalize();

        let base_offset = if let Some((first_timestamp, _)) = self.values.first() {
            *first_timestamp
        } else {
            return;
        };

        for (timestamp, _) in &mut self.values {
            *timestamp -= base_offset;
        }
    }
}

/// A metric value.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mtype")]
pub enum MetricValue {
    /// A count.
    Count { value: f64 },

    /// A rate.
    Rate { interval: u64, value: f64 },

    /// A gauge.
    Gauge { value: f64 },

    /// A sketch.
    Sketch { sketch: DDSketch },
}

impl PartialEq for MetricValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MetricValue::Count { value: value_a }, MetricValue::Count { value: value_b }) => value_a == value_b,
            (MetricValue::Rate { interval: interval_a, value: value_a }, MetricValue::Rate { interval: interval_b, value: value_b }) => {
                interval_a == interval_b && value_a == value_b
            },
            (MetricValue::Gauge { value: value_a }, MetricValue::Gauge { value: value_b }) => value_a == value_b,
            (MetricValue::Sketch { sketch: sketch_a }, MetricValue::Sketch { sketch: sketch_b }) => sketch_a == sketch_b,
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

            metrics.push(Metric { context: MetricContext { name, tags }, values })
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

            metrics.push(Metric { context: MetricContext { name, tags }, values })
        }

        Ok(metrics)
    }
}

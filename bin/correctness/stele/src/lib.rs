use datadog_protos::metrics::{MetricPayload, MetricType, SketchPayload};
use ddsketch_agent::DDSketch;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

/// A simplified metric representation.
#[derive(Clone, Deserialize, Serialize)]
pub struct Metric {
    name: String,
    tags: Vec<String>,
    values: Vec<(u64, MetricValue)>,
}

impl Metric {
    /// Returns the name of the metric.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tags associated with the metric.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Returns the values associated with the metric.
    pub fn values(&self) -> &[(u64, MetricValue)] {
        &self.values
    }
}

/// A metric value.
#[derive(Clone, Deserialize, Serialize)]
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

            metrics.push(Metric { name, tags, values })
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

            metrics.push(Metric { name, tags, values })
        }

        Ok(metrics)
    }
}

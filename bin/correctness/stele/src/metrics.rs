use std::fmt;

use datadog_protos::metrics::{MetricPayload, MetricType, SketchPayload};
use ddsketch::DDSketch;
use float_cmp::ApproxEqRatio as _;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

/// JSON envelope for the legacy V1 series intake (`/api/v1/series`).
#[derive(Deserialize)]
struct V1SeriesEnvelope {
    series: Vec<V1Serie>,
}

/// A single `Serie` entry in a V1 series payload.
///
/// Mirrors the Datadog Agent's wire format (see `pkg/metrics/series.go`). `omitempty` fields default to empty.
// Other fields present in the JSON envelope (`source_type_name`, `unit`) are intentionally not deserialized;
// serde silently ignores unknown JSON fields, so omitting them here is sufficient.
#[derive(Deserialize)]
struct V1Serie {
    metric: String,
    points: Vec<(i64, f64)>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    host: String,
    #[serde(default)]
    device: String,
    #[serde(rename = "type", default)]
    mtype: String,
    #[serde(default)]
    interval: i64,
}

/// A metric's unique identifier.
///
/// The host is normalized into the tag list as a `host:<value>` tag rather than carried as a separate field; the
/// Datadog backend treats host as a first-class dimension on the time series, equivalent to any other tag. This
/// keeps comparison logic uniform regardless of which wire format the metric originated from.
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
    ///
    /// When the underlying payload carried a host, it appears here as a `host:<value>` tag.
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
    /// Attempts to parse metrics from a series v1 payload.
    ///
    /// V1 keeps a separate `device` JSON field rather than a `device:<value>` tag like the V2 protobuf encoder. To
    /// keep the post-conversion `stele::Metric` representation comparable between V1 and V2 payloads, this re-injects
    /// `device:<value>` into the tag list when the JSON `device` field is non-empty.
    ///
    /// # Errors
    ///
    /// If the JSON cannot be deserialized, contains invalid data (for example, an unknown `type`), or has out-of-range
    /// timestamps, an error is returned.
    pub fn try_from_series_v1(payload: &[u8]) -> Result<Vec<Self>, GenericError> {
        let envelope: V1SeriesEnvelope = serde_json::from_slice(payload)
            .map_err(|e| generic_error!("Failed to parse V1 series JSON payload: {}", e))?;

        let mut metrics = Vec::with_capacity(envelope.series.len());

        for serie in envelope.series {
            let mut tags = serie.tags;
            if !serie.host.is_empty() {
                tags.push(format!("host:{}", serie.host));
            }
            if !serie.device.is_empty() {
                tags.push(format!("device:{}", serie.device));
            }

            let mut values = Vec::with_capacity(serie.points.len());
            for (ts, value) in serie.points {
                let timestamp =
                    u64::try_from(ts).map_err(|_| generic_error!("Invalid timestamp in V1 series payload: {}", ts))?;

                let metric_value = match serie.mtype.as_str() {
                    "count" => MetricValue::Count { value },
                    "rate" => MetricValue::Rate {
                        interval: serie.interval as u64,
                        value,
                    },
                    "gauge" => MetricValue::Gauge { value },
                    other => {
                        return Err(generic_error!(
                            "Unknown metric type '{}' in V1 series payload (metric '{}')",
                            other,
                            serie.metric
                        ));
                    }
                };
                values.push((timestamp, metric_value));
            }

            metrics.push(Metric {
                context: MetricContext {
                    name: serie.metric,
                    tags,
                },
                values,
            });
        }

        Ok(metrics)
    }

    /// Attempts to parse metrics from a series v2 payload.
    ///
    /// # Errors
    ///
    /// If the metric payload contains invalid data, an error will be returned.
    pub fn try_from_series_v2(payload: MetricPayload) -> Result<Vec<Self>, GenericError> {
        let mut metrics = Vec::new();

        for series in payload.series {
            let name = series.metric().to_string();
            let mut tags: Vec<String> = series.tags().iter().map(|tag| tag.to_string()).collect();
            // V2 protobuf encodes the hostname as a resource with type="host". The Datadog Agent's wire format
            // contract requires at least one such resource per series, but we still tolerate its absence. The host
            // is appended to the tag list (as `host:<value>`) to match the backend's representation and to keep
            // comparison logic uniform with the V1 JSON path.
            if let Some(host) = series.resources.iter().find(|r| r.type_() == "host") {
                let host_name = host.name();
                if !host_name.is_empty() {
                    tags.push(format!("host:{}", host_name));
                }
            }
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
            let mut tags: Vec<String> = sketch.tags().iter().map(|tag| tag.to_string()).collect();
            // V2 sketches carry the host on the sketch message itself rather than via a resources list. Fold it
            // into the tag list (as `host:<value>`) for comparison parity with the V1 JSON / V2 series paths.
            let host = sketch.host();
            if !host.is_empty() {
                tags.push(format!("host:{}", host));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_series_v1_parses_count_gauge_rate() {
        let body = br#"{"series":[
            {"metric":"a.count","points":[[100,5.0]],"tags":["env:prod"],"host":"h","type":"count","interval":0},
            {"metric":"a.gauge","points":[[101,12.0]],"tags":[],"host":"h","type":"gauge","interval":0},
            {"metric":"a.rate","points":[[102,3.0]],"tags":[],"host":"h","type":"rate","interval":10}
        ]}"#;

        let metrics = Metric::try_from_series_v1(body).expect("parse should succeed");
        assert_eq!(metrics.len(), 3);

        assert_eq!(metrics[0].context.name, "a.count");
        assert!(metrics[0].context.tags.contains(&"env:prod".to_string()));
        assert!(metrics[0].context.tags.contains(&"host:h".to_string()));
        assert_eq!(metrics[0].values, vec![(100, MetricValue::Count { value: 5.0 })]);

        assert_eq!(metrics[1].context.name, "a.gauge");
        assert_eq!(metrics[1].context.tags, vec!["host:h".to_string()]);
        assert_eq!(metrics[1].values, vec![(101, MetricValue::Gauge { value: 12.0 })]);

        assert_eq!(metrics[2].context.name, "a.rate");
        assert_eq!(metrics[2].context.tags, vec!["host:h".to_string()]);
        assert_eq!(
            metrics[2].values,
            vec![(
                102,
                MetricValue::Rate {
                    interval: 10,
                    value: 3.0
                }
            )]
        );
    }

    #[test]
    fn try_from_series_v1_omits_host_tag_when_empty() {
        // A payload without `host` is uncommon in practice but the parser must remain permissive — it omits the
        // `host:` tag rather than emitting an empty one.
        let body = br#"{"series":[
            {"metric":"m","points":[[1,1.0]],"tags":[],"type":"count","interval":0}
        ]}"#;

        let metrics = Metric::try_from_series_v1(body).expect("parse should succeed");
        assert!(!metrics[0].context.tags.iter().any(|t| t.starts_with("host:")));
    }

    #[test]
    fn try_from_series_v1_reinjects_device_tag() {
        let body = br#"{"series":[
            {"metric":"m","points":[[1,1.0]],"tags":["env:prod"],"host":"h","device":"eth0","type":"count","interval":0}
        ]}"#;

        let metrics = Metric::try_from_series_v1(body).expect("parse should succeed");
        assert!(metrics[0].context.tags.contains(&"env:prod".to_string()));
        assert!(metrics[0].context.tags.contains(&"device:eth0".to_string()));
    }

    #[test]
    fn try_from_series_v1_rejects_unknown_type() {
        let body = br#"{"series":[
            {"metric":"m","points":[[1,1.0]],"tags":[],"host":"h","type":"weird","interval":0}
        ]}"#;

        assert!(Metric::try_from_series_v1(body).is_err());
    }

    #[test]
    fn try_from_series_v2_folds_host_into_tags() {
        use datadog_protos::metrics::metric_payload::{
            MetricPoint, MetricSeries, MetricType as ProtoMetricType, Resource,
        };
        use datadog_protos::metrics::MetricPayload;

        let mut payload = MetricPayload::new();

        let mut series = MetricSeries::new();
        series.set_metric("my.metric".into());
        series.set_type(ProtoMetricType::COUNT);
        series.tags.push("env:prod".into());

        let mut host_res = Resource::new();
        host_res.set_type("host".into());
        host_res.set_name("server-1".into());
        series.resources.push(host_res);

        // Non-host resources must not be folded into tags.
        let mut device_res = Resource::new();
        device_res.set_type("device".into());
        device_res.set_name("eth0".into());
        series.resources.push(device_res);

        let mut point = MetricPoint::new();
        point.value = 1.0;
        point.timestamp = 1;
        series.points.push(point);

        payload.series.push(series);

        let metrics = Metric::try_from_series_v2(payload).expect("parse should succeed");
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].context.tags.contains(&"host:server-1".to_string()));
        assert!(metrics[0].context.tags.contains(&"env:prod".to_string()));
        assert!(!metrics[0].context.tags.iter().any(|t| t.starts_with("device:")));
    }

    #[test]
    fn try_from_sketch_folds_host_into_tags() {
        use datadog_protos::metrics::sketch_payload::Sketch;
        use datadog_protos::metrics::SketchPayload;

        let mut payload = SketchPayload::new();
        let mut sketch = Sketch::new();
        sketch.set_metric("my.metric".into());
        sketch.set_host("server-1".into());
        sketch.tags.push("env:prod".into());
        payload.sketches.push(sketch);

        let metrics = Metric::try_from_sketch(payload).expect("parse should succeed");
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].context.tags.contains(&"host:server-1".to_string()));
        assert!(metrics[0].context.tags.contains(&"env:prod".to_string()));
    }
}

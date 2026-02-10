use std::fmt;

use datadog_protos::metrics::v3::Payload as V3Payload;
use datadog_protos::metrics::{Dogsketch, MetricPayload, MetricType, SketchPayload};
use ddsketch::DDSketch;
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

// V3 metric type constants (from intake_v3.proto metricType enum).
const V3_METRIC_TYPE_COUNT: u64 = 1;
const V3_METRIC_TYPE_RATE: u64 = 2;
const V3_METRIC_TYPE_GAUGE: u64 = 3;
const V3_METRIC_TYPE_SKETCH: u64 = 4;

// V3 value type constants (from intake_v3.proto valueType enum).
const V3_VALUE_TYPE_ZERO: u64 = 0x00;
const V3_VALUE_TYPE_SINT64: u64 = 0x10;
const V3_VALUE_TYPE_FLOAT32: u64 = 0x20;
const V3_VALUE_TYPE_FLOAT64: u64 = 0x30;

/// Tracks cursors into the various value arrays of a v3 payload during decoding.
struct V3ValueCursors {
    timestamp: usize,
    sint64: usize,
    float32: usize,
    float64: usize,
    sketch_point: usize,
    sketch_bin_key: usize,
    sketch_bin_cnt: usize,
}

impl V3ValueCursors {
    fn new() -> Self {
        Self {
            timestamp: 0,
            sint64: 0,
            float32: 0,
            float64: 0,
            sketch_point: 0,
            sketch_bin_key: 0,
            sketch_bin_cnt: 0,
        }
    }
}

impl Metric {
    /// Attempts to parse metrics from a v3 payload.
    ///
    /// The v3 format uses columnar encoding with dictionary deduplication and delta encoding.
    ///
    /// # Errors
    ///
    /// If the payload contains invalid data, an error will be returned.
    pub fn try_from_v3(mut payload: V3Payload) -> Result<Vec<Self>, GenericError> {
        let data = payload
            .metricData
            .take()
            .ok_or_else(|| generic_error!("V3 payload missing metricData"))?;

        let num_metrics = data.types.len();
        if num_metrics == 0 {
            return Ok(Vec::new());
        }

        // Parse dictionaries.
        let names_dict = parse_dict_strings(&data.dictNameStr)?;
        let tags_dict = parse_dict_strings(&data.dictTagStr)?;
        let tagsets_dict = parse_tagsets(&data.dictTagsets, &tags_dict)?;

        // Delta-decode index arrays.
        let mut name_refs = data.nameRefs;
        let mut tagset_refs = data.tagsetRefs;
        let mut timestamps = data.timestamps;
        delta_decode(&mut name_refs);
        delta_decode(&mut tagset_refs);
        delta_decode(&mut timestamps);

        // Delta-decode sketch bin keys (per-sketch sequences are individually delta-encoded,
        // but we handle that during iteration).
        let mut sketch_bin_keys = data.sketchBinKeys;

        let mut cursors = V3ValueCursors::new();
        let mut metrics = Vec::with_capacity(num_metrics);

        for i in 0..num_metrics {
            let type_field = data.types[i];
            let metric_type = type_field & 0x0F;
            let value_type = type_field & 0xF0;
            let num_points = data.numPoints[i] as usize;

            // Resolve name (1-based index).
            let name_ref = name_refs[i] as usize;
            let name = if name_ref == 0 {
                String::new()
            } else {
                names_dict
                    .get(name_ref - 1)
                    .ok_or_else(|| generic_error!("Invalid name ref {} (dict size {})", name_ref, names_dict.len()))?
                    .clone()
            };

            // Resolve tags (1-based index).
            let tagset_ref = tagset_refs[i] as usize;
            let tags = if tagset_ref == 0 {
                Vec::new()
            } else {
                tagsets_dict
                    .get(tagset_ref - 1)
                    .ok_or_else(|| {
                        generic_error!("Invalid tagset ref {} (dict size {})", tagset_ref, tagsets_dict.len())
                    })?
                    .clone()
            };

            let mut values = Vec::with_capacity(num_points);

            if metric_type == V3_METRIC_TYPE_SKETCH {
                for _ in 0..num_points {
                    // Read timestamp.
                    let ts = *timestamps
                        .get(cursors.timestamp)
                        .ok_or_else(|| generic_error!("Ran out of timestamps"))?;
                    let timestamp = u64::try_from(ts).map_err(|_| generic_error!("Invalid timestamp: {}", ts))?;
                    cursors.timestamp += 1;

                    // Sketch count is always in valsSint64.
                    let cnt = *data
                        .valsSint64
                        .get(cursors.sint64)
                        .ok_or_else(|| generic_error!("Ran out of sint64 values for sketch count"))?;
                    cursors.sint64 += 1;

                    // Sum, min, max are stored as 3 consecutive values based on value_type.
                    let sum = read_value(
                        value_type,
                        &mut cursors,
                        &data.valsSint64,
                        &data.valsFloat32,
                        &data.valsFloat64,
                    )?;
                    let min = read_value(
                        value_type,
                        &mut cursors,
                        &data.valsSint64,
                        &data.valsFloat32,
                        &data.valsFloat64,
                    )?;
                    let max = read_value(
                        value_type,
                        &mut cursors,
                        &data.valsSint64,
                        &data.valsFloat32,
                        &data.valsFloat64,
                    )?;
                    let avg = if cnt != 0 { sum / cnt as f64 } else { 0.0 };

                    // Read bin data.
                    let num_bins = *data
                        .sketchNumBins
                        .get(cursors.sketch_point)
                        .ok_or_else(|| generic_error!("Ran out of sketchNumBins"))?
                        as usize;
                    cursors.sketch_point += 1;

                    let bin_key_start = cursors.sketch_bin_key;
                    let bin_key_end = bin_key_start + num_bins;
                    if bin_key_end > sketch_bin_keys.len() {
                        return Err(generic_error!("Ran out of sketch bin keys"));
                    }

                    // Delta-decode this sketch's bin keys.
                    delta_decode_i32(&mut sketch_bin_keys[bin_key_start..bin_key_end]);

                    let k: Vec<i32> = sketch_bin_keys[bin_key_start..bin_key_end].to_vec();
                    cursors.sketch_bin_key = bin_key_end;

                    let bin_cnt_start = cursors.sketch_bin_cnt;
                    let bin_cnt_end = bin_cnt_start + num_bins;
                    if bin_cnt_end > data.sketchBinCnts.len() {
                        return Err(generic_error!("Ran out of sketch bin counts"));
                    }
                    let n: Vec<u32> = data.sketchBinCnts[bin_cnt_start..bin_cnt_end].to_vec();
                    cursors.sketch_bin_cnt = bin_cnt_end;

                    // Build a Dogsketch proto and use the existing TryFrom conversion.
                    let mut dogsketch = Dogsketch::new();
                    dogsketch.ts = ts;
                    dogsketch.cnt = cnt;
                    dogsketch.min = min;
                    dogsketch.max = max;
                    dogsketch.avg = avg;
                    dogsketch.sum = sum;
                    dogsketch.set_k(k);
                    dogsketch.set_n(n);

                    let sketch = DDSketch::try_from(dogsketch)
                        .map_err(|e| generic_error!("Failed to convert v3 sketch to DDSketch: {}", e))?;
                    values.push((timestamp, MetricValue::Sketch { sketch }));
                }
            } else {
                for _ in 0..num_points {
                    // Read timestamp.
                    let ts = *timestamps
                        .get(cursors.timestamp)
                        .ok_or_else(|| generic_error!("Ran out of timestamps"))?;
                    let timestamp = u64::try_from(ts).map_err(|_| generic_error!("Invalid timestamp: {}", ts))?;
                    cursors.timestamp += 1;

                    // Read point value.
                    let value = read_value(
                        value_type,
                        &mut cursors,
                        &data.valsSint64,
                        &data.valsFloat32,
                        &data.valsFloat64,
                    )?;

                    let metric_value = match metric_type {
                        V3_METRIC_TYPE_COUNT => MetricValue::Count { value },
                        V3_METRIC_TYPE_RATE => MetricValue::Rate {
                            interval: data.intervals[i],
                            value,
                        },
                        V3_METRIC_TYPE_GAUGE => MetricValue::Gauge { value },
                        other => return Err(generic_error!("Unknown v3 metric type: {}", other)),
                    };

                    values.push((timestamp, metric_value));
                }
            }

            metrics.push(Metric {
                context: MetricContext { name, tags },
                values,
            });
        }

        Ok(metrics)
    }
}

/// Delta-decode in place: convert deltas to absolute values (prefix sum).
fn delta_decode(s: &mut [i64]) {
    for i in 1..s.len() {
        s[i] += s[i - 1];
    }
}

/// Delta-decode i32 values in place.
fn delta_decode_i32(s: &mut [i32]) {
    for i in 1..s.len() {
        s[i] += s[i - 1];
    }
}

/// Read a varint from a byte slice, returning `(value, bytes_consumed)`.
fn read_varint(data: &[u8]) -> Result<(u64, usize), GenericError> {
    let mut value: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return Err(generic_error!("Varint too large"));
        }
    }
    Err(generic_error!("Unexpected end of data reading varint"))
}

/// Parse varint-length-prefixed strings from a byte buffer.
fn parse_dict_strings(data: &[u8]) -> Result<Vec<String>, GenericError> {
    let mut strings = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let (len, varint_size) = read_varint(&data[offset..])?;
        offset += varint_size;
        let len = len as usize;
        if offset + len > data.len() {
            return Err(generic_error!("Dictionary string extends past end of buffer"));
        }
        let s = std::str::from_utf8(&data[offset..offset + len])
            .map_err(|e| generic_error!("Invalid UTF-8 in dictionary string: {}", e))?;
        strings.push(s.to_string());
        offset += len;
    }
    Ok(strings)
}

/// Parse tagsets from the dictTagsets array using the tag dictionary.
///
/// Each tagset in `dict_tagsets` is encoded as: length, then that many delta-encoded tag indices.
fn parse_tagsets(dict_tagsets: &[i64], tags_dict: &[String]) -> Result<Vec<Vec<String>>, GenericError> {
    let mut tagsets = Vec::new();
    let mut offset = 0;
    while offset < dict_tagsets.len() {
        let count = dict_tagsets[offset] as usize;
        offset += 1;
        if offset + count > dict_tagsets.len() {
            return Err(generic_error!("Tagset extends past end of dictTagsets array"));
        }

        // Delta-decode the tag indices within this tagset.
        let mut tag_indices: Vec<i64> = dict_tagsets[offset..offset + count].to_vec();
        delta_decode(&mut tag_indices);

        // Resolve tag indices (1-based) to tag strings.
        let mut tags = Vec::with_capacity(count);
        for &idx in &tag_indices {
            let idx = idx as usize;
            if idx == 0 {
                continue;
            }
            let tag = tags_dict
                .get(idx - 1)
                .ok_or_else(|| generic_error!("Invalid tag index {} (dict size {})", idx, tags_dict.len()))?;
            tags.push(tag.clone());
        }
        tagsets.push(tags);
        offset += count;
    }
    Ok(tagsets)
}

/// Read the next f64 value from the appropriate value array based on `value_type`.
fn read_value(
    value_type: u64, cursors: &mut V3ValueCursors, vals_sint64: &[i64], vals_float32: &[f32], vals_float64: &[f64],
) -> Result<f64, GenericError> {
    match value_type {
        V3_VALUE_TYPE_ZERO => Ok(0.0),
        V3_VALUE_TYPE_SINT64 => {
            let v = *vals_sint64
                .get(cursors.sint64)
                .ok_or_else(|| generic_error!("Ran out of sint64 values"))?;
            cursors.sint64 += 1;
            Ok(v as f64)
        }
        V3_VALUE_TYPE_FLOAT32 => {
            let v = *vals_float32
                .get(cursors.float32)
                .ok_or_else(|| generic_error!("Ran out of float32 values"))?;
            cursors.float32 += 1;
            Ok(v as f64)
        }
        V3_VALUE_TYPE_FLOAT64 => {
            let v = *vals_float64
                .get(cursors.float64)
                .ok_or_else(|| generic_error!("Ran out of float64 values"))?;
            cursors.float64 += 1;
            Ok(v)
        }
        _ => Err(generic_error!("Unknown v3 value type: {:#x}", value_type)),
    }
}

fn approx_eq_ratio_optional(a: Option<f64>, b: Option<f64>, ratio: f64) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.approx_eq_ratio(&b, ratio),
        (None, None) => true,
        _ => false,
    }
}

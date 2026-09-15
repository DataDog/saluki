//! Conversion of OTLP exponential histograms into the sketch representation the intake accepts.

use std::collections::BTreeMap;

use ::ddsketch::canonical::mapping::IndexMapping;
use datadog_protos::metrics::Dogsketch;
use ddsketch::canonical::mapping::LogarithmicMapping;
use ddsketch::canonical::store::DenseStore;
use ddsketch::canonical::DDSketch as CanonicalDDSketch;
use ddsketch::canonical::Store as DdStore;
use ddsketch::DDSketch;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    exponential_histogram_data_point::Buckets as OtlpExponentialHistogramBuckets,
    ExponentialHistogramDataPoint as OtlpExponentialHistogramDataPoint,
};
use saluki_error::{generic_error, GenericError};

fn to_store(b: Option<&OtlpExponentialHistogramBuckets>) -> DenseStore {
    let mut store = DenseStore::new();
    if let Some(buckets) = b {
        let offset = buckets.offset;
        let bucket_counts = &buckets.bucket_counts;

        for (j, &count) in bucket_counts.iter().enumerate() {
            let index = offset + j as i32;
            store.add(index, count);
        }
    }
    store
}

pub(super) fn exponential_histogram_to_ddsketch(
    dp: &OtlpExponentialHistogramDataPoint, delta: bool,
) -> Result<CanonicalDDSketch<LogarithmicMapping, DenseStore>, GenericError> {
    if !delta {
        return Err(generic_error!("cumulative exponential histograms are not supported"));
    }

    let positive_store = to_store(dp.positive.as_ref());
    let negative_store = to_store(dp.negative.as_ref());

    let gamma = 2_f64.powf(2_f64.powi(-dp.scale));
    let mapping = LogarithmicMapping::new_with_gamma(gamma)
        .map_err(|e| generic_error!("Failed to create index mapping for DDSketch: {}", e))?;

    let mut sketch = CanonicalDDSketch::new(mapping, positive_store, negative_store);
    sketch.add_n(0.0, dp.zero_count);

    Ok(sketch)
}

fn remap_store_bins_to_agent_space(
    source_mapping: &LogarithmicMapping, target_mapping: &LogarithmicMapping, store: &DenseStore, sign: i32,
    remapped_counts: &mut BTreeMap<i32, f64>, zeroes: &mut f64,
) -> Result<(), GenericError> {
    const MAX_AGENT_INDEX: i32 = i16::MAX as i32;

    for (index, count) in store.iter_non_zero_bins() {
        let count = count as f64;
        if count <= 0.0 {
            return Err(generic_error!("negative counts are not supported: got {}", count));
        }

        let input_lower_bound = source_mapping.lower_bound(index);
        let input_upper_bound = source_mapping.lower_bound(index + 1);
        let input_size = input_upper_bound - input_lower_bound;
        let mut output_index = target_mapping.index(input_lower_bound);

        while target_mapping.lower_bound(output_index) < input_upper_bound {
            if output_index >= MAX_AGENT_INDEX {
                return Err(generic_error!(
                    "index value {} exceeds the maximum supported index value ({})",
                    output_index,
                    MAX_AGENT_INDEX
                ));
            }

            let output_lower_bound = target_mapping.lower_bound(output_index);
            let output_upper_bound = target_mapping.lower_bound(output_index + 1);
            let lower_intersection_bound = output_lower_bound.max(input_lower_bound);
            let upper_intersection_bound = output_upper_bound.min(input_upper_bound);
            let intersection_size = upper_intersection_bound - lower_intersection_bound;

            if intersection_size > 0.0 {
                let remapped_count = count * (intersection_size / input_size);
                if output_index <= 0 {
                    *zeroes += remapped_count;
                } else {
                    *remapped_counts.entry(sign * output_index).or_default() += remapped_count;
                }
            }

            output_index += 1;
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RemappedSketchSummary {
    min: f64,
    max: f64,
    sum: f64,
}

fn sketch_value_for_key(target_mapping: &LogarithmicMapping, key: i32) -> f64 {
    if key == 0 {
        0.0
    } else if key < 0 {
        -target_mapping.value(-key)
    } else {
        target_mapping.value(key)
    }
}

fn summarize_remapped_counts(
    float_key_counts: &BTreeMap<i32, f64>, target_mapping: &LogarithmicMapping,
) -> Option<RemappedSketchSummary> {
    let mut summary: Option<RemappedSketchSummary> = None;

    for (&key, &count) in float_key_counts {
        if count <= 0.0 {
            continue;
        }

        let value = sketch_value_for_key(target_mapping, key);

        match &mut summary {
            Some(summary) => {
                summary.max = value;
                summary.sum += value * count;
            }
            None => {
                summary = Some(RemappedSketchSummary {
                    min: value,
                    max: value,
                    sum: value * count,
                });
            }
        }
    }

    summary
}

fn convert_float_counts_to_int_counts(
    float_key_counts: BTreeMap<i32, f64>,
) -> Result<(Vec<(i16, u64)>, u64), GenericError> {
    let mut key_counts = Vec::new();
    let mut float_total = 0.0;
    let mut int_total = 0_u64;

    for (key, count) in float_key_counts {
        let key = i16::try_from(key).map_err(|_| generic_error!("bin key {} overflows i16", key))?;
        float_total += count;

        let rounded_total = float_total.round() as u64;
        let rounded = rounded_total.saturating_sub(int_total);
        int_total += rounded;

        if rounded > 0 {
            key_counts.push((key, rounded));
        }
    }

    Ok((key_counts, int_total))
}

fn build_agent_sketch_from_key_counts(
    key_counts: &[(i16, u64)], total_count: u64, summary: Option<RemappedSketchSummary>,
) -> Result<DDSketch, GenericError> {
    let mut dogsketch = Dogsketch::new();
    dogsketch.set_cnt(i64::try_from(total_count).map_err(|_| generic_error!("sketch count overflows i64"))?);

    if total_count == 0 {
        return DDSketch::try_from(dogsketch).map_err(|e| generic_error!("failed to build sketch: {}", e));
    }

    let mut k = Vec::new();
    let mut n = Vec::new();

    for &(key, count) in key_counts {
        let mut remaining = count;
        while remaining > u64::from(u16::MAX) {
            k.push(i32::from(key));
            n.push(u32::from(u16::MAX));
            remaining -= u64::from(u16::MAX);
        }

        if remaining > 0 {
            k.push(i32::from(key));
            n.push(remaining as u32);
        }
    }

    let summary = summary.ok_or_else(|| generic_error!("missing sketch summary for non-empty sketch"))?;

    dogsketch.set_min(summary.min);
    dogsketch.set_max(summary.max);
    dogsketch.set_sum(summary.sum);
    dogsketch.set_avg(summary.sum / total_count as f64);
    dogsketch.set_k(k);
    dogsketch.set_n(n);

    DDSketch::try_from(dogsketch).map_err(|e| generic_error!("failed to build sketch: {}", e))
}

pub(super) fn convert_ddsketch_into_sketch(
    canonical: CanonicalDDSketch<LogarithmicMapping, DenseStore>,
) -> Result<DDSketch, GenericError> {
    let target_mapping = DDSketch::remap_mapping();
    let source_mapping = canonical.mapping();
    let mut remapped_counts = BTreeMap::new();
    let mut zeroes = canonical.zero_count() as f64;

    remap_store_bins_to_agent_space(
        source_mapping,
        &target_mapping,
        canonical.positive_store(),
        1,
        &mut remapped_counts,
        &mut zeroes,
    )?;
    remap_store_bins_to_agent_space(
        source_mapping,
        &target_mapping,
        canonical.negative_store(),
        -1,
        &mut remapped_counts,
        &mut zeroes,
    )?;

    if zeroes != 0.0 {
        *remapped_counts.entry(0).or_default() += zeroes;
    }

    let summary = summarize_remapped_counts(&remapped_counts, &target_mapping);
    let (key_counts, total_count) = convert_float_counts_to_int_counts(remapped_counts)?;
    build_agent_sketch_from_key_counts(&key_counts, total_count, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remapped_summary_uses_target_mapping_values_for_signed_keys() {
        let target_mapping = DDSketch::remap_mapping();
        let mut remapped_counts = BTreeMap::new();
        remapped_counts.insert(-10, 1.5);
        remapped_counts.insert(0, 2.0);
        remapped_counts.insert(12, 3.0);

        let summary = summarize_remapped_counts(&remapped_counts, &target_mapping).expect("summary should exist");
        let expected_min = -target_mapping.value(10);
        let expected_max = target_mapping.value(12);
        let expected_sum = expected_min * 1.5 + expected_max * 3.0;

        assert_eq!(summary.min, expected_min);
        assert_eq!(summary.max, expected_max);
        assert!((summary.sum - expected_sum).abs() < f64::EPSILON);
    }

    #[test]
    fn ddsketch_conversion_splits_coarse_bins_across_multiple_agent_bins() {
        let source_mapping = LogarithmicMapping::new_with_gamma(2.0).expect("source mapping should be valid");
        let mut positive_store = DenseStore::new();
        positive_store.add(0, 256);

        let canonical = CanonicalDDSketch::new(source_mapping, positive_store, DenseStore::new());
        let sketch = convert_ddsketch_into_sketch(canonical).expect("conversion should succeed");

        assert_eq!(sketch.count(), 256);
        assert!(sketch.bin_count() > 1);
        assert!(sketch.min().unwrap() < sketch.max().unwrap());
    }
}

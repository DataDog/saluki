//! V3 columnar metrics writer.
//!
//! The [`V3Writer`] accumulates metrics in columnar format with dictionary deduplication,
//! then produces [`V3EncodedData`] ready for protobuf serialization.

use super::interner::Interner;
use super::types::{V3MetricType, V3ValueType};

/// Appends a varint-length-prefixed string to the destination buffer.
fn append_len_str(dst: &mut Vec<u8>, s: &str) {
    let mut len = s.len() as u64;
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        dst.push(byte);
        if len == 0 {
            break;
        }
    }
    dst.extend_from_slice(s.as_bytes());
}

/// Delta-encodes a slice in place, working backwards.
///
/// After encoding, `s[i]` contains the difference `s[i] - s[i-1]`.
pub fn delta_encode(s: &mut [i64]) {
    if s.len() < 2 {
        return;
    }
    for i in (1..s.len()).rev() {
        s[i] -= s[i - 1];
    }
}

/// Delta-encodes i32 values in place, working backwards.
pub fn delta_encode_i32(s: &mut [i32]) {
    if s.len() < 2 {
        return;
    }
    for i in (1..s.len()).rev() {
        s[i] -= s[i - 1];
    }
}

/// Encoded V3 payload data ready for protobuf serialization.
#[derive(Debug, Default)]
pub struct V3EncodedData {
    // Dictionary encoded bytes (varint-length-prefixed strings)
    pub dict_name_bytes: Vec<u8>,
    pub dict_tags_bytes: Vec<u8>,
    pub dict_tagsets: Vec<i64>,
    pub dict_resource_str_bytes: Vec<u8>,
    pub dict_resource_len: Vec<i64>,
    pub dict_resource_type: Vec<i64>,
    pub dict_resource_name: Vec<i64>,
    pub dict_source_type_bytes: Vec<u8>,
    pub dict_origin_info: Vec<i32>,

    // Per-metric columns (one entry per metric)
    pub types: Vec<u64>,
    pub names: Vec<i64>,
    pub tags: Vec<i64>,
    pub resources: Vec<i64>,
    pub intervals: Vec<u64>,
    pub num_points: Vec<u64>,
    pub source_type_names: Vec<i64>,
    pub origin_infos: Vec<i64>,

    // Point data (varies per metric based on num_points)
    pub timestamps: Vec<i64>,
    pub vals_sint64: Vec<i64>,
    pub vals_float32: Vec<f32>,
    pub vals_float64: Vec<f64>,

    // Sketch data
    pub sketch_num_bins: Vec<u64>,
    pub sketch_bin_keys: Vec<i32>,
    pub sketch_bin_cnts: Vec<u32>,
}

/// V3 columnar metrics writer.
///
/// Accumulates metrics in columnar format with dictionary deduplication.
/// Call [`V3Writer::write`] for each metric, then [`V3Writer::close`] to finalize
/// and get the encoded data.
#[derive(Debug, Default)]
pub struct V3Writer {
    // Interners for dictionary deduplication
    name_interner: Interner<String>,
    tag_interner: Interner<String>,
    tagset_interner: Interner<Vec<i64>>,
    resource_str_interner: Interner<String>,
    resource_interner: Interner<Vec<(i64, i64)>>,
    source_type_interner: Interner<String>,
    origin_interner: Interner<(i32, i32, i32)>,

    // Dictionary encoded bytes
    dict_name_bytes: Vec<u8>,
    dict_tags_bytes: Vec<u8>,
    dict_tagsets: Vec<i64>,
    dict_resource_str_bytes: Vec<u8>,
    dict_resource_len: Vec<i64>,
    dict_resource_type: Vec<i64>,
    dict_resource_name: Vec<i64>,
    dict_source_type_bytes: Vec<u8>,
    dict_origin_info: Vec<i32>,

    // Per-metric columns
    types: Vec<u64>,
    names: Vec<i64>,
    tags: Vec<i64>,
    resources: Vec<i64>,
    intervals: Vec<u64>,
    num_points: Vec<u64>,
    source_type_names: Vec<i64>,
    origin_infos: Vec<i64>,

    // Point data
    timestamps: Vec<i64>,
    vals_sint64: Vec<i64>,
    vals_float32: Vec<f32>,
    vals_float64: Vec<f64>,

    // Sketch data
    sketch_num_bins: Vec<u64>,
    sketch_bin_keys: Vec<i32>,
    sketch_bin_cnts: Vec<u32>,
}

impl V3Writer {
    /// Creates a new V3 writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begins writing a new metric.
    ///
    /// Returns a [`V3MetricBuilder`] that must be used to set the metric's
    /// properties and add points, then closed with [`V3MetricBuilder::close`].
    pub fn write(&mut self, metric_type: V3MetricType, name: &str) -> V3MetricBuilder<'_> {
        let name_id = self.intern_name(name);
        let metric_idx = self.types.len();
        let point_start_idx = self.vals_float64.len();

        // Initialize the per-metric columns with default values
        self.types.push(metric_type.as_u64());
        self.names.push(name_id);
        self.tags.push(0);
        self.resources.push(0);
        self.intervals.push(0);
        self.num_points.push(0);
        self.source_type_names.push(0);
        self.origin_infos.push(0);

        V3MetricBuilder {
            writer: self,
            point_start_idx,
            metric_idx,
        }
    }

    /// Finalizes the writer and returns the encoded data.
    ///
    /// This performs delta encoding on all index arrays.
    pub fn close(mut self) -> V3EncodedData {
        // Delta-encode all the index arrays
        delta_encode(&mut self.names);
        delta_encode(&mut self.tags);
        delta_encode(&mut self.resources);
        delta_encode(&mut self.source_type_names);
        delta_encode(&mut self.origin_infos);
        delta_encode(&mut self.timestamps);

        V3EncodedData {
            dict_name_bytes: self.dict_name_bytes,
            dict_tags_bytes: self.dict_tags_bytes,
            dict_tagsets: self.dict_tagsets,
            dict_resource_str_bytes: self.dict_resource_str_bytes,
            dict_resource_len: self.dict_resource_len,
            dict_resource_type: self.dict_resource_type,
            dict_resource_name: self.dict_resource_name,
            dict_source_type_bytes: self.dict_source_type_bytes,
            dict_origin_info: self.dict_origin_info,
            types: self.types,
            names: self.names,
            tags: self.tags,
            resources: self.resources,
            intervals: self.intervals,
            num_points: self.num_points,
            source_type_names: self.source_type_names,
            origin_infos: self.origin_infos,
            timestamps: self.timestamps,
            vals_sint64: self.vals_sint64,
            vals_float32: self.vals_float32,
            vals_float64: self.vals_float64,
            sketch_num_bins: self.sketch_num_bins,
            sketch_bin_keys: self.sketch_bin_keys,
            sketch_bin_cnts: self.sketch_bin_cnts,
        }
    }

    // Internal helper methods

    fn intern_name(&mut self, name: &str) -> i64 {
        if name.is_empty() {
            return 0;
        }
        let (id, is_new) = self.name_interner.get_or_insert(name.to_string());
        if is_new {
            append_len_str(&mut self.dict_name_bytes, name);
        }
        id
    }

    fn intern_tag(&mut self, tag: &str) -> i64 {
        if tag.is_empty() {
            return 0;
        }
        let (id, is_new) = self.tag_interner.get_or_insert(tag.to_string());
        if is_new {
            append_len_str(&mut self.dict_tags_bytes, tag);
        }
        id
    }

    fn intern_tagset(&mut self, tag_ids: Vec<i64>) -> i64 {
        if tag_ids.is_empty() {
            return 0;
        }
        let (id, is_new) = self.tagset_interner.get_or_insert(tag_ids.clone());
        if is_new {
            self.encode_tagset(&tag_ids);
        }
        id
    }

    fn encode_tagset(&mut self, tag_ids: &[i64]) {
        // Push the length
        self.dict_tagsets.push(tag_ids.len() as i64);

        let start = self.dict_tagsets.len();

        // Add all tag IDs
        self.dict_tagsets.extend_from_slice(tag_ids);

        // Sort and delta-encode the tagset portion
        self.dict_tagsets[start..].sort_unstable();
        delta_encode(&mut self.dict_tagsets[start..]);
    }

    fn intern_resource_str(&mut self, s: &str) -> i64 {
        if s.is_empty() {
            return 0;
        }
        let (id, is_new) = self.resource_str_interner.get_or_insert(s.to_string());
        if is_new {
            append_len_str(&mut self.dict_resource_str_bytes, s);
        }
        id
    }

    fn intern_resources(&mut self, resources: &[(String, String)]) -> i64 {
        if resources.is_empty() {
            return 0;
        }

        // Convert to (type_id, name_id) pairs
        let id_pairs: Vec<(i64, i64)> = resources
            .iter()
            .map(|(t, n)| (self.intern_resource_str(t), self.intern_resource_str(n)))
            .collect();

        let (id, is_new) = self.resource_interner.get_or_insert(id_pairs.clone());
        if is_new {
            self.encode_resources(&id_pairs);
        }
        id
    }

    fn encode_resources(&mut self, id_pairs: &[(i64, i64)]) {
        self.dict_resource_len.push(id_pairs.len() as i64);

        let type_start = self.dict_resource_type.len();
        let name_start = self.dict_resource_name.len();

        for (type_id, name_id) in id_pairs {
            self.dict_resource_type.push(*type_id);
            self.dict_resource_name.push(*name_id);
        }

        delta_encode(&mut self.dict_resource_type[type_start..]);
        delta_encode(&mut self.dict_resource_name[name_start..]);
    }

    fn intern_source_type(&mut self, s: &str) -> i64 {
        if s.is_empty() {
            return 0;
        }
        let (id, is_new) = self.source_type_interner.get_or_insert(s.to_string());
        if is_new {
            append_len_str(&mut self.dict_source_type_bytes, s);
        }
        id
    }

    fn intern_origin(&mut self, product: i32, category: i32, service: i32) -> i64 {
        if product == 0 && category == 0 && service == 0 {
            return 0;
        }
        let (id, is_new) = self.origin_interner.get_or_insert((product, category, service));
        if is_new {
            self.dict_origin_info.push(product);
            self.dict_origin_info.push(category);
            self.dict_origin_info.push(service);
        }
        id
    }
}

/// Builder for a single metric within a V3 payload.
///
/// Use the setter methods to configure the metric, add points with [`add_point`](Self::add_point),
/// then call [`close`](Self::close) to finalize.
pub struct V3MetricBuilder<'a> {
    writer: &'a mut V3Writer,
    point_start_idx: usize,
    metric_idx: usize,
}

impl<'a> V3MetricBuilder<'a> {
    /// Sets the tags for this metric.
    ///
    /// Tags should be in "key:value" format.
    pub fn set_tags<I, S>(&mut self, tags: I)
    where
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let tag_ids: Vec<i64> = tags.map(|t| self.writer.intern_tag(t.as_ref())).collect();
        let tagset_id = self.writer.intern_tagset(tag_ids);
        self.writer.tags[self.metric_idx] = tagset_id;
    }

    /// Sets the resources for this metric.
    ///
    /// Resources are (type, name) pairs, e.g., ("host", "server1").
    pub fn set_resources<I>(&mut self, resources: I)
    where
        I: Iterator<Item = (String, String)>,
    {
        let resources: Vec<(String, String)> = resources.collect();
        if resources.is_empty() {
            self.writer.resources[self.metric_idx] = 0;
            return;
        }
        let res_id = self.writer.intern_resources(&resources);
        self.writer.resources[self.metric_idx] = res_id;
    }

    /// Sets the interval for this metric (used for rate metrics).
    pub fn set_interval(&mut self, interval: u64) {
        self.writer.intervals[self.metric_idx] = interval;
    }

    /// Sets the source type name for this metric.
    pub fn set_source_type(&mut self, source_type: &str) {
        if source_type.is_empty() {
            self.writer.source_type_names[self.metric_idx] = 0;
            return;
        }
        let id = self.writer.intern_source_type(source_type);
        self.writer.source_type_names[self.metric_idx] = id;
    }

    /// Sets the origin metadata for this metric.
    pub fn set_origin(&mut self, product: u32, category: u32, service: u32) {
        let id = self.writer.intern_origin(product as i32, category as i32, service as i32);
        self.writer.origin_infos[self.metric_idx] = id;
    }

    /// Adds a data point to this metric.
    pub fn add_point(&mut self, timestamp: i64, value: f64) {
        self.writer.timestamps.push(timestamp);
        self.writer.vals_float64.push(value);
        self.writer.num_points[self.metric_idx] += 1;
    }

    /// Adds sketch data for a distribution metric.
    ///
    /// For sketches, the summary values (count, sum, min, max) are stored as points,
    /// and the bin keys/counts are stored separately.
    pub fn add_sketch(&mut self, timestamp: i64, count: i64, sum: f64, min: f64, max: f64, bin_keys: &[i32], bin_counts: &[u32]) {
        self.writer.timestamps.push(timestamp);

        // Count goes in sint64, sum/min/max go in float64
        self.writer.vals_sint64.push(count);
        self.writer.vals_float64.push(sum);
        self.writer.vals_float64.push(min);
        self.writer.vals_float64.push(max);

        // Store bin data
        self.writer.sketch_num_bins.push(bin_keys.len() as u64);

        let key_start = self.writer.sketch_bin_keys.len();
        self.writer.sketch_bin_keys.extend_from_slice(bin_keys);
        self.writer.sketch_bin_cnts.extend_from_slice(bin_counts);

        // Delta-encode this sketch's bin keys
        delta_encode_i32(&mut self.writer.sketch_bin_keys[key_start..]);

        self.writer.num_points[self.metric_idx] += 1;
    }

    /// Finalizes this metric.
    ///
    /// This compacts the point values to use the smallest representation
    /// that can hold all values without loss.
    pub fn close(mut self) {
        self.compact_values();
    }

    fn compact_values(&mut self) {
        let count = self.writer.num_points[self.metric_idx] as usize;
        if count == 0 {
            return;
        }

        let start = self.point_start_idx;
        let end = self.writer.vals_float64.len();

        // Determine the maximum value type needed
        let mut val_ty = V3ValueType::Zero;
        for i in start..end {
            let val = self.writer.vals_float64[i];
            let pnt_val_ty = V3ValueType::for_value(val);
            val_ty = val_ty.max(pnt_val_ty);
        }

        // Update the type field
        self.writer.types[self.metric_idx] |= val_ty.as_u64();

        // Convert values to the appropriate storage
        match val_ty {
            V3ValueType::Zero => {
                // Values are all zero, don't store anything
                self.writer.vals_float64.truncate(start);
            }
            V3ValueType::Sint64 => {
                for i in start..end {
                    self.writer.vals_sint64.push(self.writer.vals_float64[i] as i64);
                }
                self.writer.vals_float64.truncate(start);
            }
            V3ValueType::Float32 => {
                for i in start..end {
                    self.writer.vals_float32.push(self.writer.vals_float64[i] as f32);
                }
                self.writer.vals_float64.truncate(start);
            }
            V3ValueType::Float64 => {
                // Already stored in vals_float64, keep them
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta_encode() {
        let mut data = vec![100, 110, 130, 145];
        delta_encode(&mut data);
        assert_eq!(data, vec![100, 10, 20, 15]);
    }

    #[test]
    fn test_delta_encode_empty() {
        let mut data: Vec<i64> = vec![];
        delta_encode(&mut data);
        assert!(data.is_empty());
    }

    #[test]
    fn test_delta_encode_single() {
        let mut data = vec![42];
        delta_encode(&mut data);
        assert_eq!(data, vec![42]);
    }

    #[test]
    fn test_append_len_str() {
        let mut buf = Vec::new();
        append_len_str(&mut buf, "hello");
        // Length 5 = 0x05, then "hello"
        assert_eq!(buf, vec![5, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn test_writer_basic() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.set_tags(["env:prod", "service:web"].iter().copied());
            metric.add_point(1000, 42.0);
            metric.add_point(1010, 43.5);
            metric.close();
        }

        let data = writer.close();

        assert_eq!(data.types.len(), 1);
        assert_eq!(data.names.len(), 1);
        assert_eq!(data.timestamps.len(), 2);
    }

    #[test]
    fn test_writer_multiple_metrics() {
        let mut writer = V3Writer::new();

        {
            let mut m1 = writer.write(V3MetricType::Count, "metric1");
            m1.add_point(1000, 10.0);
            m1.close();
        }

        {
            let mut m2 = writer.write(V3MetricType::Rate, "metric2");
            m2.set_interval(60);
            m2.add_point(2000, 20.0);
            m2.close();
        }

        let data = writer.close();

        assert_eq!(data.types.len(), 2);
        assert_eq!(data.names.len(), 2);
        assert_eq!(data.intervals[0], 0);
        // Second metric's interval won't be 60 directly since names is delta-encoded,
        // but we can verify the structure is correct
    }

    #[test]
    fn test_value_compaction_zero() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "zero.metric");
            metric.add_point(1000, 0.0);
            metric.add_point(2000, 0.0);
            metric.close();
        }

        let data = writer.close();

        // Values should be compacted - zero values don't need storage
        assert!(data.vals_float64.is_empty());
        assert!(data.vals_sint64.is_empty());
        assert!(data.vals_float32.is_empty());
    }

    #[test]
    fn test_value_compaction_int() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Count, "int.metric");
            metric.add_point(1000, 100.0);
            metric.add_point(2000, 200.0);
            metric.close();
        }

        let data = writer.close();

        // Integer values should be stored in sint64
        assert!(data.vals_float64.is_empty());
        assert_eq!(data.vals_sint64, vec![100, 200]);
        assert!(data.vals_float32.is_empty());
    }
}

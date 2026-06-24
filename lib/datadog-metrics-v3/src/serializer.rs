//! V3 payload protobuf serialization using raw wire-format encoding.
//!
//! No external dependencies — implements the protobuf wire format subset needed
//! by the V3 MetricData message directly.

use super::writer::V3EncodedData;

// ── Protobuf wire types ──────────────────────────────────────────────────────

const WIRE_LEN: u32 = 2;

// ── Field numbers (from payload_v3.proto) ────────────────────────────────────

mod field {
    pub const DICT_NAME_STR: u32 = 1;
    pub const DICT_TAGS_STR: u32 = 2;
    pub const DICT_TAGSETS: u32 = 3;
    pub const DICT_RESOURCE_STR: u32 = 4;
    pub const DICT_RESOURCE_LEN: u32 = 5;
    pub const DICT_RESOURCE_TYPE: u32 = 6;
    pub const DICT_RESOURCE_NAME: u32 = 7;
    pub const DICT_SOURCE_TYPE_NAME: u32 = 8;
    pub const DICT_ORIGIN_INFO: u32 = 9;
    pub const TYPES: u32 = 10;
    pub const NAMES: u32 = 11;
    pub const TAGS: u32 = 12;
    pub const RESOURCES: u32 = 13;
    pub const INTERVALS: u32 = 14;
    pub const NUM_POINTS: u32 = 15;
    pub const TIMESTAMPS: u32 = 16;
    pub const VALS_SINT64: u32 = 17;
    pub const VALS_FLOAT32: u32 = 18;
    pub const VALS_FLOAT64: u32 = 19;
    pub const SKETCH_NUM_BINS: u32 = 20;
    pub const SKETCH_BIN_KEYS: u32 = 21;
    pub const SKETCH_BIN_CNTS: u32 = 22;
    pub const SOURCE_TYPE_NAME: u32 = 23;
    pub const ORIGIN_INFO: u32 = 24;
}

// ── Varint primitives ────────────────────────────────────────────────────────

fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn varint_len(mut value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    let mut n = 0;
    while value > 0 {
        value >>= 7;
        n += 1;
    }
    n
}

#[inline]
fn zigzag64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn zigzag32(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

fn write_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    write_varint(buf, ((field as u64) << 3) | (wire as u64));
}

// ── Field writers ────────────────────────────────────────────────────────────

/// Writes a `bytes` field (wire type 2). No-op if data is empty.
fn write_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Writes a packed repeated `sint64` field (zigzag-encoded varints).
fn write_packed_sint64(buf: &mut Vec<u8>, field: u32, values: &[i64]) {
    if values.is_empty() {
        return;
    }
    let size: usize = values.iter().map(|&v| varint_len(zigzag64(v))).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, zigzag64(v));
    }
}

/// Writes a packed repeated `int64` field (standard unsigned varint, no zigzag).
fn write_packed_int64(buf: &mut Vec<u8>, field: u32, values: &[i64]) {
    if values.is_empty() {
        return;
    }
    let size: usize = values.iter().map(|&v| varint_len(v as u64)).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, v as u64);
    }
}

/// Writes a packed repeated `uint64` field.
fn write_packed_uint64(buf: &mut Vec<u8>, field: u32, values: &[u64]) {
    if values.is_empty() {
        return;
    }
    let size: usize = values.iter().map(|&v| varint_len(v)).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, v);
    }
}

/// Writes a packed repeated `int32` field (sign-extended to unsigned varint).
fn write_packed_int32(buf: &mut Vec<u8>, field: u32, values: &[i32]) {
    if values.is_empty() {
        return;
    }
    // int32 wire format: negative values sign-extend to 10 bytes; cast to u64.
    let size: usize = values.iter().map(|&v| varint_len(v as i64 as u64)).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, v as i64 as u64);
    }
}

/// Writes a packed repeated `sint32` field (zigzag-encoded).
fn write_packed_sint32(buf: &mut Vec<u8>, field: u32, values: &[i32]) {
    if values.is_empty() {
        return;
    }
    let size: usize = values.iter().map(|&v| varint_len(zigzag32(v) as u64)).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, zigzag32(v) as u64);
    }
}

/// Writes a packed repeated `uint32` field.
fn write_packed_uint32(buf: &mut Vec<u8>, field: u32, values: &[u32]) {
    if values.is_empty() {
        return;
    }
    let size: usize = values.iter().map(|&v| varint_len(v as u64)).sum();
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, size as u64);
    for &v in values {
        write_varint(buf, v as u64);
    }
}

/// Writes a packed repeated `float` field — 4 bytes little-endian each.
fn write_packed_float(buf: &mut Vec<u8>, field: u32, values: &[f32]) {
    if values.is_empty() {
        return;
    }
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, (values.len() * 4) as u64);
    for &v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Writes a packed repeated `double` field — 8 bytes little-endian each.
fn write_packed_double(buf: &mut Vec<u8>, field: u32, values: &[f64]) {
    if values.is_empty() {
        return;
    }
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, (values.len() * 8) as u64);
    for &v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Serializes [`V3EncodedData`] to protobuf wire format.
///
/// The output conforms to the `MetricData` message in `payload_v3.proto`.
/// Fields are written in ascending field-number order as the spec requires.
pub fn serialize_v3_payload(data: &V3EncodedData, output: &mut Vec<u8>) {
    // Dictionary fields
    write_bytes_field(output, field::DICT_NAME_STR, &data.dict_name_bytes);
    write_bytes_field(output, field::DICT_TAGS_STR, &data.dict_tags_bytes);
    write_packed_sint64(output, field::DICT_TAGSETS, &data.dict_tagsets);
    write_bytes_field(output, field::DICT_RESOURCE_STR, &data.dict_resource_str_bytes);
    write_packed_int64(output, field::DICT_RESOURCE_LEN, &data.dict_resource_len);
    write_packed_sint64(output, field::DICT_RESOURCE_TYPE, &data.dict_resource_type);
    write_packed_sint64(output, field::DICT_RESOURCE_NAME, &data.dict_resource_name);
    write_bytes_field(output, field::DICT_SOURCE_TYPE_NAME, &data.dict_source_type_bytes);
    write_packed_int32(output, field::DICT_ORIGIN_INFO, &data.dict_origin_info);
    // Per-metric columns
    write_packed_uint64(output, field::TYPES, &data.types);
    write_packed_sint64(output, field::NAMES, &data.names);
    write_packed_sint64(output, field::TAGS, &data.tags);
    write_packed_sint64(output, field::RESOURCES, &data.resources);
    write_packed_uint64(output, field::INTERVALS, &data.intervals);
    write_packed_uint64(output, field::NUM_POINTS, &data.num_points);
    // Point data
    write_packed_sint64(output, field::TIMESTAMPS, &data.timestamps);
    write_packed_sint64(output, field::VALS_SINT64, &data.vals_sint64);
    write_packed_float(output, field::VALS_FLOAT32, &data.vals_float32);
    write_packed_double(output, field::VALS_FLOAT64, &data.vals_float64);
    // Sketch data
    write_packed_uint64(output, field::SKETCH_NUM_BINS, &data.sketch_num_bins);
    write_packed_sint32(output, field::SKETCH_BIN_KEYS, &data.sketch_bin_keys);
    write_packed_uint32(output, field::SKETCH_BIN_CNTS, &data.sketch_bin_cnts);
    // Additional per-metric columns (higher field numbers, written after point data)
    write_packed_sint64(output, field::SOURCE_TYPE_NAME, &data.source_type_names);
    write_packed_sint64(output, field::ORIGIN_INFO, &data.origin_infos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{V3MetricType, V3Writer};

    #[test]
    fn test_serialize_empty_is_empty() {
        let data = V3EncodedData::default();
        let mut output = Vec::new();
        serialize_v3_payload(&data, &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn test_serialize_basic_gauge() {
        let mut writer = V3Writer::new();
        let mut m = writer.write(V3MetricType::Gauge, "test.metric");
        m.add_point(1000, 42.0);
        m.close();
        let data = writer.close();
        let mut output = Vec::new();
        serialize_v3_payload(&data, &mut output);
        assert!(!output.is_empty());
        // Field 10 (TYPES) tag = (10 << 3) | 2 = 82 = 0x52
        assert!(output.contains(&0x52));
    }

    #[test]
    fn test_varint_encoding() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 0);
        assert_eq!(buf, [0x00]);
        buf.clear();
        write_varint(&mut buf, 127);
        assert_eq!(buf, [0x7f]);
        buf.clear();
        write_varint(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);
        buf.clear();
        write_varint(&mut buf, 300);
        assert_eq!(buf, [0xac, 0x02]);
    }

    #[test]
    fn test_zigzag64() {
        assert_eq!(zigzag64(0), 0);
        assert_eq!(zigzag64(-1), 1);
        assert_eq!(zigzag64(1), 2);
        assert_eq!(zigzag64(-2), 3);
        assert_eq!(zigzag64(2147483647), 4294967294);
        assert_eq!(zigzag64(-2147483648), 4294967295);
    }

    #[test]
    fn test_zigzag32() {
        assert_eq!(zigzag32(0), 0);
        assert_eq!(zigzag32(-1), 1);
        assert_eq!(zigzag32(1), 2);
        assert_eq!(zigzag32(-2), 3);
    }

    #[test]
    fn test_packed_sint64_roundtrip_length() {
        let values: Vec<i64> = vec![0, 1, -1, 100, -100, i64::MAX / 2];
        let mut buf = Vec::new();
        write_packed_sint64(&mut buf, 1, &values);
        // Tag (1 << 3 | 2 = 0x0a) + length varint + encoded values
        assert!(buf.len() > 2);
        assert_eq!(buf[0], 0x0a); // field 1, wire type 2
    }

    #[test]
    fn test_float_little_endian() {
        let values = vec![1.0f32];
        let mut buf = Vec::new();
        write_packed_float(&mut buf, 1, &values);
        // 1.0f32 = 0x3f800000; LE bytes = [0x00, 0x00, 0x80, 0x3f]
        let payload = &buf[2..]; // skip tag + length
        assert_eq!(payload, &[0x00, 0x00, 0x80, 0x3f]);
    }

    #[test]
    fn test_double_little_endian() {
        let values = vec![1.0f64];
        let mut buf = Vec::new();
        write_packed_double(&mut buf, 1, &values);
        let payload = &buf[2..]; // skip tag + length
        assert_eq!(payload, &1.0f64.to_le_bytes()[..]);
    }
}

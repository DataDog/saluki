//! V3 payload protobuf serialization.
//!
//! Serializes [`V3EncodedData`] to protobuf wire format using `CodedOutputStream`.

use protobuf::{rt::WireType, CodedOutputStream};

use super::types::field_numbers;
use super::writer::V3EncodedData;

/// Serializes V3 encoded data to protobuf wire format.
///
/// The output is a MetricData message as defined in `payload_v3.proto`.
pub fn serialize_v3_payload(data: &V3EncodedData, output: &mut Vec<u8>) -> Result<(), protobuf::Error> {
    let mut os = CodedOutputStream::vec(output);

    // Dictionary fields (bytes - varint-length-prefixed strings concatenated)
    if !data.dict_name_bytes.is_empty() {
        os.write_bytes(field_numbers::DICT_NAME_STR, &data.dict_name_bytes)?;
    }
    if !data.dict_tags_bytes.is_empty() {
        os.write_bytes(field_numbers::DICT_TAGS_STR, &data.dict_tags_bytes)?;
    }

    // Packed repeated fields for dictionaries
    write_packed_sint64(&mut os, field_numbers::DICT_TAGSETS, &data.dict_tagsets)?;

    if !data.dict_resource_str_bytes.is_empty() {
        os.write_bytes(field_numbers::DICT_RESOURCE_STR, &data.dict_resource_str_bytes)?;
    }

    write_packed_int64(&mut os, field_numbers::DICT_RESOURCE_LEN, &data.dict_resource_len)?;
    write_packed_sint64(&mut os, field_numbers::DICT_RESOURCE_TYPE, &data.dict_resource_type)?;
    write_packed_sint64(&mut os, field_numbers::DICT_RESOURCE_NAME, &data.dict_resource_name)?;

    if !data.dict_source_type_bytes.is_empty() {
        os.write_bytes(field_numbers::DICT_SOURCE_TYPE_NAME, &data.dict_source_type_bytes)?;
    }

    write_packed_int32(&mut os, field_numbers::DICT_ORIGIN_INFO, &data.dict_origin_info)?;

    // Per-metric columns
    write_packed_uint64(&mut os, field_numbers::TYPES, &data.types)?;
    write_packed_sint64(&mut os, field_numbers::NAMES, &data.names)?;
    write_packed_sint64(&mut os, field_numbers::TAGS, &data.tags)?;
    write_packed_sint64(&mut os, field_numbers::RESOURCES, &data.resources)?;
    write_packed_uint64(&mut os, field_numbers::INTERVALS, &data.intervals)?;
    write_packed_uint64(&mut os, field_numbers::NUM_POINTS, &data.num_points)?;
    write_packed_sint64(&mut os, field_numbers::SOURCE_TYPE_NAME, &data.source_type_names)?;
    write_packed_sint64(&mut os, field_numbers::ORIGIN_INFO, &data.origin_infos)?;

    // Point data
    write_packed_sint64(&mut os, field_numbers::TIMESTAMPS, &data.timestamps)?;
    write_packed_sint64(&mut os, field_numbers::VALS_SINT64, &data.vals_sint64)?;
    write_packed_float(&mut os, field_numbers::VALS_FLOAT32, &data.vals_float32)?;
    write_packed_double(&mut os, field_numbers::VALS_FLOAT64, &data.vals_float64)?;

    // Sketch data
    write_packed_uint64(&mut os, field_numbers::SKETCH_NUM_BINS, &data.sketch_num_bins)?;
    write_packed_sint32(&mut os, field_numbers::SKETCH_BIN_KEYS, &data.sketch_bin_keys)?;
    write_packed_uint32(&mut os, field_numbers::SKETCH_BIN_CNTS, &data.sketch_bin_cnts)?;

    os.flush()?;
    Ok(())
}

/// Writes a packed repeated sint64 field.
fn write_packed_sint64(os: &mut CodedOutputStream, field: u32, values: &[i64]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    // Calculate the encoded size
    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_sint64(v);
    }

    // Write the field header and length
    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    // Write the values
    for &v in values {
        os.write_sint64_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated int64 field.
fn write_packed_int64(os: &mut CodedOutputStream, field: u32, values: &[i64]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_varint64(v as u64);
    }

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_int64_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated uint64 field.
fn write_packed_uint64(os: &mut CodedOutputStream, field: u32, values: &[u64]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_varint64(v);
    }

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_uint64_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated int32 field.
fn write_packed_int32(os: &mut CodedOutputStream, field: u32, values: &[i32]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_varint32(v as u32);
    }

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_int32_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated sint32 field.
fn write_packed_sint32(os: &mut CodedOutputStream, field: u32, values: &[i32]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_sint32(v);
    }

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_sint32_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated uint32 field.
fn write_packed_uint32(os: &mut CodedOutputStream, field: u32, values: &[u32]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    let mut size: usize = 0;
    for &v in values {
        size += encoded_len_varint32(v);
    }

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_uint32_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated float field.
fn write_packed_float(os: &mut CodedOutputStream, field: u32, values: &[f32]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    // Each float is 4 bytes
    let size = values.len() * 4;

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_float_no_tag(v)?;
    }

    Ok(())
}

/// Writes a packed repeated double field.
fn write_packed_double(os: &mut CodedOutputStream, field: u32, values: &[f64]) -> Result<(), protobuf::Error> {
    if values.is_empty() {
        return Ok(());
    }

    // Each double is 8 bytes
    let size = values.len() * 8;

    os.write_tag(field, WireType::LengthDelimited)?;
    os.write_raw_varint32(size as u32)?;

    for &v in values {
        os.write_double_no_tag(v)?;
    }

    Ok(())
}

// Varint encoding length helpers

fn encoded_len_varint64(value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    let bits = 64 - value.leading_zeros() as usize;
    (bits + 6) / 7
}

fn encoded_len_varint32(value: u32) -> usize {
    if value == 0 {
        return 1;
    }
    let bits = 32 - value.leading_zeros() as usize;
    (bits + 6) / 7
}

fn encoded_len_sint64(value: i64) -> usize {
    // Zigzag encoding: (value << 1) ^ (value >> 63)
    let encoded = ((value << 1) ^ (value >> 63)) as u64;
    encoded_len_varint64(encoded)
}

fn encoded_len_sint32(value: i32) -> usize {
    // Zigzag encoding: (value << 1) ^ (value >> 31)
    let encoded = ((value << 1) ^ (value >> 31)) as u32;
    encoded_len_varint32(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoders::datadog::metrics::v3::{V3Writer, V3MetricType};

    #[test]
    fn test_serialize_empty() {
        let data = V3EncodedData::default();
        let mut output = Vec::new();
        serialize_v3_payload(&data, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn test_serialize_basic_metric() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.add_point(1000, 42.0);
            metric.close();
        }

        let data = writer.close();
        let mut output = Vec::new();
        serialize_v3_payload(&data, &mut output).unwrap();

        // Should produce non-empty output
        assert!(!output.is_empty());
    }

    #[test]
    fn test_encoded_len_varint() {
        assert_eq!(encoded_len_varint64(0), 1);
        assert_eq!(encoded_len_varint64(1), 1);
        assert_eq!(encoded_len_varint64(127), 1);
        assert_eq!(encoded_len_varint64(128), 2);
        assert_eq!(encoded_len_varint64(16383), 2);
        assert_eq!(encoded_len_varint64(16384), 3);
    }

    #[test]
    fn test_encoded_len_sint() {
        // Zigzag: 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, 2 -> 4, ...
        assert_eq!(encoded_len_sint64(0), 1);
        assert_eq!(encoded_len_sint64(-1), 1);
        assert_eq!(encoded_len_sint64(1), 1);
        assert_eq!(encoded_len_sint64(-64), 1);
        assert_eq!(encoded_len_sint64(63), 1);
        assert_eq!(encoded_len_sint64(64), 2);
        assert_eq!(encoded_len_sint64(-65), 2);
    }
}

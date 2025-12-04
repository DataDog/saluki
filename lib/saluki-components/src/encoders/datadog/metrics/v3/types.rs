//! V3 payload type definitions and protocol buffer field numbers.

/// Protocol buffer field numbers for MetricData message.
///
/// These correspond to the field numbers in `payload_v3.proto`.
pub mod field_numbers {
    // Dictionary fields
    pub const DICT_NAME_STR: u32 = 1;
    pub const DICT_TAGS_STR: u32 = 2;
    pub const DICT_TAGSETS: u32 = 3;
    pub const DICT_RESOURCE_STR: u32 = 4;
    pub const DICT_RESOURCE_LEN: u32 = 5;
    pub const DICT_RESOURCE_TYPE: u32 = 6;
    pub const DICT_RESOURCE_NAME: u32 = 7;
    pub const DICT_SOURCE_TYPE_NAME: u32 = 8;
    pub const DICT_ORIGIN_INFO: u32 = 9;

    // Per-metric columns
    pub const TYPES: u32 = 10;
    pub const NAMES: u32 = 11;
    pub const TAGS: u32 = 12;
    pub const RESOURCES: u32 = 13;
    pub const INTERVALS: u32 = 14;
    pub const NUM_POINTS: u32 = 15;

    // Point data
    pub const TIMESTAMPS: u32 = 16;
    pub const VALS_SINT64: u32 = 17;
    pub const VALS_FLOAT32: u32 = 18;
    pub const VALS_FLOAT64: u32 = 19;

    // Sketch data
    pub const SKETCH_NUM_BINS: u32 = 20;
    pub const SKETCH_BIN_KEYS: u32 = 21;
    pub const SKETCH_BIN_CNTS: u32 = 22;

    // Additional per-metric columns
    pub const SOURCE_TYPE_NAME: u32 = 23;
    pub const ORIGIN_INFO: u32 = 24;
}

/// V3 metric type values.
///
/// These match the `metricType` enum in `payload_v3.proto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum V3MetricType {
    Count = 1,
    Rate = 2,
    Gauge = 3,
    Sketch = 4,
}

impl V3MetricType {
    /// Returns the numeric value for encoding in the types column.
    pub fn as_u64(self) -> u64 {
        self as u64
    }
}

/// V3 value type values.
///
/// These are encoded in bits 4-7 of the types column and indicate which
/// value array contains the metric's points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum V3ValueType {
    /// Value is zero, not stored explicitly.
    Zero = 0x00,
    /// Value is stored in vals_sint64.
    Sint64 = 0x10,
    /// Value is stored in vals_float32.
    Float32 = 0x20,
    /// Value is stored in vals_float64.
    Float64 = 0x30,
}

impl V3ValueType {
    /// Returns the numeric value for encoding in the types column.
    pub fn as_u64(self) -> u64 {
        self as u64
    }

    /// Determines the best value type for a given f64 value.
    ///
    /// Prefers smaller representations when lossless:
    /// - Zero for 0.0
    /// - Sint64 for integers that fit in 49 bits
    /// - Float32 for values representable as f32
    /// - Float64 otherwise
    pub fn for_value(v: f64) -> Self {
        if v == 0.0 {
            return Self::Zero;
        }

        // Varint range that fits in 7 bytes or less (49 bits)
        const VARINT_WIDTH: i32 = 7 * 7 - 1;
        const MAX_INT: i64 = 1 << VARINT_WIDTH;
        const MIN_INT: i64 = -MAX_INT;

        let i = v as i64;
        if (MIN_INT..MAX_INT).contains(&i) && (i as f64) == v {
            return Self::Sint64;
        }

        if (v as f32 as f64) == v {
            return Self::Float32;
        }

        Self::Float64
    }

    /// Returns the maximum (largest encoding) of two value types.
    pub fn max(self, other: Self) -> Self {
        if (other as u8) > (self as u8) {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_type_for_value() {
        assert_eq!(V3ValueType::for_value(0.0), V3ValueType::Zero);
        assert_eq!(V3ValueType::for_value(100.0), V3ValueType::Sint64);
        assert_eq!(V3ValueType::for_value(-100.0), V3ValueType::Sint64);
        assert_eq!(V3ValueType::for_value(1.5), V3ValueType::Float32);
        assert_eq!(V3ValueType::for_value(2.75), V3ValueType::Float32);

        // Large integers that don't fit in 49 bits AND can't be exactly represented in f32
        // Powers of 2 like (1 << 50) can be exactly represented in f32, so we add 1
        // to make it an odd number that requires more precision than f32 provides
        let large = ((1i64 << 50) + 1) as f64;
        assert_eq!(V3ValueType::for_value(large), V3ValueType::Float64);

        // Values that require f64 precision - use PI which has more precision than f32 can hold
        // and isn't an integer, so it won't be stored as Sint64
        let pi = std::f64::consts::PI;
        // PI requires full f64 precision to store exactly
        assert_eq!(V3ValueType::for_value(pi), V3ValueType::Float64);
    }

    #[test]
    fn test_value_type_max() {
        assert_eq!(V3ValueType::Zero.max(V3ValueType::Sint64), V3ValueType::Sint64);
        assert_eq!(V3ValueType::Sint64.max(V3ValueType::Float32), V3ValueType::Float32);
        assert_eq!(V3ValueType::Float32.max(V3ValueType::Float64), V3ValueType::Float64);
        assert_eq!(V3ValueType::Float64.max(V3ValueType::Zero), V3ValueType::Float64);
    }
}

use http::HeaderValue;

pub const SERIES_V2_COMPRESSED_SIZE_LIMIT: usize = 512_000; // 500 KiB
pub const SERIES_V2_UNCOMPRESSED_SIZE_LIMIT: usize = 5_242_880; // 5 MiB

// Protocol Buffers field numbers for series and sketch payload messages in the V2 format.
//
// These field numbers come from the Protocol Buffers definitions in `lib/datadog-protos/proto/agent_payload.proto`.
pub const RESOURCES_TYPE_FIELD_NUMBER: u32 = 1;
pub const RESOURCES_NAME_FIELD_NUMBER: u32 = 2;

pub const METADATA_ORIGIN_FIELD_NUMBER: u32 = 1;

pub const ORIGIN_ORIGIN_PRODUCT_FIELD_NUMBER: u32 = 4;
pub const ORIGIN_ORIGIN_CATEGORY_FIELD_NUMBER: u32 = 5;
pub const ORIGIN_ORIGIN_SERVICE_FIELD_NUMBER: u32 = 6;

pub const METRIC_POINT_VALUE_FIELD_NUMBER: u32 = 1;
pub const METRIC_POINT_TIMESTAMP_FIELD_NUMBER: u32 = 2;

pub const DOGSKETCH_TS_FIELD_NUMBER: u32 = 1;
pub const DOGSKETCH_CNT_FIELD_NUMBER: u32 = 2;
pub const DOGSKETCH_MIN_FIELD_NUMBER: u32 = 3;
pub const DOGSKETCH_MAX_FIELD_NUMBER: u32 = 4;
pub const DOGSKETCH_AVG_FIELD_NUMBER: u32 = 5;
pub const DOGSKETCH_SUM_FIELD_NUMBER: u32 = 6;
pub const DOGSKETCH_K_FIELD_NUMBER: u32 = 7;
pub const DOGSKETCH_N_FIELD_NUMBER: u32 = 8;

pub const SERIES_RESOURCES_FIELD_NUMBER: u32 = 1;
pub const SERIES_METRIC_FIELD_NUMBER: u32 = 2;
pub const SERIES_TAGS_FIELD_NUMBER: u32 = 3;
pub const SERIES_POINTS_FIELD_NUMBER: u32 = 4;
pub const SERIES_TYPE_FIELD_NUMBER: u32 = 5;
pub const SERIES_SOURCE_TYPE_NAME_FIELD_NUMBER: u32 = 7;
pub const SERIES_INTERVAL_FIELD_NUMBER: u32 = 8;
pub const SERIES_METADATA_FIELD_NUMBER: u32 = 9;

pub const SKETCH_METRIC_FIELD_NUMBER: u32 = 1;
pub const SKETCH_HOST_FIELD_NUMBER: u32 = 2;
pub const SKETCH_TAGS_FIELD_NUMBER: u32 = 4;
pub const SKETCH_DOGSKETCHES_FIELD_NUMBER: u32 = 7;
pub const SKETCH_METADATA_FIELD_NUMBER: u32 = 8;

pub static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

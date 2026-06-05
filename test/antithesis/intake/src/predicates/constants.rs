//! Spec-derived constants the W predicates depend on.
//!
//! Sourced from the `invariant-jig` `README.md` §Properties.Payloads and from
//! the Agent's published intake limits.

/// Host name byte-length cap (W19).
pub const MAX_HOST_NAME_BYTES: usize = 255;

/// Metric name byte-length cap (W10).
pub const MAX_METRIC_NAME_BYTES: usize = 350;

/// Origin product ordinal upper bound (W16).
pub const ORIGIN_PRODUCT_MAX: u32 = 45;

/// Origin category ordinal upper bound (W16).
pub const ORIGIN_CATEGORY_MAX: u32 = 87;

/// Origin category reserved ordinals (W16).
pub const ORIGIN_CATEGORY_RESERVED: [u32; 2] = [1, 13];

/// Origin service ordinal upper bound (W16).
pub const ORIGIN_SERVICE_MAX: u32 = 519;

/// Origin service reserved ordinals (W16).
pub const ORIGIN_SERVICE_RESERVED: [u32; 8] = [8, 31, 32, 33, 46, 88, 123, 159];

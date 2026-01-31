//! Histogram bucket representation for interpolation.

/// A histogram bucket.
///
/// This type can be used to represent a classic histogram bucket, such as those defined by Prometheus or OpenTelemetry,
/// in a way that is then able to be inserted into the DDSketch via linear interpolation.
pub struct Bucket {
    /// The upper limit (inclusive) of values in the bucket.
    pub upper_limit: f64,

    /// The number of values in the bucket.
    pub count: u64,
}

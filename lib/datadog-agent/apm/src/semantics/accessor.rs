//! Storage-independent access to typed attributes.

/// An abstract view over a bag of typed attributes.
pub trait Accessor {
    /// Returns a string attribute, without converting other types.
    fn get_string(&self, key: &str) -> Option<&str>;
    /// Returns an integer attribute, without parsing strings.
    fn get_int64(&self, key: &str) -> Option<i64>;
    /// Returns a floating-point attribute, without parsing strings.
    fn get_float64(&self, key: &str) -> Option<f64>;
}

//! Timing utilities for realistic span durations.

use rand::Rng;

/// Generator for realistic span timing.
pub struct TimingGenerator {
    /// Base duration in nanoseconds.
    base_ns: u64,
}

impl TimingGenerator {
    /// Creates a new timing generator with the given base duration in nanoseconds.
    pub fn new(base_ns: u64) -> Self {
        Self { base_ns }
    }

    /// Generates a random duration that varies around the base.
    ///
    /// Returns a duration in nanoseconds that is between 50% and 200% of the base duration.
    pub fn generate_duration_ns(&self, rng: &mut impl Rng) -> u64 {
        // Add some variance: 50% to 200% of base
        let factor: f64 = rng.random_range(0.5..2.0);
        (self.base_ns as f64 * factor) as u64
    }

    /// Generates a random offset for child spans (how long before the child starts).
    ///
    /// Returns an offset in nanoseconds, typically a small fraction of the parent duration.
    pub fn generate_child_offset_ns(&self, rng: &mut impl Rng, parent_duration_ns: u64) -> u64 {
        // Child starts somewhere in the first 30% of the parent
        let max_offset = (parent_duration_ns as f64 * 0.3) as u64;
        if max_offset == 0 {
            0
        } else {
            rng.random_range(0..max_offset)
        }
    }
}

/// Converts nanoseconds to milliseconds.
pub fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

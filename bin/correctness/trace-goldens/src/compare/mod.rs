//! Semantic comparison of actual output against the golden outputs.
//!
//! The golden outputs pin what the Go trace-agent sent for each case, but several parts of that output legitimately
//! vary between runs and implementations: stats bucket boundaries depend on the wall clock, string table refs depend
//! on map iteration order, and span starts depend on the test clock. The comparators here reduce both sides to a
//! canonical, semantic form before comparing:
//!
//! - [`StatsSummary`] aggregates every stats group across payloads and buckets, keyed by its dimensions, so values may
//!   land in different buckets as long as the totals and tags match.
//! - [`TraceSummary`] resolves every string table ref, normalizes span starts relative to the payload build clock,
//!   and compares chunks as an unordered multiset.
//! - [`compare_wire`] checks the requests sent to each intake path, combining both comparators with pinned headers.
//!
//! This module is independent of [`crate::cases`]: the only thing the trace comparator needs from the input side is
//! the clock the payload was built against.

use std::fmt;

mod protojson;

mod stats;
pub use self::stats::{compare_stats, GroupKey, StatsSummary, Totals};

mod traces;
pub use self::traces::{compare_traces, normalize_start, TraceSummary};

mod wire;
pub use self::wire::{
    compare_wire, CapturedPayload, CapturedRequest, WireGolden, WireGoldenRequest, PINNED_HEADERS, STATS_PATH,
    TRACES_PATH,
};

/// Options that control how strictly actual output is compared against the golden files.
#[derive(Clone, Debug, PartialEq)]
pub struct CompareOptions {
    /// Whether to ignore the agent version reported in stats and trace payloads.
    ///
    /// The golden files record an empty agent version, while a real build reports its own version. Defaults to `true`.
    pub ignore_agent_version: bool,

    /// Rounding applied to span start offsets, in nanoseconds, before comparison.
    ///
    /// This absorbs the small jitter between the payload build clock and the clock of the system under test. It
    /// **SHOULD** match the rounding used when the golden files were recorded. Defaults to 100 milliseconds.
    pub start_rounding_ns: i64,

    /// Maximum relative difference allowed between quantiles of an expected and actual latency sketch.
    ///
    /// Sketch counts must always match exactly; this tolerance only applies to the min, max, p50, p95, and p99
    /// values, and absorbs encoding differences between sketch implementations. Defaults to `0.02` (2%).
    pub sketch_relative_tolerance: f64,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            ignore_agent_version: true,
            start_rounding_ns: 100_000_000,
            sketch_relative_tolerance: 0.02,
        }
    }
}

/// A single difference between the expected and the actual output.
#[derive(Clone, Debug, PartialEq)]
pub struct Mismatch {
    /// Where the difference was found, for example a stats group or a span field.
    pub path: String,

    /// The expected value, rendered for humans.
    pub expected: String,

    /// The actual value, rendered for humans.
    pub actual: String,
}

impl Mismatch {
    /// Creates a new mismatch.
    pub fn new(path: impl Into<String>, expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: expected {}, got {}", self.path, self.expected, self.actual)
    }
}

/// Every difference found by a comparison.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mismatches(pub Vec<Mismatch>);

impl Mismatches {
    /// Returns `Ok(())` if there are no mismatches, or `Err(self)` otherwise.
    pub fn into_result(self) -> Result<(), Mismatches> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }

    pub(crate) fn push(&mut self, path: impl Into<String>, expected: impl Into<String>, actual: impl Into<String>) {
        self.0.push(Mismatch::new(path, expected, actual));
    }

    /// Records a mismatch if `expected` and `actual` differ.
    pub(crate) fn check<T: PartialEq + fmt::Debug>(&mut self, path: impl fmt::Display, expected: &T, actual: &T) {
        if expected != actual {
            self.push(path.to_string(), format!("{:?}", expected), format!("{:?}", actual));
        }
    }

    pub(crate) fn extend_prefixed(&mut self, prefix: &str, other: Mismatches) {
        for m in other.0 {
            self.0
                .push(Mismatch::new(format!("{}: {}", prefix, m.path), m.expected, m.actual));
        }
    }
}

impl fmt::Display for Mismatches {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} mismatch(es):", self.0.len())?;
        for m in &self.0 {
            writeln!(f, "  - {}", m)?;
        }
        Ok(())
    }
}

impl std::error::Error for Mismatches {}

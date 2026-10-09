//! Metric processing semantics, extracted once and shared by every component that needs them.
//!
//! A kernel here is the single implementation of a piece of metric behavior: the legacy components that originally
//! owned the behavior call into this crate, and so do plan-driven execution and its reference interpreter. Nothing in
//! this crate reads configuration, performs I/O, or awaits; callers pass resolved parameters in.
//!
//! Kernels take their parameters as the plain types of [`params`], never as a host's configuration types, and where a
//! host validates its configuration by a kernel's rules, it calls the kernel's validation rather than keeping its own.
//!
//! - [`aggregate`]: native aggregation of metrics into time buckets.
//! - [`host_enrichment`]: host enrichment, which leaves metrics unchanged, and host tags, which adds the host's tags
//!   to metrics until a deadline.
//! - [`intake_name`]: the name a metric is stored under at the intake, which filterlists and allowlists compare
//!   against.
//! - [`matcher`]: the sorted string-set matcher behind every name and value list.
//! - [`mapper`]: DogStatsD name mapping, which renames metrics and adds tags by mapping profile.
//! - [`params`]: parameter types for kernels.
//! - [`prefix`]: namespace prefixing, which puts a namespace in front of metric names.
//! - [`selection`]: the name sets that gateways and post-aggregation filtering select metrics by.
//! - [`tag_projection`]: tag projection, which removes tags and restricts tag values of counters and distributions.
//! - [`timestamp_split`]: timestamp splitting, which routes timestamped points around aggregation.
//!
//! Kernels whose logic was restructured when it moved here keep a frozen copy of the code they replaced next to their
//! tests, and property tests compare the two, so the comparison stays meaningful once the legacy component calls the
//! kernel. With the `test-util` feature, the `testing` module provides property-testing strategies for metrics.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod aggregate;
pub mod host_enrichment;
pub mod intake_name;
pub mod mapper;
pub mod matcher;
pub mod params;
pub mod prefix;
pub mod selection;
pub mod tag_projection;
pub mod timestamp_split;

#[cfg(any(test, feature = "test-util"))]
pub mod testing;

#[cfg(test)]
mod test_support;

//! Metric processing semantics, extracted once and shared by every component that needs them.
//!
//! A kernel here is the single implementation of a piece of metric behavior: the legacy components that originally
//! owned the behavior call into this crate, and so do plan-driven execution and its reference interpreter. Nothing in
//! this crate reads configuration, performs I/O, or awaits; callers pass resolved parameters in.
//!
//! Behavior moves here one piece at a time. Until a piece's kernel is extracted, the legacy component keeps its own
//! code, and where this crate already defines the piece's parameters and their validation (see [`params`]), a
//! differential test in the host keeps the two in agreement.
//!
//! - [`intake_name`]: the name a metric is stored under at the intake, which filterlists and allowlists compare
//!   against.
//! - [`matcher`]: the sorted string-set matcher behind every name and value list.
//! - [`params`]: parameter types for kernels.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod intake_name;
pub mod matcher;
pub mod params;

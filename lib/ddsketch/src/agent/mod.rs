//! Agent-specific DDSketch implementation.
//!
//! This implementation matches the Datadog Agent's DDSketch for wire-compatibility.
//! Configuration is fixed at compile time for optimal performance.

mod bin;
mod bucket;
mod config;
mod sketch;

pub use bin::Bin;
pub use bucket::Bucket;
pub use sketch::DDSketch;

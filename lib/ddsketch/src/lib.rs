//! DDSketch implementations for quantile estimation.
//!
//! This crate provides two DDSketch implementations:
//!
//! - [`agent`]: Optimized for the Datadog Agent, with fixed compile-time configuration.
//!   Use this when you need wire compatibility with the Datadog Agent or maximum
//!   insertion performance.
//!
//! - [`canonical`]: Full implementation mirroring the official `sketches-go` library.
//!   Use this when you need runtime-configurable accuracy or different store types.
//!
//! # Quick Start
//!
//! For most use cases, the agent implementation (re-exported at the crate root) is
//! sufficient:
//!
//! ```
//! use ddsketch::DDSketch;
//!
//! let mut sketch = DDSketch::default();
//! sketch.insert(1.0);
//! sketch.insert(2.0);
//! sketch.insert(3.0);
//!
//! let median = sketch.quantile(0.5);
//! ```
//!
//! For the canonical implementation with configurable accuracy:
//!
//! ```
//! use ddsketch::canonical::DDSketch;
//!
//! let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
//! sketch.add(1.0);
//! sketch.add(2.0);
//! sketch.add(3.0);
//!
//! let median = sketch.quantile(0.5);
//! ```
//!
//! # Feature Flags
//!
//! - `serde`: Enables serialization/deserialization for the agent implementation.
//!   **Warning**: The serialization format is not guaranteed to be stable.

#![deny(warnings)]
#![deny(missing_docs)]

pub mod agent;
pub mod canonical;

mod common;

// Re-export the agent implementation at the crate root for backward compatibility.
// This allows `ddsketch::DDSketch` to work the same as the old `ddsketch_agent::DDSketch`.
pub use agent::{Bin, Bucket, DDSketch};

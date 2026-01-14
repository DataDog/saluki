//! Canonical implementation of DDSketch.
//!
//! This module provides a DDSketch implementation that closely follows the official Datadog implementations of
//! DDSketch, with configurable storage and index mappings.
//!
//! # Quick Start
//!
//! ```
//! use ddsketch::canonical::DDSketch;
//!
//! // Create a sketch with 1% relative accuracy
//! let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
//!
//! // Add some values
//! sketch.add(1.5);
//! sketch.add(2.5);
//! sketch.add(3.5);
//!
//! // Query quantiles
//! let p50 = sketch.quantile(0.5);
//! let p99 = sketch.quantile(0.99);
//! ```
//!
//! # Store Types
//!
//! The canonical implementation supports multiple store types:
//!
//! - [`CollapsingLowestDenseStore`]: Collapses lowest bins when limit is reached. Best for when higher quantiles (p95,
//!   p99) matter most.
//! - [`CollapsingHighestDenseStore`]: Collapses highest bins when limit is reached. Best for when lower quantiles (p1,
//!   p5) matter most.
//! - [`DenseStore`]: Unbounded dense storage. Best when memory is not a concern.
//! - [`SparseStore`]: Hash-based storage. Best for widely scattered values.

mod error;
pub use self::error::ProtoConversionError;

pub mod mapping;
pub use self::mapping::IndexMapping;

pub mod store;
pub use self::store::Store;

mod sketch;
pub use self::sketch::DDSketch;

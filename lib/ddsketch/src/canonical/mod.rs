//! Canonical DDSketch implementation mirroring `sketches-go`.
//!
//! This module provides a DDSketch implementation that closely follows the official
//! Datadog `sketches-go` library, with runtime-configurable accuracy and multiple
//! store types.
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
//! - [`CollapsingLowestDenseStore`]: Collapses lowest bins when limit is reached.
//!   Best for when higher quantiles (p95, p99) matter most.
//! - [`CollapsingHighestDenseStore`]: Collapses highest bins when limit is reached.
//!   Best for when lower quantiles (p1, p5) matter most.
//! - [`DenseStore`]: Unbounded dense storage. Best when memory is not a concern.
//! - [`SparseStore`]: Hash-based storage. Best for widely scattered values.
//!
//! # Comparison with Agent Implementation
//!
//! | Feature | Agent | Canonical |
//! |---------|-------|-----------|
//! | Configuration | Compile-time fixed | Runtime configurable |
//! | Store types | Single fixed | Multiple options |
//! | Negative values | Single bin array | Separate stores |
//! | Zero handling | Mapped to bin 0 | Explicit zero count |
//! | Wire compatibility | Datadog Agent | sketches-go |

pub mod mapping;
pub mod store;

mod sketch;

pub use mapping::{IndexMapping, LogarithmicMapping};
pub use sketch::DDSketch;
pub use store::{CollapsingHighestDenseStore, CollapsingLowestDenseStore, DenseStore, SparseStore, Store};

//! Detector implementations and the shared numerical utilities they build on.
//!
//! Anomaly detectors are ported one at a time from the Agent's Go `observer/impl` package. Everything that
//! more than one detector needs lives in [`numerics`] so the individual detector modules stay focused on
//! their algorithm instead of re-deriving medians, ranks, or tail probabilities.

pub mod numerics;

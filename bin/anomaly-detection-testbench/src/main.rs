//! Offline replay testbench for the anomaly detection core.
//!
//! This binary loads recorded Observer scenarios (Parquet v1 and v2 layouts), normalizes them into
//! a single ordered observation stream, and will later orchestrate replay and diagnostic export on
//! top of that stream. Only scenario discovery and the Parquet loader live here today; the replay
//! CLI follows in later work, so the unused loader entry points are intentionally not yet wired
//! into `main`.
#![deny(warnings)]
// The loader is exercised by unit tests until the replay CLI is wired in (a later card); suppress
// dead-code warnings for the not-yet-called public surface rather than weakening `deny(warnings)`.
#![allow(dead_code)]

mod parquet;
mod scenario;

fn main() {
    println!("anomaly-detection-testbench: not yet implemented");
}

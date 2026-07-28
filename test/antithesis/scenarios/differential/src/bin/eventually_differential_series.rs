//! Antithesis `eventually_` check: every shared context aggregates the same on both lanes.
//!
//! The intake folds both lanes, runs the Fréchet measure per context, and asserts. This sends the
//! comparison parameters and reads a status.

use antithesis_scenario_differential::post;
use antithesis_sdk::prelude::*;
use clap::Parser;
use harness::Phase;
use serde::Serialize;

/// README defaults. `W = 1` and `equivalence_threshold = 0.02` hold until empirical results warrant
/// otherwise.
const LEASH_WIDTH: usize = 1;
const EQUIVALENCE_THRESHOLD: f64 = 0.02;
/// Bucket width in seconds, matching the Agent's default flush interval.
const BUCKET_WIDTH: i64 = 10;

#[derive(Debug, Parser)]
struct Config {
    #[arg(long = "intake-addr", env = "INTAKE_CONTROL_ADDR", default_value = "intake:2049")]
    intake_addr: String,
}

#[derive(Serialize)]
struct Params {
    bucket_width: i64,
    leash_width: usize,
    equivalence_threshold: f64,
    phase: Phase,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();
    let config = Config::parse();
    let client = post::client()?;

    post::post(
        &client,
        &config.intake_addr,
        "frechet_distance",
        &Params {
            bucket_width: BUCKET_WIDTH,
            leash_width: LEASH_WIDTH,
            equivalence_threshold: EQUIVALENCE_THRESHOLD,
            phase: Phase::Eventually,
        },
    )
}

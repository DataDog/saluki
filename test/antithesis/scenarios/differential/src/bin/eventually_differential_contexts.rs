//! Antithesis `eventually_` check: the two lanes' context sets agree.
//!
//! The intake computes the symmetric difference and asserts. This sends the flush budget and reads a
//! status.

use antithesis_scenario_differential::post;
use antithesis_sdk::prelude::*;
use clap::Parser;
use harness::{Phase, ACCEPTABLE_FLUSH_DELAY};
use serde::Serialize;

#[derive(Debug, Parser)]
struct Config {
    #[arg(long = "intake-addr", env = "INTAKE_CONTROL_ADDR", default_value = "intake:2049")]
    intake_addr: String,
}

#[derive(Serialize)]
struct Params {
    acceptable_flush_delay: i64,
    phase: Phase,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();
    let config = Config::parse();
    let client = post::client()?;

    post::post(
        &client,
        &config.intake_addr,
        "contexts",
        &Params {
            acceptable_flush_delay: ACCEPTABLE_FLUSH_DELAY.as_secs().cast_signed(),
            phase: Phase::Eventually,
        },
    )
}

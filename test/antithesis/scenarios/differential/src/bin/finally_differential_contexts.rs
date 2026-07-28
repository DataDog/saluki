//! Antithesis `finally_` check: the two lanes' context sets agree once load has drained.
//!
//! Sleeps one flush budget so in-flight contexts land, then posts a zero budget. At zero every member
//! of the difference is overdue, so the intake's assertion fails on any residual.

use std::thread::sleep;

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

    sleep(ACCEPTABLE_FLUSH_DELAY);

    post::post(
        &client,
        &config.intake_addr,
        "contexts",
        &Params {
            acceptable_flush_delay: 0,
            phase: Phase::Finally,
        },
    )
}

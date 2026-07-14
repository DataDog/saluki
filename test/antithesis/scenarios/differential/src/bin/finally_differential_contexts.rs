//! Antithesis `finally_` check: after drivers and faults stop, sleeps one `ACCEPTABLE_FLUSH_DELAY`
//! to drain in-flight contexts, then fails if the symmetric difference `D` is not empty.

use std::thread::sleep;
use std::time::Duration;

use antithesis_scenario_differential::contexts::{Difference, Lane, LaneView};
use antithesis_sdk::prelude::*;
use anyhow::Context;
use clap::Parser;
use harness::ACCEPTABLE_FLUSH_DELAY;
use reqwest::blocking::Client;
use serde_json::json;

/// Cap on diverging contexts listed in the assertion details, to bound the payload.
const SAMPLE_LIMIT: usize = 25;

#[derive(Debug, Parser)]
struct Config {
    #[arg(long = "intake-addr", env = "INTAKE_CONTROL_ADDR", default_value = "intake:2049")]
    intake_addr: String,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();
    let config = Config::parse();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build HTTP client")?;

    sleep(ACCEPTABLE_FLUSH_DELAY);

    let agent = LaneView::fetch(&client, &config.intake_addr, Lane::Agent)?;
    let adp = LaneView::fetch(&client, &config.intake_addr, Lane::Adp)?;
    let difference = Difference::between(&agent, &adp);
    let mut residual = difference.diverging();
    let adp_only = residual.iter().filter(|d| matches!(d.lane, Lane::Adp)).count();
    let agent_only = residual.iter().filter(|d| matches!(d.lane, Lane::Agent)).count();
    residual.truncate(SAMPLE_LIMIT);

    let details = json!({
        "difference": difference.len(),
        "adp_only": adp_only,
        "agent_only": agent_only,
        "sample": residual,
    });

    assert_always!(
        difference.is_empty(),
        "differential.contexts_finally_converged",
        &details
    );
    Ok(())
}

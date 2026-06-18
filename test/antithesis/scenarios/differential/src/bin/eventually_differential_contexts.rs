//! Antithesis `eventually_` check: fails when a context has sat in the symmetric difference of the
//! two lanes longer than `ACCEPTABLE_FLUSH_DELAY`. A member still within the budget is in flight.

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

    let agent = LaneView::fetch(&client, &config.intake_addr, Lane::Agent)?;
    let adp = LaneView::fetch(&client, &config.intake_addr, Lane::Adp)?;
    let difference = Difference::between(&agent, &adp);
    let delayed = difference.delayed(ACCEPTABLE_FLUSH_DELAY);
    let budget = ACCEPTABLE_FLUSH_DELAY.as_secs() as i64;
    let mut overdue: Vec<_> = difference
        .diverging()
        .into_iter()
        .filter(|d| d.age_secs > budget)
        .collect();
    let adp_only = overdue.iter().filter(|d| matches!(d.lane, Lane::Adp)).count();
    let agent_only = overdue.iter().filter(|d| matches!(d.lane, Lane::Agent)).count();
    overdue.truncate(SAMPLE_LIMIT);

    let details = json!({
        "difference": difference.len(),
        "delayed": delayed,
        "acceptable_flush_delay_secs": ACCEPTABLE_FLUSH_DELAY.as_secs(),
        "adp_only": adp_only,
        "agent_only": agent_only,
        "sample": overdue,
    });

    assert_always!(delayed == 0, "differential.contexts_eventually_equivalent", &details);
    Ok(())
}

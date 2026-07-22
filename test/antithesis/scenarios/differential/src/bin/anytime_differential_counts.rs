//! Antithesis `anytime_` check: fails when a shared context's per-lane point-count skew has exceeded
//! `ACCEPTABLE_COUNT_SKEW` for longer than `ACCEPTABLE_FLUSH_DELAY`.
//!
//! Runs live beside the load driver with faults active, so it catches a divergence the moment its
//! budget elapses rather than at a run-end sweep. Steady-state flush phase is absorbed by the skew
//! tolerance; a transient symmetric fault that heals within the budget is absorbed by the aging. The
//! aging state lives here, in the check, which is outside the SUT fault domain.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use antithesis_scenario_differential::contexts::{count_skews, Context, Lane, LaneView};
use antithesis_sdk::prelude::*;
use anyhow::Context as _;
use clap::Parser;
use harness::{ACCEPTABLE_COUNT_SKEW, ACCEPTABLE_FLUSH_DELAY};
use reqwest::blocking::Client;
use serde_json::json;

/// How often the two lanes are polled and the assertion re-evaluated.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Cap on skewed contexts listed in the assertion details, to bound the payload.
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

    // Per shared context, the instant its skew first rose above the tolerance. Cleared once it heals
    // or the context is no longer on both lanes.
    let mut since: BTreeMap<Context, Instant> = BTreeMap::new();

    loop {
        if let Err(error) = poll(&client, &config.intake_addr, &mut since) {
            // A momentary fetch failure is not a divergence. Keep the aging state and retry.
            eprintln!("poll failed, retrying: {error:#}");
        }
        sleep(POLL_INTERVAL);
    }
}

fn poll(client: &Client, intake_addr: &str, since: &mut BTreeMap<Context, Instant>) -> anyhow::Result<()> {
    let agent = LaneView::fetch(client, intake_addr, Lane::Agent)?;
    let adp = LaneView::fetch(client, intake_addr, Lane::Adp)?;
    let skews = count_skews(&agent, &adp, ACCEPTABLE_COUNT_SKEW);
    let now = Instant::now();

    // Age only contexts still over the tolerance; drop the rest so a healed context resets.
    since.retain(|context, _| skews.iter().any(|s| &s.context == context));
    for skew in &skews {
        since.entry(skew.context.clone()).or_insert(now);
    }

    let defects = defect_count(since, now, ACCEPTABLE_FLUSH_DELAY);
    let budget_secs = ACCEPTABLE_FLUSH_DELAY.as_secs();
    let mut sample: Vec<_> = skews
        .iter()
        .filter_map(|skew| {
            let age = since.get(&skew.context).map(|t| now.duration_since(*t).as_secs())?;
            (age > budget_secs).then(|| {
                json!({
                    "context": skew.context,
                    "agent_points": skew.agent_points,
                    "adp_points": skew.adp_points,
                    "skew": skew.skew,
                    "age_secs": age,
                })
            })
        })
        .collect();
    sample.truncate(SAMPLE_LIMIT);

    let details = json!({
        "skewed": skews.len(),
        "defects": defects,
        "acceptable_count_skew": ACCEPTABLE_COUNT_SKEW,
        "acceptable_flush_delay_secs": budget_secs,
        "sample": sample,
    });

    assert_always!(defects == 0, "differential.counts_within_skew", &details);
    Ok(())
}

/// How many contexts have been over the tolerance for longer than the budget. Pure over the aging
/// map so it is testable without sleeping.
fn defect_count<K>(since: &BTreeMap<K, Instant>, now: Instant, budget: Duration) -> usize {
    since
        .values()
        .filter(|&&first| now.duration_since(first) > budget)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defect_count_fires_only_past_the_budget() {
        let now = Instant::now();
        let budget = Duration::from_secs(30);
        let mut since = BTreeMap::new();
        // One context over the budget, one still within it.
        since.insert(0_u8, now.checked_sub(Duration::from_secs(40)).expect("sub"));
        since.insert(1_u8, now.checked_sub(Duration::from_secs(5)).expect("sub"));

        assert_eq!(defect_count(&since, now, budget), 1);
    }

    #[test]
    fn defect_count_is_zero_when_all_within_budget() {
        let now = Instant::now();
        let mut since = BTreeMap::new();
        since.insert(0_u8, now.checked_sub(Duration::from_secs(10)).expect("sub"));

        assert_eq!(defect_count(&since, now, Duration::from_secs(30)), 0);
    }
}

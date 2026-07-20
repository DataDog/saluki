//! The single differential correctness assertion: the online curve oracle. Each tick it polls both
//! lanes' settled aggregation curves over `/antithesis/curves`, runs the per-context banded-Frechet
//! aligner plus the decision layer, unions the per-context trips into one `defect_count`, and fires one
//! `assert_always!` that the count is zero. The details carry the defect tally and a bounded triage
//! sample sorted by continuous worst gap.
//!
//! ADP-off (Agent) is the normative reference and ADP-on (data plane) is the SUT, so any surviving trip
//! in either direction is an ADP defect. The extent rides as triage metadata only and never decides the
//! verdict.

use std::collections::BTreeSet;
use std::thread::sleep;
use std::time::Duration;

use antithesis_scenario_differential::contexts::{Bucket, Context, Lane, LaneView};
use antithesis_scenario_differential::decision::{
    bounded_triage, defect_count, evaluate, ContextVerdict, DecisionParams, Extent,
};
use antithesis_scenario_differential::ground::RuleSet;
use antithesis_sdk::prelude::*;
use clap::Parser;
use reqwest::blocking::Client;
use serde_json::json;

/// How often the two lanes are polled and the assertion re-evaluated.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Cap on defect contexts listed in the assertion details, to bound the payload.
const SAMPLE_LIMIT: usize = 25;

#[derive(Debug, Parser)]
struct Config {
    #[arg(long = "intake-addr", env = "INTAKE_CONTROL_ADDR", default_value = "intake:2049")]
    intake_addr: String,
}

/// One defect context's triage row: its identity, the mechanisms that fired, and its extent. Sorted by
/// the extent's continuous worst gap.
struct Triage {
    context: Context,
    trips: Vec<String>,
    extent: Extent,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();
    let config = Config::parse();
    // reqwest resolves with rustls-no-provider across the workspace, so install a
    // crypto provider before the Client is built, even for plain HTTP, or the build
    // panics on its internal runtime thread.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("build HTTP client: {e}"))?;

    loop {
        if let Err(error) = poll(&client, &config.intake_addr) {
            // A momentary fetch failure is not a divergence. Retry on the next tick.
            eprintln!("poll failed, retrying: {error:#}");
        }
        sleep(POLL_INTERVAL);
    }
}

fn poll(client: &Client, intake_addr: &str) -> anyhow::Result<()> {
    let reference = LaneView::fetch(client, intake_addr, Lane::Agent)?.curves();
    let sut = LaneView::fetch(client, intake_addr, Lane::Adp)?.curves();
    let params = DecisionParams::launch();
    let rules = RuleSet::default_rules();
    let empty: Vec<Bucket> = Vec::new();

    let contexts: BTreeSet<&Context> = reference.keys().chain(sut.keys()).collect();
    let mut verdicts: Vec<ContextVerdict> = Vec::with_capacity(contexts.len());
    let mut triage: Vec<Triage> = Vec::new();
    for context in contexts {
        let reference_curve = reference.get(context).unwrap_or(&empty);
        let sut_curve = sut.get(context).unwrap_or(&empty);
        let verdict = evaluate(reference_curve, sut_curve, &rules, &params);
        if verdict.tripped {
            triage.push(Triage {
                context: context.clone(),
                trips: verdict.trips.iter().map(|t| t.rule.clone()).collect(),
                extent: verdict.extent,
            });
        }
        verdicts.push(verdict);
    }

    let defects = defect_count(&verdicts);
    let sample: Vec<_> = bounded_triage(triage, |t| t.extent.worst_gap, SAMPLE_LIMIT)
        .into_iter()
        .map(|t| {
            json!({
                "context": t.context,
                "trips": t.trips,
                "run_length": t.extent.run_length,
                "worst_gap": finite(t.extent.worst_gap),
                "accumulated_gap": finite(t.extent.accumulated_gap),
            })
        })
        .collect();

    let details = json!({
        "contexts": verdicts.len(),
        "defects": defects,
        "band_alpha": params.band.alpha,
        "band_floor": params.band.floor,
        "sample": sample,
    });

    assert_always!(defects == 0, "differential.contexts_curve_equivalent", &details);
    Ok(())
}

/// A finite magnitude as itself, a non-finite one as JSON null: an infinite present/absent leash serdes
/// cleanly rather than truncating the payload.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

//! Antithesis `finally_` test command: verifies metrics reached the intake
//! after drivers complete.
//!
//! Runs after the driver(s) finish and fault injection has stopped. Checks the
//! eventual-delivery liveness baseline for the `forwarder-eventual-delivery`
//! property: at least once across the run, metrics submitted to ADP are
//! aggregated, forwarded, and observed at the mock intake. Aggregation flushes
//! on an interval before forwarding, so this polls with retries.
//!
//! This is a fairly weak verification and will be improved in the future.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use antithesis_sdk::prelude::*;
use anyhow::{anyhow, bail};
use clap::{builder::NonEmptyStringValueParser, Parser};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "finally_verify_delivery")]
struct Config {
    #[arg(
        long = "intake-addr",
        env = "INTAKE_ADDR",
        default_value = "intake:2049",
        value_parser = NonEmptyStringValueParser::new()
    )]
    intake_addr: String,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();

    let config = Config::try_parse()?;

    let mut delivered = 0usize;
    let mut query_ok = false;
    // Poll up to ~60s: aggregation flushes periodically, then the forwarder ships to the intake.
    for _ in 0..60 {
        if let Ok(n) = fetch_intake_metric_count(&config) {
            query_ok = true;
            delivered = n;
            if n > 0 {
                break;
            }
        } else {
            // Transient during recovery; keep polling.
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    assert_reachable!(
        "intake metrics dump query succeeded",
        &json!({ "delivered": delivered, "query_ok": query_ok })
    );

    assert_sometimes!(
        delivered > 0,
        "metrics delivered end-to-end to the intake",
        &json!({ "delivered": delivered })
    );

    Ok(())
}

fn fetch_intake_metric_count(config: &Config) -> anyhow::Result<usize> {
    let mut stream = TcpStream::connect(config.intake_addr.as_str())?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!(
        "GET /metrics/dump HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        config.intake_addr
    );
    stream.write_all(req.as_bytes())?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("intake response did not include an HTTP header terminator"))?;
    let body = &buf[header_end + 4..];

    let value: serde_json::Value = serde_json::from_slice(body)?;
    let Some(arr) = value.as_array() else {
        bail!("intake /metrics/dump did not return a JSON array");
    };

    Ok(arr.len())
}

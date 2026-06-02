//! Antithesis `parallel_driver_` test command: sends a batch of `DogStatsD` metrics to ADP.
//!
//! Draws a per-timeline cardinality regime (swarm biasing) and a batch size, then sends metrics over
//! UDS. The high-cardinality regime floods distinct aggregation contexts, targeting the
//! `rss-bounded-under-cardinality` property (ADP's memory limiter is disabled by default, so RSS can
//! grow without bound under sustained high cardinality).

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
use anyhow::Context as _;
use clap::Parser;
use rand::{rand_core::UnwrapErr, seq::IndexedRandom as _, RngExt as _};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "parallel_driver_send_dogstatsd")]
struct Config {
    #[arg(
        long = "dogstatsd-socket",
        env = "DSD_SOCKET",
        default_value = "/var/run/datadog/dsd.socket"
    )]
    dogstatsd_socket: PathBuf,
}

#[derive(Clone, Copy, Debug)]
enum Cardinality {
    Low,
    Medium,
    High,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();

    let config = Config::try_parse()?;
    let mut rng = UnwrapErr(AntithesisRng);
    let regimes = [Cardinality::Low, Cardinality::Medium, Cardinality::High];
    let regime = *regimes
        .choose(&mut rng)
        .context("cardinality regime choices must not be empty")?;
    let regime_label = match regime {
        Cardinality::Low => "low",
        Cardinality::Medium => "medium",
        Cardinality::High => "high",
    };
    let count: u64 = rng.random_range(50..=2000);

    let socket = connect_with_retry(&config.dogstatsd_socket)?;

    let names = ["adp.test.foo", "adp.test.bar", "adp.test.balkajsldfkjasdlfkjasdfz"];
    let metric_types = ["c", "g"];
    let mut attempted = 0usize;
    for i in 0..count {
        let name = *names
            .choose(&mut rng)
            .context("metric name choices must not be empty")?;
        let metric_type = *metric_types
            .choose(&mut rng)
            .context("metric type choices must not be empty")?;
        let value: u64 = rng.random_range(0..=1000);
        let tag = match regime {
            Cardinality::Low => format!("host:h{}", rng.random_range(0..4)),
            Cardinality::Medium => format!("host:h{}", rng.random_range(0..256)),
            Cardinality::High => format!("uid:{i}-{}", rng.random::<u64>()),
        };
        let line = format!("{name}:{value}|{metric_type}|#{tag}\n");
        if socket.send(line.as_bytes()).is_ok() {
            attempted += 1;
        }
    }

    assert_reachable!(
        "workload sent a dogstatsd batch",
        &json!({
            "attempted": attempted,
            "regime": regime_label,
            "socket": config.dogstatsd_socket.display().to_string(),
        })
    );

    // Confirm timelines sometimes drive a high-cardinality flood (the interesting case for memory).
    assert_sometimes!(
        matches!(regime, Cardinality::High),
        "workload drove a high-cardinality dogstatsd flood",
        &json!({ "attempted": attempted })
    );

    Ok(())
}

// Wait for ADP to bind the socket, intentionally naive.
fn connect_with_retry(path: &Path) -> anyhow::Result<UnixDatagram> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let socket = UnixDatagram::unbound()?;
        match socket.connect(path) {
            Ok(()) => return Ok(socket),
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(250)),
            Err(e) => return Err(e).with_context(|| format!("ADP did not bind {} within 30s", path.display())),
        }
    }
}

//! Antithesis parallel driver that polls ADP's `/dogstatsd/stats` privileged
//! endpoint. Sends two overlapping `GET /dogstatsd/stats` requests to the
//! privileged API at `:5101` over HTTPS with no client auth. One request runs on
//! a background thread. A second follows after a short delay while the first is
//! still outstanding.

use std::thread;
use std::time::Duration;

use antithesis_sdk::prelude::*;
use clap::Parser;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "parallel_driver_poll_stats")]
struct Config {
    #[arg(
        long = "adp-secure-api-addr",
        env = "ADP_SECURE_API_ADDR",
        default_value = "agent:5101"
    )]
    adp_secure_api_addr: String,
    /// Collection window each request asks the SUT to hold, in seconds.
    #[arg(long = "collection-secs", env = "STATS_COLLECTION_SECS", default_value_t = 5)]
    collection_secs: u64,
}

/// Sends one `GET /dogstatsd/stats` and returns the HTTP status, or `None` on a
/// transport error.
fn poll(client: &reqwest::blocking::Client, addr: &str, collection_secs: u64) -> Option<u16> {
    let url = format!("https://{addr}/dogstatsd/stats?collection_duration_secs={collection_secs}");
    client.get(&url).send().ok().map(|resp| resp.status().as_u16())
}

fn main() -> anyhow::Result<()> {
    antithesis_init();

    // reqwest is built without selecting a crypto provider, so install the
    // process-wide one before any Rustls client configuration is built.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let config = Config::try_parse()?;

    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(config.collection_secs + 10))
        .build()?;

    let addr = config.adp_secure_api_addr;
    let secs = config.collection_secs;

    // Send the first request from a background thread, then a second after a short delay.
    let opener_client = client.clone();
    let opener_addr = addr.clone();
    let opener = thread::spawn(move || poll(&opener_client, &opener_addr, secs));

    thread::sleep(Duration::from_millis(250));
    let overlap_status = poll(&client, &addr, secs);

    let opener_status = opener.join().map_err(|_| anyhow::anyhow!("opener thread panicked"))?;

    // No response from either request. Exit without asserting.
    let (Some(opener_status), Some(overlap_status)) = (opener_status, overlap_status) else {
        return Ok(());
    };

    assert_reachable!(
        "workload polled dogstatsd stats with overlapping requests",
        &json!({
            "adp_secure_api_addr": addr,
            "opener_status": opener_status,
            "overlap_status": overlap_status,
        })
    );

    assert_sometimes!(
        opener_status == 429 || overlap_status == 429,
        "workload overlapped an active dogstatsd stats collection",
        &json!({ "opener_status": opener_status, "overlap_status": overlap_status })
    );

    Ok(())
}

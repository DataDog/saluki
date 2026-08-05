//! Antithesis parallel driver that polls ADP's `/dogstatsd/stats` privileged
//! endpoint. Sends two overlapping `GET /dogstatsd/stats` requests to the
//! privileged API at `:5101` over HTTPS with the shared IPC mTLS identity. One
//! request runs on a background thread. A second follows after a short delay
//! while the first is still outstanding.

use std::fs;
use std::path::{Path, PathBuf};
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
    /// Combined certificate and private key PEM used for IPC mTLS.
    #[arg(
        long = "ipc-cert-path",
        env = "IPC_CERT_PATH",
        default_value = "/ipc-auth/ipc_cert.pem"
    )]
    ipc_cert_path: PathBuf,
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

fn build_client(ipc_cert_path: &Path, timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    let identity_pem = fs::read(ipc_cert_path)?;
    let identity = reqwest::Identity::from_pem(&identity_pem)?;

    // The Antithesis client reaches the generated test certificate through the cross-container `agent` hostname,
    // which is not guaranteed to appear in its SANs. Server-certificate validation is disabled only for this test
    // client; the privileged server still authenticates the client by exact DER equality with this shared identity.
    Ok(reqwest::blocking::Client::builder()
        .identity(identity)
        .danger_accept_invalid_certs(true)
        .timeout(timeout)
        .build()?)
}

fn main() -> anyhow::Result<()> {
    antithesis_init();

    // reqwest is built without selecting a crypto provider, so install the
    // process-wide one before any Rustls client configuration is built.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let config = Config::try_parse()?;
    let addr = config.adp_secure_api_addr;
    let secs = config.collection_secs;

    let (opener_status, overlap_status) =
        match build_client(&config.ipc_cert_path, Duration::from_secs(config.collection_secs + 10)) {
            Ok(client) => {
                // Send the first request from a background thread, then a second after a short delay.
                let opener_client = client.clone();
                let opener_addr = addr.clone();
                let opener = thread::spawn(move || poll(&opener_client, &opener_addr, secs));

                thread::sleep(Duration::from_millis(250));
                let overlap_status = poll(&client, &addr, secs);
                (opener.join().unwrap_or(None), overlap_status)
            }
            Err(_) => (None, None),
        };
    let opener_reached_http = opener_status.is_some();
    let overlap_reached_http = overlap_status.is_some();
    assert_sometimes!(
        opener_reached_http && overlap_reached_http,
        "both overlapping privileged requests completed TLS and HTTP transport",
        &json!({
            "opener_reached_http": opener_reached_http,
            "overlap_reached_http": overlap_reached_http,
        })
    );
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

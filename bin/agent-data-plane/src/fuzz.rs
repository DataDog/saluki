#![allow(dead_code)]
use std::path::Path;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use bytes::Bytes;
use saluki_error::{generic_error, GenericError};
use tempfile::TempDir;
use tokio::time::{self};
use tracing::{info, warn};

use crate::fuzz_run::handle_run_command;

pub const INTAKE_PORT: u16 = 2049;

pub fn saluki_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("dd/saluki")
}


pub fn metric_line_from_raw(raw: &[u8]) -> Result<(u64, Bytes), GenericError> {
    let split_index = memchr::memchr(b';', raw).ok_or_else(|| generic_error!("missing ';'"))?;
    let ts = std::str::from_utf8(&raw[..split_index])?.parse::<u64>()?;
    let body = Bytes::copy_from_slice(&raw[split_index + 1..]);
    Ok((ts, body.into()))
}

pub fn aggregate_metric_lines(unsorted_packets: Vec<(u64, Bytes)>) -> Vec<(u64, Vec<Bytes>)> {
    let mut groups: BTreeMap<u64, Vec<Bytes>> = BTreeMap::new();
    unsorted_packets.into_iter().for_each(|(ts, packet)| {
        groups.entry(ts).or_default().push(packet);
    });
    groups.into_iter().collect()
}

#[derive(Debug)]
pub struct DogStatsDInput {
    pub messages: Vec<(u64, Vec<Bytes>)>,
}

async fn inject_plan(socket: tokio::net::UnixDatagram, plan: Vec<(u64, Vec<Bytes>)>) -> Result<(), GenericError> {
    let start = Instant::now();
    let mut maybe_prev_ts = None;
    for (ts, packets) in &plan {
        // timestamp X is sent at second X/10
        if let Some(prev_ts) = maybe_prev_ts {
            assert!(prev_ts < ts);
        }
        maybe_prev_ts = Some(ts);
        let target = start + Duration::from_millis(ts * 10);
        if target > Instant::now() {
            tokio::time::sleep_until(target.into()).await;
        }
        info!(?ts, "Injecting");
        for packet in packets {
            if let Err(e) = socket.send(packet).await {
                eprintln!("inject error: {e}");
            }
        }
    }
    Ok(())
}

const DD_YAML_CONFIG: &str = "bin/agent-data-plane/fuzz_datadog_config.yaml";

fn build_config_object(agent_unix_socket: &Path, intake_port: u16) -> serde_json::Value {
    // health port useless?
    serde_json::json!({
        "hostname": "correctness-testing",
        "api_key": "dummy",
        "health_port": 5555,
        "dd_url": format!("http://localhost:{intake_port}"),
        "dogstatsd_port": 0,
        "dogstatsd_socket": agent_unix_socket.to_str().expect("socket path is not valid UTF-8"),
        "dogstatsd_worker_count": 1,
        "data_plane": {
            "enabled": true,
            "standalone_mode": true,
            "dogstatsd": { "enabled": true }
        }
    })
}

// inject configuration
// - start with 3-4 static config entries that
// - config registry (saluki-component > registry)
// value lies in pair comparison "if you changed that config value, we do expect some/no change in the output when running the same corpus with/without that config entry. Do we actually observe that ?"

pub async fn inner(corpus: DogStatsDInput) {
    // unix domain sockets instead of UDP
    let dir = TempDir::with_prefix("saluki").expect("could not create temp dir");
    let unix_socket_path = dir.path().join("dsd_socket");
    info!(?unix_socket_path, "Created UDS");

    // for output port (intake), we can bind to it, let it report back to our main loop, so we can configure the pipeline to use it
    let intake_tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind mock intake TCP listener on 127.0.0.1:0");
    let intake_port = intake_tcp_listener
        .local_addr()
        .expect("failed to read mock intake listener local address")
        .port();

    let config = build_config_object(&unix_socket_path, intake_port);

    let (intake_router, intake_signals) = datadog_intake::build_intake();
    let (st2_tx, st2_rx) = tokio::sync::oneshot::channel();
    let intake_handle = tokio::spawn(datadog_intake::serve_intake(
        intake_tcp_listener,
        intake_router,
        async move { let _ = st2_rx.await; },
    ));

    let (st1_tx, st1_rx) = tokio::sync::oneshot::channel();
    let saluki_handle = tokio::spawn(handle_run_command(config, st1_rx, Instant::now()));

    // Wait for ADP to create the socket file before trying to connect.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if unix_socket_path.exists() {
            break;
        }
        if saluki_handle.is_finished() {
            let result = saluki_handle.await.expect("ADP task panicked");
            panic!("ADP task exited before creating socket: {:?}", result);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for ADP to create socket at {unix_socket_path:?}"
        );
        time::sleep(Duration::from_millis(100)).await;
    }
    info!("Socket ready, connecting");

    // run injection
    let socket = tokio::net::UnixDatagram::unbound().expect("failed to create unbound Unix datagram socket");
    socket
        .connect(&unix_socket_path)
        .expect("failed to connect to dogstatsd UDS — is saluki listening?");
    info!("Starting injection");
    inject_plan(socket, corpus.messages)
        .await
        .expect("failed to inject corpus into dogstatsd socket");

    info!("Waiting for 2 series payloads at fake-intake");
    let mut series_rx = intake_signals.series_v2_count;
    series_rx
        .wait_for(|&n| n >= 2)
        .await
        .expect("intake server dropped before receiving 2 series payloads");
    info!(count = *series_rx.borrow(), "Received expected series payloads");

    // trigger shutdown
    info!("Trigger shutdown");
    let _ = st1_tx.send(());
    saluki_handle
        .await
        .expect("saluki task panicked or was cancelled before shutdown")
        .expect("saluki run command returned an error");

    info!("stopping mock intake");
    let _ = st2_tx.send(());
    if let Err(e) = intake_handle
        .await
        .expect("mock intake task panicked or was cancelled before shutdown")
    {
        warn!("Mock intake error: {}", e);
    }
}

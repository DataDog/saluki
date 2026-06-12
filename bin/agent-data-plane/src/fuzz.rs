//! Fuzz integration test harness.
//!
//! This module drives the agent-data-plane (ADP) end-to-end against a synthetic DogStatsD corpus
//! while staying entirely in-process and under simulated time, so the test completes in well
//! under a second of wall clock.
//!
//! # Design
//!
//! Three coupled changes make `tokio::time::pause()` auto-advance behave correctly here. Each
//! one is necessary; removing any of them stretches the test out to many real seconds.
//!
//! 1. **In-memory transport between forwarder and intake.** The intake's axum router is served
//!    over a [`tokio::io::DuplexStream`] handed back by a custom hyper connector instead of
//!    over loopback TCP. With kernel I/O on the path, paused-tokio sees every task parked on
//!    real I/O during a request roundtrip and auto-advances simulated time, firing periodic
//!    timers and burning real CPU. With duplex IO, writing on one half wakes the other half via
//!    in-memory wakers — the runtime always has a runnable task and never auto-advances during
//!    a request.
//!
//! 2. **Ambient worker pool for the topology.** [`saluki_core`]'s default
//!    `WorkerPoolConfiguration::Dedicated` builds its own multi-thread runtime to run
//!    components on. That runtime does not inherit `start_paused`, so the topology would run
//!    against real wall clock even though the test runtime is paused. Calling
//!    `with_ambient_worker_pool()` (in `fuzz_run.rs`) keeps every component on the paused
//!    runtime.
//!
//! 3. **Tokio-anchored unix clock.** The aggregator decides which buckets to flush by reading
//!    [`saluki_common::time::get_unix_timestamp`]. That function now derives the timestamp from
//!    a [`tokio::time::Instant`] anchored on the wall clock at first use, so under paused tokio
//!    the unix clock advances with simulated time and bucket expiration races simulated time
//!    rather than real time.
//!
//! # Wait condition
//!
//! Under paused tokio with a finite injected corpus, the aggregator naturally produces one
//! payload per metric type once the first flush window closes — then everything goes quiet (no
//! more metrics arrive, the runtime no longer advances real wall clock). The test therefore
//! waits for `>= 1` series payload as the end-to-end invariant: it proves the pipeline went all
//! the way from UDS → source → aggregator → encoder → forwarder → in-process intake.

#![allow(dead_code)]
use std::path::Path;
use std::sync::Arc;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use bytes::Bytes;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use saluki_error::{generic_error, GenericError};
use tempfile::TempDir;
use tokio::time::{self};
use tracing::info;

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
    // ADP listens for DogStatsD on a UDS we create in a tempdir; injection writes packets there.
    let dir = TempDir::with_prefix("saluki").expect("could not create temp dir");
    let unix_socket_path = dir.path().join("dsd_socket");
    info!(?unix_socket_path, "Created UDS");

    // Build the intake router and its signal receivers. The router itself drives request
    // handling; signals are readable immediately, no separate listener task is needed.
    let (intake_router, intake_signals) = datadog_intake::build_intake();

    // The in-process connector ignores the URI host/port — it always returns a duplex pair —
    // but ADP still parses `dd_url` into a real URI. Use a literal IP + arbitrary port so URL
    // parsing succeeds without consulting DNS.
    let config = build_config_object(&unix_socket_path, /* placeholder */ 1);

    // Per-connection handler: hand the server-side IO half to a fresh `serve_connection` task
    // that drives the axum router. `axum::Router` already implements `tower::Service`, so
    // `TowerToHyperService` is the only adapter needed to feed it to hyper. Errors from
    // `serve_connection` are dropped silently — they're typically just "connection closed" at
    // shutdown and have no signal value in a test harness.
    let intake_handler: crate::fuzz_run::InProcessIntakeHandler = Arc::new(move |io| {
        let svc = TowerToHyperService::new(intake_router.clone());
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(io), svc)
                .await;
        });
    });

    let (st1_tx, st1_rx) = tokio::sync::oneshot::channel();
    let saluki_handle = tokio::spawn(handle_run_command(
        config,
        Some(intake_handler),
        st1_rx,
        Instant::now(),
    ));

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

    // See the module-level docs for why we wait for a single series payload.
    info!("Waiting for series payload at fake-intake");
    let mut series_rx = intake_signals.series_v2_count;
    series_rx
        .wait_for(|&n| n >= 1)
        .await
        .expect("intake server dropped before receiving series payload");
    info!(count = *series_rx.borrow(), "Received expected series payload");

    info!("Trigger shutdown");
    let _ = st1_tx.send(());
    saluki_handle
        .await
        .expect("saluki task panicked or was cancelled before shutdown")
        .expect("saluki run command returned an error");
}

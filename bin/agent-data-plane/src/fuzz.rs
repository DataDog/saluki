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
//! 1. **In-memory transports on both edges of ADP.** Neither leg of the pipeline goes through
//!    the kernel. The DogStatsD source is fed by an in-process `Listener` (see
//!    [`saluki_io::net::listener::Listener::in_process`]) backed by a tokio mpsc channel; the
//!    Datadog forwarder hits an `InProcessHandler` that hands [`tokio::io::DuplexStream`]
//!    halves to a hyper-server task running the intake's axum router. With kernel I/O on the
//!    path, paused-tokio sees every task parked on real I/O and auto-advances simulated time,
//!    firing periodic timers and burning real CPU. With in-memory transports, writing always
//!    wakes the reader via in-memory wakers — the runtime always has a runnable task and never
//!    auto-advances during a request.
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
//! After injecting the corpus, the harness waits for the in-process intake to observe its
//! first series payload, or for 40 simulated seconds — whichever happens first — and then
//! shuts down. Under paused tokio with auto-advance, the timeout elapses in microseconds of
//! wall clock when nothing else is making progress, so a bad input bounds the iteration cost
//! rather than hanging libfuzzer. Inputs that decode to zero metrics are rejected upstream
//! via `arbitrary::Error::IncorrectFormat` so [`inner`] is never called on them.

#![allow(dead_code)]
use std::sync::Arc;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use bytes::Bytes;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{listener::Listener, ConnectionAddress};
use tokio::sync::mpsc;
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

async fn inject_plan(
    tx: mpsc::Sender<(Bytes, ConnectionAddress)>, plan: Vec<(u64, Vec<Bytes>)>,
) -> Result<(), GenericError> {
    // Synthetic peer address for every injected packet — datagram peers don't have a meaningful
    // identity here, and `ProcessLike(None)` matches what UDS-datagram returns when no peer
    // credentials are attached.
    let peer = ConnectionAddress::ProcessLike(None);

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
            if let Err(e) = tx.send((packet.clone(), peer.clone())).await {
                eprintln!("inject error: {e}");
            }
        }
    }
    Ok(())
}

const DD_YAML_CONFIG: &str = "bin/agent-data-plane/fuzz_datadog_config.yaml";

fn build_config_object(intake_port: u16) -> serde_json::Value {
    // No `dogstatsd_socket`/`dogstatsd_port`: the source listens only on the in-process
    // listener injected via `DogStatsDConfiguration::with_extra_listener`.
    serde_json::json!({
        "hostname": "correctness-testing",
        "api_key": "dummy",
        "health_port": 5555,
        "dd_url": format!("http://localhost:{intake_port}"),
        "dogstatsd_port": 0,
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
    // Build the intake router and its signal receivers. The router itself drives request
    // handling; signals are readable immediately, no separate listener task is needed.
    let (intake_router, intake_signals) = datadog_intake::build_intake();

    // The in-process connector ignores the URI host/port — it always returns a duplex pair —
    // but ADP still parses `dd_url` into a real URI. Use a literal IP + arbitrary port so URL
    // parsing succeeds without consulting DNS.
    let config = build_config_object(/* placeholder */ 1);

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

    // In-process DogStatsD transport: ADP's source drains `dsd_rx`; the injector below pushes
    // packets through `dsd_tx`. Channel capacity is comfortably above any single burst from
    // the corpus generator.
    let (dsd_tx, dsd_rx) = mpsc::channel::<(Bytes, ConnectionAddress)>(1024);
    let dsd_listener = Listener::in_process(dsd_rx);

    let (st1_tx, st1_rx) = tokio::sync::oneshot::channel();
    let saluki_handle = tokio::spawn(handle_run_command(
        config,
        Some(intake_handler),
        Some(dsd_listener),
        st1_rx,
        Instant::now(),
    ));

    info!("Starting injection");
    inject_plan(dsd_tx, corpus.messages)
        .await
        .expect("failed to inject corpus into dogstatsd channel");

    // Shut down as soon as the intake observes its first series payload — or after a 40-sim-sec
    // timeout, whichever comes first. The timeout bounds iteration cost on inputs that never
    // produce a payload (e.g. a single zero-valued counter that sits in the aggregator without
    // ever crossing a closed-bucket boundary); under paused tokio with auto-advance the 40 sim
    // seconds elapse in microseconds of wall clock.
    let mut series_rx = intake_signals.series_v2_count;
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        series_rx.wait_for(|&n| n >= 1),
    )
    .await
    .map(|res| res.map(|guard| *guard));
    match outcome {
        Ok(Ok(count)) => info!(count, "Intake received first series payload"),
        Ok(Err(_)) => panic!("intake series-count watch closed before any payload arrived"),
        Err(_) => {
            info!("Timed out waiting for intake payload after 40 sim seconds; shutting down anyway")
        }
    }

    info!("Trigger shutdown");
    let _ = st1_tx.send(());
    saluki_handle
        .await
        .expect("saluki task panicked or was cancelled before shutdown")
        .expect("saluki run command returned an error");
}

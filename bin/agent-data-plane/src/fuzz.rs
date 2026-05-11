#![allow(dead_code)]
use std::sync::LazyLock;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use arbitrary::{Arbitrary, Unstructured};
use barkus_core::{generate::decode, ir::GrammarIr, profile::Profile};
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use saluki_error::{generic_error, GenericError};
use tokio::{
    net::UdpSocket,
    time::{self},
};
use tracing::{info, warn};

use crate::fuzz_run::handle_run_command;

const DSD_PORT: u16 = 3000;
const INTAKE_PORT: u16 = 2049;

pub fn saluki_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("dd/saluki")
}

/// In-process Datadog intake server using the real datadog-intake implementation.
async fn run_mock_intake(shutdown: tokio::sync::oneshot::Receiver<()>) -> Result<(), GenericError> {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], INTAKE_PORT));
    datadog_intake::serve(addr, async move { let _ = shutdown.await; }).await
}

pub static PROFILE: LazyLock<Profile> = LazyLock::new(|| Profile::default());

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


static GRAMMAR_MULTILINE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path = saluki_path().join("fuzz/dogstatsd_multi_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path).unwrap();
    barkus_ebnf::compile(&grammar).unwrap()
});

#[derive(Debug)]
pub struct DogStatsDInput {
    pub messages: Vec<(u64, Vec<Bytes>)>,
}

impl<'a> Arbitrary<'a> for DogStatsDInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let tape = u.bytes(u.len())?;
        let profile = Profile::default();
        let (ast, _) = decode(&*&GRAMMAR_MULTILINE, &profile, tape).map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let text = ast.serialize();
        let message_lines: Vec<(u64, Bytes)> = text
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(metric_line_from_raw)
            .collect::<Result<_, _>>()
            .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let aggregated_messages = aggregate_metric_lines(message_lines);

        Ok(DogStatsDInput {
            messages: aggregated_messages,
        })
    }
}

async fn inject_plan(plan: Vec<(u64, Vec<Bytes>)>) -> Result<(), GenericError> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.expect("failed to bind UDP socket");
    socket
        .connect(("127.0.0.1", DSD_PORT))
        .await
        .unwrap_or_else(|e| panic!("failed to connect to port {}: {}", DSD_PORT, e));

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
        for packet in packets {
            if let Err(e) = socket.send(packet).await {
                eprintln!("send error: {e}");
            }
        }
    }
    Ok(())
}

const DD_YAML_CONFIG: &str = "bin/agent-data-plane/fuzz_datadog_config.yaml";

pub async fn inner(corpus: DogStatsDInput) {
    let config_path = saluki_path().join(DD_YAML_CONFIG);
    let (st1_tx, st1_rx) = tokio::sync::oneshot::channel();
    let saluki_handle = tokio::spawn(handle_run_command(config_path, st1_rx, Instant::now()));
    let (st2_tx, st2_rx) = tokio::sync::oneshot::channel();
    let intake_handle = tokio::spawn(run_mock_intake(st2_rx));
    time::sleep(Duration::from_secs(2)).await;

    // run injection
    info!("Starting injection");
    inject_plan(stream, corpus.messages)
        .await
        .expect("failed to inject corpus into dogstatsd socket");
    time::sleep(Duration::from_secs(10)).await;

    // trigger shutdown
    info!("Trigger shutdown");
    let _ = st1_tx.send(());
    saluki_handle
        .await
        .expect("saluki task panicked or was cancelled before shutdown")
        .expect("saluki run command returned an error");

    info!("stopping mock intake");
    let _ = st2_tx.send(());
    if let Err(e) = intake_handle.await.expect("mock intake task panicked or was cancelled before shutdown") {
        warn!("Mock intake error: {}", e);
    }
}

fuzz_target!(|input: DogStatsDInput| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(input));
});

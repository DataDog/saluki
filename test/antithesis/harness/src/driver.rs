//! Shared `DogStatsD` load-driver engine.
//!
//! A run fetches a working set of contexts from the intake's pool, then a producer
//! thread packs metric lines for those contexts into a bounded channel while a
//! consumer thread fans each datagram out to every socket and tallies per-socket
//! sends. Drivers differ only in how many sockets they target and which anchors
//! they fire, so both the single-socket and differential drivers run on this one
//! engine.
//!
//! NOTE: this driver intentionally blocks on backpressure from the SUT. Retry
//! and backoff timers are meant to endure transient errors.

use std::io::ErrorKind;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use antithesis_sdk::prelude::*;
use rand::{Rng, RngExt};
use serde_json::json;

use crate::context::{decode_response, Context};
use crate::dogstatsd::is_malformed;
use crate::payload::dogstatsd;

const SEND_RETRY_BUDGET: Duration = Duration::from_secs(5);
const SEND_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// How long to wait for the intake's pool to serve contexts before giving up.
const CONTEXT_FETCH_BUDGET: Duration = Duration::from_secs(30);
const CONTEXT_FETCH_BACKOFF: Duration = Duration::from_millis(250);

/// Largest working set a single invocation samples from the pool. Each invocation
/// touches at most this many contexts, so coverage of a large pool builds across
/// invocations and recurrence emerges once cumulative samples exceed the cap.
const MAX_WORKING_SET: usize = 1_024;

/// A generated payload queued for the sockets: the packed bytes and what they hold.
struct Datagram {
    /// The `\n`-packed payload bytes to ship over a socket.
    bytes: Vec<u8>,
    /// The lines and largest packed run in `bytes`.
    payload: dogstatsd::Payload,
}

/// What a driver run shipped, for anchoring assertions.
#[derive(Clone, Debug)]
pub struct Stats {
    /// Payloads pulled from the channel, whether or not any send succeeded.
    pub received: usize,
    /// Lines delivered per socket, summed across payloads, indexed as the sockets
    /// were passed to [`run`].
    pub sent: Vec<usize>,
    /// Largest packed run that reached each socket, indexed likewise. Zero when
    /// no multi-value line reached that socket.
    pub max_packed: Vec<usize>,
    /// Whether a send exhausted the retry budget under sustained backpressure.
    /// Distinguishes a wedged or paused peer from a clean partial batch.
    pub timed_out: bool,
}

impl Stats {
    /// A run that shipped nothing, for a socket count of `sockets`.
    fn empty(sockets: usize) -> Self {
        Self {
            received: 0,
            sent: vec![0; sockets],
            max_packed: vec![0; sockets],
            timed_out: false,
        }
    }
}

/// Fetch a working set of contexts from `http://{intake_addr}/contexts`, retrying
/// through the pool's config not being ready yet. Returns `None` if the intake
/// stays unreachable past [`CONTEXT_FETCH_BUDGET`].
fn fetch_contexts(intake_addr: &str, n: usize) -> Option<Vec<Context>> {
    let url = format!("http://{intake_addr}/contexts?n={n}");
    // reqwest resolves with rustls-no-provider across the workspace, so a crypto
    // provider must be installed before a Client is built, even for plain HTTP, or
    // the build panics on its internal runtime thread. Idempotent, so ignore the
    // already-installed error.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Build fallibly so a builder failure degrades to None like every other fetch path.
    let client = reqwest::blocking::Client::builder().build().ok()?;
    let deadline = Instant::now() + CONTEXT_FETCH_BUDGET;
    loop {
        if let Some(contexts) = try_fetch(&client, &url) {
            return Some(contexts);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(CONTEXT_FETCH_BACKOFF);
    }
}

/// One fetch attempt: a successful body decoded into contexts, else `None`.
fn try_fetch(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<Context>> {
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    decode_response(&response.bytes().ok()?)
}

/// Fetch a sampled working set from `intake_addr`, then drive `count` datagrams of
/// metric lines for those contexts to every socket, packing each to at most
/// `limit_bytes` and blocking through transient backpressure so every datagram
/// reaches every socket. `limit_bytes` and `count` come from a load generator's
/// [`crate::config::DriverConfig`], so a datagram never truncates on receive.
///
/// A peer that leaves mid-batch, or backpressure that outlasts the retry budget,
/// ends the run early with a partial [`Stats`] rather than an error. An intake that
/// never serves the working set ends the run as a no-op [`Stats::empty`], not an
/// error, so a transient intake outage does not fail the scenario.
///
/// # Errors
///
/// Errors if a worker thread panics.
pub fn run<R: Rng + Send + 'static>(
    mut rng: R, intake_addr: &str, limit_bytes: usize, count: usize, sockets: Vec<UnixDatagram>,
) -> anyhow::Result<Stats> {
    let working_set = rng.random_range(1..=MAX_WORKING_SET);
    let Some(contexts) = fetch_contexts(intake_addr, working_set) else {
        return Ok(Stats::empty(sockets.len()));
    };

    let (tx, rx) = sync_channel::<Datagram>(2024);

    let producer = thread::spawn(move || {
        for _ in 0..count {
            let mut bytes = Vec::new();
            let payload = dogstatsd::write_payload(&mut rng, &mut bytes, &contexts, limit_bytes);
            // A tiny limit can leave the first line unable to fit, yielding an empty
            // buffer. Do not ship a zero-length datagram.
            if bytes.is_empty() {
                continue;
            }
            // Assertion #1: the generator only ever emits well-formed payloads.
            assert_always!(
                is_malformed(&bytes).is_none(),
                "driver payload is well-formed",
                &json!({ "lines": payload.lines })
            );
            if tx.send(Datagram { bytes, payload }).is_err() {
                break;
            }
        }
    });

    let consumer = thread::spawn(move || -> anyhow::Result<Stats> {
        let mut received = 0usize;
        let mut sent = vec![0usize; sockets.len()];
        let mut max_packed = vec![0usize; sockets.len()];
        let mut timed_out = false;
        'recv: while let Ok(datagram) = rx.recv() {
            received += 1;
            for (i, socket) in sockets.iter().enumerate() {
                match deliver(socket, &datagram.bytes) {
                    Delivery::Sent => {
                        sent[i] += datagram.payload.lines;
                        max_packed[i] = max_packed[i].max(datagram.payload.max_packed);
                    }
                    // Peer left mid-batch after Antithesis killed the SUT. Stop and
                    // report the partial batch rather than failing the run.
                    Delivery::Unavailable => break 'recv,
                    // Backpressure outlasted the retry budget. A legit Antithesis
                    // pause reaches here, so record it and stop rather than fail.
                    Delivery::Timeout => {
                        timed_out = true;
                        break 'recv;
                    }
                }
            }
        }
        Ok(Stats {
            received,
            sent,
            max_packed,
            timed_out,
        })
    });

    producer
        .join()
        .map_err(|_| anyhow::anyhow!("producer thread panicked"))?;
    consumer
        .join()
        .map_err(|_| anyhow::anyhow!("consumer thread panicked"))?
}

/// Outcome of delivering one line to a socket.
enum Delivery {
    /// The line reached the socket.
    Sent,
    /// The peer is gone. Stop the batch and report the partial result.
    Unavailable,
    /// Backpressure outlasted the retry budget. Fail the run.
    Timeout,
}

fn deliver(socket: &UnixDatagram, bytes: &[u8]) -> Delivery {
    let deadline = Instant::now() + SEND_RETRY_BUDGET;
    loop {
        match socket.send(bytes) {
            Ok(_) => return Delivery::Sent,
            Err(e) if is_transient(&e) => {
                if Instant::now() >= deadline {
                    return Delivery::Timeout;
                }
                sleep(SEND_RETRY_BACKOFF);
            }
            Err(_) => return Delivery::Unavailable,
        }
    }
}

fn is_transient(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
        || error.raw_os_error() == Some(libc::ENOBUFS)
}

/// Wait for the remote process to bind `path`, intentionally naive. Returns
/// `None` if the socket is still unavailable after 30 seconds.
#[must_use]
pub fn connect_with_retry(path: &Path) -> Option<UnixDatagram> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(socket) = UnixDatagram::unbound() {
            if socket.connect(path).is_ok() {
                return Some(socket);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(250));
    }
}

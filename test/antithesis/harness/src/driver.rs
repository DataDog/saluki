//! Shared `DogStatsD` load-driver engine.
//!
//! The engine fetches a working set of contexts from the shared intake pool, then a producer thread
//! renders per-occurrence values against them into a bounded channel while a consumer thread fans
//! each datagram out to every socket and tallies per-socket sends. Drivers differ only in how many
//! sockets they target and which anchors they fire, so both the single-socket and differential
//! drivers run on this one engine.
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
use rand::Rng;
use serde_json::json;

use crate::contexts::{decode_response, Context};
use crate::dogstatsd::is_malformed;
use crate::payload::dogstatsd;

const SEND_RETRY_BUDGET: Duration = Duration::from_secs(5);
const SEND_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// How long to keep retrying the context fetch before giving up and running no load this invocation.
const CONTEXT_FETCH_BUDGET: Duration = Duration::from_secs(30);
/// Backoff between context-fetch attempts.
const CONTEXT_FETCH_BACKOFF: Duration = Duration::from_millis(250);
/// Per-request timeout on a single context fetch.
const CONTEXT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A generated datagram queued for the sockets: the packed bytes and what they hold.
struct Datagram {
    /// The `\n`-packed bytes of one datagram, shipped in a single send.
    bytes: Vec<u8>,
    /// The lines and largest packed run in `bytes`.
    stats: dogstatsd::DatagramStats,
}

/// What a driver run shipped, for anchoring assertions.
#[derive(Clone, Debug)]
pub struct Stats {
    /// Payloads pulled from the channel, whether or not any send succeeded.
    pub received: usize,
    /// Lines delivered per socket, summed across datagrams, indexed as the sockets
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
    /// The zero result for `sockets` sockets: nothing received, nothing sent. Reported when the
    /// context pool is unreachable so a driver invocation degrades to a no-op rather than an error.
    fn empty(sockets: usize) -> Self {
        Self {
            received: 0,
            sent: vec![0; sockets],
            max_packed: vec![0; sockets],
            timed_out: false,
        }
    }
}

/// Pull `context_count` contexts from the intake pool at `intake_addr` once, then drive `count` datagrams
/// to every socket from that one pull, each datagram a fresh per-occurrence render of every context that
/// fits under `limit_bytes`, blocking through transient backpressure so every datagram reaches every
/// socket.
/// `context_count`, `count`, and `limit_bytes` come from a load generator's
/// [`crate::config::DriverConfig`], so a datagram never truncates on receive.
///
/// An unreachable pool ends the run with an empty [`Stats`] rather than an error, so a driver that
/// cannot reach the intake for its whole fetch budget degrades to a no-op. A peer that leaves mid-batch,
/// or backpressure that outlasts the retry budget, ends the run early with a partial [`Stats`].
///
/// # Errors
///
/// Errors if a worker thread panics, or if a reachable pool serves no contexts. Sustained backpressure
/// is reported via [`Stats::timed_out`], not as an error.
pub fn run<R: Rng + Send + 'static>(
    mut rng: R, intake_addr: &str, context_count: usize, limit_bytes: usize, count: usize, sockets: Vec<UnixDatagram>,
) -> anyhow::Result<Stats> {
    // One pull for the whole invocation, reused for every datagram. The pull is what decides whether
    // this invocation's datagrams carry a non-UTF-8 byte, and a run schedules many invocations, so the
    // fraction of pulls carrying one is the fraction of datagrams carrying one.
    let pull = match fetch_contexts(intake_addr, context_count)? {
        Some(contexts) => {
            // The pool serves exactly what was asked for or errors, so a body that decodes to no
            // contexts means the driver and the intake disagree about the request rather than that load
            // is unavailable. Shipping nothing on it would hide the disagreement.
            match dogstatsd::Pull::new(contexts) {
                Some(pull) => pull,
                None => anyhow::bail!("context pool served nothing for a request of {context_count} contexts"),
            }
        }
        // The intake stayed unreachable for the whole fetch budget, which is what an injected partition
        // looks like. No load this invocation, not a failure.
        None => return Ok(Stats::empty(sockets.len())),
    };

    let (tx, rx) = sync_channel::<Datagram>(2024);

    let producer = thread::spawn(move || {
        for _ in 0..count {
            let mut bytes = Vec::new();
            let stats = dogstatsd::write_datagram(&mut rng, &pull, &mut bytes, limit_bytes);
            // Green by construction: write_datagram packs only rendered lines the Agent forwards. The
            // anchor catches any drift that would ship a droppable datagram.
            assert_always!(
                is_malformed(&bytes).is_ok(),
                "driver datagram is well-formed",
                &json!({})
            );
            if tx.send(Datagram { bytes, stats }).is_err() {
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
                        sent[i] += datagram.stats.lines;
                        max_packed[i] = max_packed[i].max(datagram.stats.max_packed);
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

/// What a context fetch attempt came back with.
enum Fetched {
    /// The pool answered with contexts.
    Contexts(Vec<Context>),
    /// Nothing answered, or the body did not decode. Worth retrying.
    Unreachable,
    /// The pool answered and refused. Its own invariant broke, so retrying it is pointless.
    Refused(reqwest::StatusCode),
}

/// Fetch a pull of `n` contexts from the pool at `intake_addr` over blocking HTTP, retrying through
/// [`CONTEXT_FETCH_BUDGET`]. Returns `None` when the pool never answers, so the caller degrades to no
/// load rather than failing, which is what an injected partition looks like.
///
/// # Errors
///
/// Errors when the pool answers and refuses. A refusal means the intake decided it could not serve this
/// timeline's config at all, and shipping no load on it would hide that behind an idle driver.
fn fetch_contexts(intake_addr: &str, n: usize) -> anyhow::Result<Option<Vec<Context>>> {
    let url = format!("http://{intake_addr}/contexts?n={n}");
    let client = reqwest::blocking::Client::builder()
        .timeout(CONTEXT_FETCH_TIMEOUT)
        .build()
        .map_err(|e| anyhow::anyhow!("build the context-fetch client: {e}"))?;
    let deadline = Instant::now() + CONTEXT_FETCH_BUDGET;
    loop {
        match try_fetch_contexts(&client, &url) {
            Fetched::Contexts(contexts) => return Ok(Some(contexts)),
            Fetched::Refused(status) => {
                anyhow::bail!("context pool refused a request for {n} contexts with {status}")
            }
            Fetched::Unreachable => {}
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(CONTEXT_FETCH_BACKOFF);
    }
}

/// One context-fetch attempt. A transport error or a body that does not decode is retried. A non-success
/// status is not: the pool answered, so the fault is in the request or in the pool's own config, and no
/// amount of waiting fixes either.
fn try_fetch_contexts(client: &reqwest::blocking::Client, url: &str) -> Fetched {
    let Ok(response) = client.get(url).send() else {
        return Fetched::Unreachable;
    };
    let status = response.status();
    if !status.is_success() {
        return Fetched::Refused(status);
    }
    let Ok(body) = response.bytes() else {
        return Fetched::Unreachable;
    };
    match decode_response(&body) {
        Some(contexts) => Fetched::Contexts(contexts),
        None => Fetched::Unreachable,
    }
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

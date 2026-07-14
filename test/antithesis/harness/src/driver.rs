//! Shared `DogStatsD` load-driver engine.
//!
//! A producer thread samples lines into a bounded channel; a consumer thread
//! fans each line out to every socket and tallies per-socket sends. Drivers
//! differ only in how many sockets they target and which anchors they fire, so
//! both the single-socket and differential drivers run on this one engine.
//!
//! NOTE: this driver intentionally blocks on backpressure from the SUT. Retry
//! and backoff timers are meant to endure transient errors.

use std::io::ErrorKind;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use rand::seq::IndexedRandom;
use rand::Rng;

use crate::payload::dogstatsd;
pub use crate::payload::dogstatsd::Batch;

const SEND_RETRY_BUDGET: Duration = Duration::from_secs(5);
const SEND_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// Sample a line composition: half clean, a quarter feral, a quarter mixed.
#[must_use]
pub fn sample<R: Rng + ?Sized>(rng: &mut R) -> Batch {
    match [Batch::Clean, Batch::Clean, Batch::Feral, Batch::Mixed].choose(rng) {
        Some(Batch::Feral) => Batch::Feral,
        Some(Batch::Mixed) => Batch::Mixed,
        _ => Batch::Clean,
    }
}

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

/// Drive `count` sampled `DogStatsD` datagrams to every socket, packing each to
/// at most `limit_bytes` and blocking through transient backpressure so every
/// datagram reaches every socket. Both `count` and `limit_bytes` come from a load
/// generator's [`crate::config::DriverConfig`], so a datagram never truncates on
/// receive.
///
/// A peer that leaves mid-batch, or backpressure that outlasts the retry budget,
/// ends the run early with a partial [`Stats`] rather than an error.
///
/// # Errors
///
/// Errors if a worker thread panics. Sustained backpressure is reported via
/// [`Stats::timed_out`], not as an error.
pub fn run<R: Rng + Send + 'static>(
    mut rng: R, batch: Batch, limit_bytes: usize, count: usize, sockets: Vec<UnixDatagram>,
) -> anyhow::Result<Stats> {
    let (tx, rx) = sync_channel::<Datagram>(2024);

    let producer = thread::spawn(move || {
        for _ in 0..count {
            let mut bytes = Vec::new();
            let payload = dogstatsd::write_payload(&mut rng, &mut bytes, batch, limit_bytes);
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

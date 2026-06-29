//! Shared `DogStatsD` load-driver engine.
//!
//! A producer thread samples lines into a bounded channel; a consumer thread
//! fans each line out to every socket and tallies per-socket sends. Drivers
//! differ only in how many sockets they target and which anchors they fire, so
//! both the single-socket and differential drivers run on this one engine.

use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use antithesis_sdk::random::{random_choice, AntithesisRng};
use rand::{rand_core::UnwrapErr, RngExt};

use crate::payload::dogstatsd;

/// Per-batch composition: 50% clean, 25% feral, 25% mixed.
#[derive(Clone, Copy, Debug)]
pub enum Batch {
    /// Every line clean.
    Clean,
    /// Every line feral.
    Feral,
    /// A per-line clean-or-feral mix.
    Mixed,
}

impl Batch {
    /// Sample a batch composition: half clean, a quarter feral, a quarter mixed.
    #[must_use]
    pub fn sample() -> Self {
        match random_choice(&[Batch::Clean, Batch::Clean, Batch::Feral, Batch::Mixed]) {
            Some(Batch::Feral) => Batch::Feral,
            Some(Batch::Mixed) => Batch::Mixed,
            _ => Batch::Clean,
        }
    }

    /// The vibe for one line drawn from this batch.
    fn vibe(self) -> dogstatsd::Vibe {
        match self {
            Batch::Clean => dogstatsd::Vibe::Clean,
            Batch::Feral => dogstatsd::Vibe::Feral,
            Batch::Mixed => dogstatsd::sample_vibe(),
        }
    }
}

/// A generated dogstatsd line queued for the sockets.
enum Line {
    /// A single-value line.
    Single { bytes: Vec<u8> },
    /// A multi-value `:`-packed metric.
    Multi {
        /// The encoded line.
        bytes: Vec<u8>,
        /// The number of values in the packed run.
        count: usize,
    },
}

impl Line {
    /// The encoded bytes to ship over a socket.
    fn bytes(&self) -> &[u8] {
        match self {
            Line::Single { bytes } | Line::Multi { bytes, .. } => bytes,
        }
    }
}

/// What a driver run shipped, for anchoring assertions.
#[derive(Clone, Debug)]
pub struct Stats {
    /// Lines pulled from the channel, whether or not any send succeeded.
    pub received: usize,
    /// Successful sends per socket, indexed as the sockets were passed to [`run`].
    pub sent: Vec<usize>,
    /// Largest packed run that reached each socket, indexed likewise. Zero when
    /// no multi-value line reached that socket.
    pub max_packed: Vec<usize>,
}

/// Drive one batch of sampled `DogStatsD` lines to every socket.
///
/// A producer samples up to ~10k lines at the batch's vibe and queues them; a
/// consumer ships each line to every socket and tallies per-socket sends. The
/// returned [`Stats`] anchor the caller's assertions.
///
/// # Panics
///
/// Panics if the producer or consumer thread panics.
#[must_use]
pub fn run(batch: Batch, sockets: Vec<UnixDatagram>) -> Stats {
    let count = {
        let mut rng = UnwrapErr(AntithesisRng);
        rng.random_range(0..=10_000u64)
    };

    let (tx, rx) = sync_channel::<Line>(2024);

    let producer = thread::spawn(move || {
        let mut rng = UnwrapErr(AntithesisRng);
        for _ in 0..count {
            let mut bytes = Vec::new();
            let line = match dogstatsd::send(&mut rng, &mut bytes, batch.vibe()) {
                None => Line::Single { bytes },
                Some(count) => Line::Multi { bytes, count },
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let consumer = thread::spawn(move || {
        let mut received = 0usize;
        let mut sent = vec![0usize; sockets.len()];
        let mut max_packed = vec![0usize; sockets.len()];
        while let Ok(line) = rx.recv() {
            received += 1;
            for (i, socket) in sockets.iter().enumerate() {
                if socket.send(line.bytes()).is_ok() {
                    sent[i] += 1;
                    if let Line::Multi { count, .. } = &line {
                        max_packed[i] = max_packed[i].max(*count);
                    }
                }
            }
        }
        Stats {
            received,
            sent,
            max_packed,
        }
    });

    producer.join().expect("producer thread panicked");
    consumer.join().expect("consumer thread panicked")
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

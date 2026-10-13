//! The loopback witness: control datagrams that show when the host drops loopback datagrams before any reader sees
//! them.
//!
//! The host can drop UDP datagrams on their way over loopback to a socket whose reader keeps up: while another process
//! on the host floods UDP, macOS drops a share of every loopback socket's datagrams, and no counter of the receiving
//! process sees them. ADP's DogStatsD source then never receives some of the demo's datagrams, and the canary check
//! would take their canaries for canaries that ADP lost. So the traffic generator sends a control datagram of the same
//! size after each send of DogStatsD datagrams (one per datagram up to [`BURST_RATE`](crate::traffic::BURST_RATE), one
//! per burst above it) to a socket of the demo's own, which a task reads as the datagrams arrive
//! ([`LoopbackWitness::run`]). A control datagram that never arrives was dropped by the host, in the second it was
//! sent: the canary check counts those of the seconds it covers ([`LoopbackWitness::between`]).

use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use tokio::net::UdpSocket;

use crate::traffic::CANARY_HISTORY_SECS;

/// The bytes of a control datagram that hold the second it was sent in, big-endian, at its start.
const SECOND_BYTES: usize = 8;

/// The control datagrams sent in some seconds, and how many of them arrived.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ControlTally {
    /// The control datagrams sent.
    pub sent: u64,

    /// The control datagrams that arrived.
    pub received: u64,
}

impl ControlTally {
    /// Returns how many of the control datagrams sent did not arrive: those the host dropped.
    pub fn lost(&self) -> u64 {
        self.sent.saturating_sub(self.received)
    }
}

/// The control datagrams sent and received in each second (Unix seconds), over the last [`CANARY_HISTORY_SECS`].
#[derive(Debug, Default)]
struct ControlCounts(BTreeMap<u64, ControlTally>);

impl ControlCounts {
    fn record_sent(&mut self, second: u64) {
        self.0.entry(second).or_default().sent += 1;
        self.forget_before(second.saturating_sub(CANARY_HISTORY_SECS));
    }

    /// Counts a control datagram sent in `second` as arrived, unless the second has left the history.
    fn record_received(&mut self, second: u64) {
        let oldest = self
            .0
            .keys()
            .next_back()
            .map_or(0, |newest| newest.saturating_sub(CANARY_HISTORY_SECS));
        if second >= oldest {
            self.0.entry(second).or_default().received += 1;
        }
    }

    fn forget_before(&mut self, oldest: u64) {
        while self.0.first_key_value().is_some_and(|(second, _)| *second < oldest) {
            self.0.pop_first();
        }
    }

    fn between(&self, from_s: u64, to_s: u64) -> ControlTally {
        self.0
            .range(from_s..to_s.max(from_s))
            .fold(ControlTally::default(), |total, (_, tally)| ControlTally {
                sent: total.sent + tally.sent,
                received: total.received + tally.received,
            })
    }
}

/// The loopback witness: the socket control datagrams are sent to, and their counts.
///
/// Cheap to clone: clones share the socket and the counts.
#[derive(Clone)]
pub struct LoopbackWitness {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    counts: Arc<Mutex<ControlCounts>>,
}

impl LoopbackWitness {
    /// Binds the witness's socket, on an ephemeral port of the loopback address.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound or its address read.
    pub async fn bind() -> io::Result<Self> {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await?;
        let target = socket.local_addr()?;
        Ok(Self {
            socket: Arc::new(socket),
            target,
            counts: Arc::new(Mutex::new(ControlCounts::default())),
        })
    }

    /// Returns the address control datagrams are sent to.
    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Returns the payload of a control datagram sent in `second`, `len` bytes long (at least the bytes that hold the
    /// second), so that it is the size of the datagrams it stands beside.
    pub fn payload(second: u64, len: usize) -> Vec<u8> {
        let mut payload = vec![b'.'; len.max(SECOND_BYTES)];
        payload[..SECOND_BYTES].copy_from_slice(&second.to_be_bytes());
        payload
    }

    /// Counts a control datagram as sent in `second`.
    pub fn record_sent(&self, second: u64) {
        self.lock().record_sent(second);
    }

    /// Returns the control datagrams sent from second `from_s` up to, not including, second `to_s`, and how many of
    /// them arrived.
    pub fn between(&self, from_s: u64, to_s: u64) -> ControlTally {
        self.lock().between(from_s, to_s)
    }

    /// Reads control datagrams as they arrive, counting each in the second it was sent in, until the task is dropped.
    pub async fn run(self) {
        let mut buffer = vec![0; 65_536];
        loop {
            let Ok((len, _)) = self.socket.recv_from(&mut buffer).await else {
                continue;
            };
            let Some(second) = buffer[..len]
                .get(..SECOND_BYTES)
                .and_then(|bytes| <[u8; SECOND_BYTES]>::try_from(bytes).ok())
                .map(u64::from_be_bytes)
            else {
                continue;
            };
            self.lock().record_received(second);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlCounts> {
        self.counts.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn control_datagrams_are_counted_in_the_second_they_were_sent_and_those_that_never_arrive_are_lost() {
        let mut counts = ControlCounts::default();
        for second in [100, 100, 100, 101, 103] {
            counts.record_sent(second);
        }
        // Two of second 100's arrive, late ones included, and second 101's; second 103's never does.
        for second in [100, 101, 100] {
            counts.record_received(second);
        }
        assert_eq!(counts.between(100, 101), ControlTally { sent: 3, received: 2 });
        assert_eq!(counts.between(100, 101).lost(), 1);
        assert_eq!(counts.between(99, 104), ControlTally { sent: 5, received: 3 });
        assert_eq!(counts.between(99, 104).lost(), 2);
        assert_eq!(counts.between(101, 103).lost(), 0);
        assert_eq!(counts.between(104, 200), ControlTally::default());
        assert_eq!(counts.between(103, 100), ControlTally::default());

        // Old seconds leave the history, and a datagram of a second that left it is not counted.
        counts.record_sent(101 + CANARY_HISTORY_SECS);
        assert_eq!(counts.between(0, 101), ControlTally::default());
        counts.record_received(100);
        assert_eq!(counts.between(0, 101), ControlTally::default());
        assert_eq!(counts.between(101, 102), ControlTally { sent: 1, received: 1 });
    }

    #[test]
    fn a_control_payload_holds_its_second_and_has_the_size_asked_for() {
        let payload = LoopbackWitness::payload(1_791_822_140, 640);
        assert_eq!(payload.len(), 640);
        assert_eq!(u64::from_be_bytes(payload[..8].try_into().unwrap()), 1_791_822_140);
        assert_eq!(LoopbackWitness::payload(7, 3).len(), SECOND_BYTES);
    }

    #[tokio::test]
    async fn the_witness_counts_the_control_datagrams_that_arrive() {
        let witness = LoopbackWitness::bind().await.unwrap();
        let task = tokio::spawn(witness.clone().run());
        let sender = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        for second in [500, 500, 501] {
            sender
                .send_to(&LoopbackWitness::payload(second, 600), witness.target())
                .await
                .unwrap();
            witness.record_sent(second);
        }
        // A datagram too short to hold a second is not one of the generator's, and is ignored.
        sender.send_to(b"short", witness.target()).await.unwrap();
        let arrived = async {
            while witness.between(500, 502).received < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .expect("every control datagram arrives over loopback");
        assert_eq!(witness.between(500, 502), ControlTally { sent: 3, received: 3 });
        assert_eq!(witness.between(500, 501), ControlTally { sent: 2, received: 2 });
        task.abort();
    }
}

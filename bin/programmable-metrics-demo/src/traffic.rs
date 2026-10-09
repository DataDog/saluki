//! The traffic generator: DogStatsD datagrams over UDP at a steady rate.
//!
//! Every datagram carries the same set of lines, so each metric arrives at the datagram rate:
//!
//! - `demo.checkout.requests:1|c|#team:checkout` and `demo.checkout.latency:<x>|d|#team:checkout`, routed to team B
//!   by the default program's prefix allowlist;
//! - `demo.catalog.views:1|c|#team:catalog`, routed to team C;
//! - `demo.platform.queue_depth:<x>|g|#team:platform`, which only the primary endpoint receives;
//! - twenty noisy counters `demo.debug.0` through `demo.debug.19`;
//! - `demo.canary:1|c`, exactly once per datagram, so that the canary total an intake receives can be compared with
//!   the number of datagrams sent (conservation). The generator counts the canaries it sends in each second of wall
//!   clock time, which is the second ADP's aggregation assigns them to.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;
use tokio::{net::UdpSocket, sync::Notify, time::MissedTickBehavior};

use crate::clock::unix_millis;

/// How many seconds of canary counts the generator keeps.
pub const CANARY_HISTORY_SECS: u64 = 300;

/// How many noisy `demo.debug.<n>` counters each datagram carries.
pub const DEBUG_COUNTERS: u64 = 20;

/// The lowest datagram rate the generator accepts, in datagrams per second.
pub const MIN_RATE: f64 = 0.1;

/// The highest datagram rate the generator accepts, in datagrams per second.
pub const MAX_RATE: f64 = 1000.0;

/// The datagram rate used when a requested rate is not a finite number, in datagrams per second.
pub const DEFAULT_RATE: f64 = 20.0;

/// Returns the DogStatsD payload of the datagram with the given sequence number.
///
/// The latency and queue depth values vary with the sequence number, deterministically, so that the payload is a pure
/// function of it.
pub fn datagram(seq: u64) -> String {
    let latency = 20 + (seq * 37) % 80;
    let queue_depth = 50 + (seq / 5) % 20;
    let mut payload = String::with_capacity(640);
    let _ = writeln!(payload, "demo.checkout.requests:1|c|#team:checkout");
    let _ = writeln!(payload, "demo.checkout.latency:{}|d|#team:checkout", latency);
    let _ = writeln!(payload, "demo.catalog.views:1|c|#team:catalog");
    let _ = writeln!(payload, "demo.platform.queue_depth:{}|g|#team:platform", queue_depth);
    for n in 0..DEBUG_COUNTERS {
        let _ = writeln!(payload, "demo.debug.{}:1|c", n);
    }
    let _ = write!(payload, "demo.canary:1|c");
    payload
}

/// The generator's state, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrafficState {
    /// Whether the generator is sending.
    pub running: bool,

    /// The datagram rate, in datagrams per second.
    pub rate: f64,

    /// Where datagrams are sent.
    pub target: String,

    /// Datagrams sent so far.
    pub datagrams_sent: u64,

    /// Canary counts sent so far (one per datagram).
    pub canaries_sent: u64,

    /// Datagrams that could not be sent.
    pub send_errors: u64,

    /// The most recent send error.
    pub last_error: Option<String>,
}

/// A change to the generator's settings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrafficChange {
    /// Start (`true`) or pause (`false`) sending.
    pub running: Option<bool>,

    /// The new datagram rate, in datagrams per second, clamped to [`MIN_RATE`]..=[`MAX_RATE`].
    pub rate: Option<f64>,
}

/// The traffic generator.
///
/// Cheap to clone: clones control the same generator.
#[derive(Clone)]
pub struct TrafficGenerator {
    state: Arc<Mutex<TrafficState>>,
    canaries: Arc<Mutex<CanaryCounts>>,
    changed: Arc<Notify>,
    target: SocketAddr,
}

/// Canaries sent per second of wall clock time, as `(unix second, canaries)`, oldest first.
#[derive(Default)]
struct CanaryCounts(VecDeque<(u64, u64)>);

impl CanaryCounts {
    fn record(&mut self, second: u64) {
        match self.0.back_mut() {
            Some((last, count)) if *last >= second => *count += 1,
            _ => self.0.push_back((second, 1)),
        }
        while self
            .0
            .front()
            .is_some_and(|(first, _)| first + CANARY_HISTORY_SECS <= second)
        {
            self.0.pop_front();
        }
    }

    fn between(&self, from_s: u64, to_s: u64) -> u64 {
        self.0
            .iter()
            .filter(|(second, _)| (from_s..to_s).contains(second))
            .map(|(_, count)| count)
            .sum()
    }
}

impl TrafficGenerator {
    /// Creates a generator that sends to `target` at `rate` datagrams per second once started.
    ///
    /// The rate is clamped to [`MIN_RATE`]..=[`MAX_RATE`]; a rate that is not a finite number is [`DEFAULT_RATE`].
    pub fn new(target: SocketAddr, rate: f64, running: bool) -> Self {
        let rate = if rate.is_finite() { rate } else { DEFAULT_RATE };
        Self {
            state: Arc::new(Mutex::new(TrafficState {
                running,
                rate: rate.clamp(MIN_RATE, MAX_RATE),
                target: target.to_string(),
                datagrams_sent: 0,
                canaries_sent: 0,
                send_errors: 0,
                last_error: None,
            })),
            canaries: Arc::new(Mutex::new(CanaryCounts::default())),
            changed: Arc::new(Notify::new()),
            target,
        }
    }

    /// Returns how many canaries were sent from second `from_s` up to, not including, second `to_s` (Unix seconds),
    /// within the last [`CANARY_HISTORY_SECS`].
    pub fn canaries_sent_between(&self, from_s: u64, to_s: u64) -> u64 {
        self.canaries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .between(from_s, to_s)
    }

    /// Returns the generator's state.
    pub fn state(&self) -> TrafficState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Applies a change, and returns the resulting state.
    ///
    /// A rate that is not a finite number is ignored.
    pub fn apply(&self, change: TrafficChange) -> TrafficState {
        let state = {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(running) = change.running {
                state.running = running;
            }
            if let Some(rate) = change.rate.filter(|rate| rate.is_finite()) {
                state.rate = rate.clamp(MIN_RATE, MAX_RATE);
            }
            state.clone()
        };
        self.changed.notify_one();
        state
    }

    /// Sends datagrams until the task is dropped, following the generator's settings.
    pub async fn run(self) {
        let socket = match UdpSocket::bind(("127.0.0.1", 0)).await {
            Ok(socket) => socket,
            Err(error) => {
                let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                state.last_error = Some(format!("Failed to bind a UDP socket for DogStatsD traffic: {}", error));
                return;
            }
        };

        let mut seq = 0u64;
        loop {
            let (running, rate) = {
                let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.running, state.rate)
            };
            if !running {
                self.changed.notified().await;
                continue;
            }

            let mut ticks = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    _ = self.changed.notified() => break,
                }
                let payload = datagram(seq);
                seq += 1;
                let result = socket.send_to(payload.as_bytes(), self.target).await;
                if result.is_ok() {
                    self.canaries
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .record(unix_millis() / 1000);
                }
                let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                match result {
                    Ok(_) => {
                        state.datagrams_sent += 1;
                        state.canaries_sent += 1;
                    }
                    Err(error) => {
                        state.send_errors += 1;
                        state.last_error = Some(error.to_string());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagrams_carry_the_demo_metric_set_with_one_canary() {
        let payload = datagram(3);
        let lines = payload.lines().collect::<Vec<_>>();
        let mut expected = vec![
            "demo.checkout.requests:1|c|#team:checkout".to_string(),
            // 20 + (3 * 37) % 80 = 20 + 31 = 51
            "demo.checkout.latency:51|d|#team:checkout".to_string(),
            "demo.catalog.views:1|c|#team:catalog".to_string(),
            // 50 + (3 / 5) % 20 = 50
            "demo.platform.queue_depth:50|g|#team:platform".to_string(),
        ];
        expected.extend((0..20).map(|n| format!("demo.debug.{}:1|c", n)));
        expected.push("demo.canary:1|c".to_string());
        assert_eq!(lines, expected);
        assert!(!payload.ends_with('\n'));
        assert_eq!(payload.matches("demo.canary:").count(), 1);
    }

    #[tokio::test]
    async fn the_generator_sends_one_canary_per_datagram_and_pauses_on_request() {
        let receiver = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let generator = TrafficGenerator::new(receiver.local_addr().unwrap(), 200.0, true);
        let task = tokio::spawn(generator.clone().run());

        let mut buffer = vec![0; 2048];
        for expected_seq in 0..3 {
            let (len, _) = receiver.recv_from(&mut buffer).await.unwrap();
            assert_eq!(
                String::from_utf8(buffer[..len].to_vec()).unwrap(),
                datagram(expected_seq)
            );
        }

        let paused = generator.apply(TrafficChange {
            running: Some(false),
            rate: Some(5000.0),
        });
        assert!(!paused.running);
        assert_eq!(paused.rate, MAX_RATE);
        // Let the generator observe the pause, then check that sending stopped.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sent = generator.state().datagrams_sent;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state = generator.state();
        assert_eq!(state.datagrams_sent, sent);
        assert_eq!(state.canaries_sent, state.datagrams_sent);
        assert!(state.datagrams_sent >= 3);
        // Every canary sent is counted in the second it was sent.
        let now_s = unix_millis() / 1000;
        assert_eq!(
            generator.canaries_sent_between(now_s - 10, now_s + 1),
            state.canaries_sent
        );
        task.abort();
    }

    #[test]
    fn canaries_are_counted_per_second_over_a_bounded_history() {
        let mut counts = CanaryCounts::default();
        for second in [100, 100, 101, 103, 103, 103] {
            counts.record(second);
        }
        assert_eq!(counts.between(100, 104), 6);
        assert_eq!(counts.between(101, 103), 1);
        assert_eq!(counts.between(103, 104), 3);
        assert_eq!(counts.between(104, 200), 0);

        // Old seconds leave the history.
        counts.record(100 + CANARY_HISTORY_SECS);
        assert_eq!(counts.between(0, 101), 0);
        assert_eq!(counts.between(101, 104), 4);
        assert_eq!(counts.0.len(), 3);
    }
}

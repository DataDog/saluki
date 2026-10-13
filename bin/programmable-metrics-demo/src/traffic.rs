//! The traffic generator: DogStatsD datagrams over UDP at a steady rate.
//!
//! Every datagram carries the same set of lines, so each metric arrives at the datagram rate:
//!
//! - `demo.checkout.requests:1|c|#team:checkout` and `demo.checkout.latency:<x>|d|#team:checkout`, routed to team B
//!   by the default program's prefix allowlist;
//! - `demo.catalog.views:1|c|#team:catalog`, routed to team C;
//! - `demo.platform.queue_depth:<x>|g|#team:platform`, which only the primary endpoint receives;
//! - twenty noisy counters `demo.debug.0` through `demo.debug.19`;
//! - with load contexts (`--contexts N`), [`LOAD_LINES`] lines `demo.load:1|c|#context:<k>`, `k` cycling through `0`
//!   to `N - 1` from one datagram to the next, so that many contexts spread over the state lanes;
//! - `demo.canary:1|c|#shard:<s>`, exactly once per datagram, so that the canary total an intake receives can be
//!   compared with the number of datagrams sent (conservation). The generator counts the canaries it sends in each
//!   second of wall clock time, which is the second ADP's aggregation assigns them to. `s` cycles through `0` to
//!   [`CANARY_SHARDS`]` - 1` from one datagram to the next: the canary is that many contexts, which aggregation's state
//!   lanes share by key, so that the conservation check covers every state lane (with two lanes, all sixteen land in
//!   one lane once in 32,768 launches). The intakes count canaries by name, whatever the shard.
//!
//! With the canary-only mix ([`TrafficMix::CanaryOnly`]), datagrams carry the canary alone, of shard 0 only: a single
//! context, which one state lane holds while the others empty, as the lanes check's empty-lane step needs.
//!
//! Above [`BURST_RATE`] datagrams per second the generator sends in bursts, one every [`BURST_PERIOD`], so that rates
//! of tens of thousands of datagrams per second do not depend on the timer's resolution.
//!
//! With a [loopback witness](crate::loopback), the generator also sends a control datagram, of the same size, after
//! each datagram, or each burst, so that the canary check can tell datagrams the host dropped before ADP read them.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{net::UdpSocket, sync::Notify, time::MissedTickBehavior};

use crate::{
    clock::unix_millis,
    loopback::{ControlTally, LoopbackWitness},
};

/// How many seconds of canary counts the generator keeps.
pub const CANARY_HISTORY_SECS: u64 = 300;

/// How many noisy `demo.debug.<n>` counters each datagram carries.
pub const DEBUG_COUNTERS: u64 = 20;

/// How many load lines each datagram carries when the generator has load contexts.
pub const LOAD_LINES: u64 = 10;

/// The most load contexts the generator cycles through.
pub const MAX_CONTEXTS: u64 = 1_000_000;

/// The lowest datagram rate the generator accepts, in datagrams per second.
pub const MIN_RATE: f64 = 0.1;

/// The highest datagram rate the generator accepts, in datagrams per second.
pub const MAX_RATE: f64 = 50_000.0;

/// The datagram rate used when a requested rate is not a finite number, in datagrams per second.
pub const DEFAULT_RATE: f64 = 20.0;

/// How many contexts the canary is spread over in the full mix, by its `shard` tag.
pub const CANARY_SHARDS: u64 = 16;

/// The datagram rate above which the generator sends in bursts, in datagrams per second.
pub const BURST_RATE: f64 = 200.0;

/// How often the generator sends a burst above [`BURST_RATE`].
pub const BURST_PERIOD: Duration = Duration::from_millis(5);

/// Which lines the generator's datagrams carry.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficMix {
    /// The demo's whole metric set, with the load contexts, if any, and the canary.
    #[default]
    Full,

    /// The canary alone, of one shard: a single context.
    CanaryOnly,
}

/// Returns the DogStatsD payload of the datagram with the given sequence number, with the lines of `mix` and, for the
/// full mix, [`LOAD_LINES`] of `contexts` load contexts (none if `contexts` is zero). The canary is of shard `seq`
/// modulo [`CANARY_SHARDS`] in the full mix, and of shard 0 in the canary-only mix.
///
/// The latency and queue depth values vary with the sequence number, deterministically, so that the payload is a pure
/// function of its arguments.
pub fn datagram_of(seq: u64, mix: TrafficMix, contexts: u64) -> String {
    let mut payload = String::with_capacity(1024);
    if mix == TrafficMix::Full {
        let latency = 20 + (seq * 37) % 80;
        let queue_depth = 50 + (seq / 5) % 20;
        let _ = writeln!(payload, "demo.checkout.requests:1|c|#team:checkout");
        let _ = writeln!(payload, "demo.checkout.latency:{}|d|#team:checkout", latency);
        let _ = writeln!(payload, "demo.catalog.views:1|c|#team:catalog");
        let _ = writeln!(payload, "demo.platform.queue_depth:{}|g|#team:platform", queue_depth);
        for n in 0..DEBUG_COUNTERS {
            let _ = writeln!(payload, "demo.debug.{}:1|c", n);
        }
        for line in 0..LOAD_LINES.min(contexts) {
            let context = (seq.wrapping_mul(LOAD_LINES).wrapping_add(line)) % contexts;
            let _ = writeln!(payload, "demo.load:1|c|#context:{}", context);
        }
    }
    let shard = match mix {
        TrafficMix::Full => seq % CANARY_SHARDS,
        TrafficMix::CanaryOnly => 0,
    };
    let _ = write!(payload, "demo.canary:1|c|#shard:{}", shard);
    payload
}

/// The generator's state, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrafficState {
    /// Whether the generator is sending.
    pub running: bool,

    /// The datagram rate, in datagrams per second.
    pub rate: f64,

    /// Which lines the datagrams carry.
    pub mix: TrafficMix,

    /// How many load contexts the full mix cycles through.
    pub contexts: u64,

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

    /// The lines the datagrams carry from now on.
    pub mix: Option<TrafficMix>,
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
    witness: Option<LoopbackWitness>,
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
        self.by_second(from_s, to_s).iter().map(|(_, count)| count).sum()
    }

    /// Returns how many were sent from `from_ms` up to `to_ms` (milliseconds since the Unix epoch), counting a second
    /// that the span covers in part in proportion: the generator sends at a steady rate, so the count is off by at most
    /// a datagram at each end.
    fn between_ms(&self, from_ms: u64, to_ms: u64) -> f64 {
        self.0
            .iter()
            .map(|&(second, count)| {
                let start = second * 1000;
                let covered = to_ms.min(start + 1000).saturating_sub(from_ms.max(start));
                count as f64 * covered as f64 / 1000.0
            })
            .sum()
    }

    fn by_second(&self, from_s: u64, to_s: u64) -> Vec<(u64, u64)> {
        self.0
            .iter()
            .filter(|(second, _)| (from_s..to_s).contains(second))
            .copied()
            .collect()
    }
}

/// Returns how often the generator sends at `rate` datagrams per second, and how many datagrams each send is worth on
/// average: one datagram per period up to [`BURST_RATE`], and a burst every [`BURST_PERIOD`] above it.
fn send_schedule(rate: f64) -> (Duration, f64) {
    if rate <= BURST_RATE {
        (Duration::from_secs_f64(1.0 / rate), 1.0)
    } else {
        (BURST_PERIOD, rate * BURST_PERIOD.as_secs_f64())
    }
}

impl TrafficGenerator {
    /// Creates a generator that sends to `target` at `rate` datagrams per second once started, its full mix cycling
    /// through `contexts` load contexts (none if zero).
    ///
    /// The rate is clamped to [`MIN_RATE`]..=[`MAX_RATE`]; a rate that is not a finite number is [`DEFAULT_RATE`]. The
    /// load contexts are at most [`MAX_CONTEXTS`].
    pub fn new(target: SocketAddr, rate: f64, running: bool, contexts: u64) -> Self {
        let rate = if rate.is_finite() { rate } else { DEFAULT_RATE };
        Self {
            state: Arc::new(Mutex::new(TrafficState {
                running,
                rate: rate.clamp(MIN_RATE, MAX_RATE),
                mix: TrafficMix::Full,
                contexts: contexts.min(MAX_CONTEXTS),
                target: target.to_string(),
                datagrams_sent: 0,
                canaries_sent: 0,
                send_errors: 0,
                last_error: None,
            })),
            canaries: Arc::new(Mutex::new(CanaryCounts::default())),
            changed: Arc::new(Notify::new()),
            target,
            witness: None,
        }
    }

    /// Makes the generator send a control datagram to `witness` after each datagram it sends, or each burst above
    /// [`BURST_RATE`] (see [`loopback`](crate::loopback)).
    pub fn with_witness(mut self, witness: LoopbackWitness) -> Self {
        self.witness = Some(witness);
        self
    }

    /// Returns how many datagrams were sent from `from_ms` up to `to_ms` (milliseconds since the Unix epoch), within
    /// the last [`CANARY_HISTORY_SECS`], to within a datagram at each end (see [`CanaryCounts::between_ms`]).
    pub fn datagrams_sent_between_ms(&self, from_ms: u64, to_ms: u64) -> f64 {
        self.canaries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .between_ms(from_ms, to_ms)
    }

    /// Returns the control datagrams sent from second `from_s` up to, not including, second `to_s`, and how many of
    /// them arrived, if the generator has a loopback witness.
    pub fn control_between(&self, from_s: u64, to_s: u64) -> Option<ControlTally> {
        self.witness.as_ref().map(|witness| witness.between(from_s, to_s))
    }

    /// Returns how many canaries were sent from second `from_s` up to, not including, second `to_s` (Unix seconds),
    /// within the last [`CANARY_HISTORY_SECS`].
    pub fn canaries_sent_between(&self, from_s: u64, to_s: u64) -> u64 {
        self.canaries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .between(from_s, to_s)
    }

    /// Returns how many canaries were sent in each second from `from_s` up to, not including, `to_s`, as `(second,
    /// canaries)`: one entry per second that sent any.
    pub fn canaries_sent_by_second(&self, from_s: u64, to_s: u64) -> Vec<(u64, u64)> {
        self.canaries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_second(from_s, to_s)
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
            if let Some(mix) = change.mix {
                state.mix = mix;
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
            let (running, rate, mix, contexts) = {
                let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.running, state.rate, state.mix, state.contexts)
            };
            if !running {
                self.changed.notified().await;
                continue;
            }

            let (period, per_tick) = send_schedule(rate);
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The datagrams owed: each tick's share of the rate, the fraction carried over to the next tick.
            let mut owed = 0.0;
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    _ = self.changed.notified() => break,
                }
                owed += per_tick;
                let mut last_len = None;
                while owed >= 1.0 {
                    owed -= 1.0;
                    let payload = datagram_of(seq, mix, contexts);
                    seq += 1;
                    self.send(&socket, payload.as_bytes()).await;
                    last_len = Some(payload.len());
                }
                if let (Some(witness), Some(len)) = (&self.witness, last_len) {
                    let second = unix_millis() / 1000;
                    witness.record_sent(second);
                    // A control datagram that cannot be sent is one the host dropped: it counts as lost.
                    let _ = socket
                        .send_to(&LoopbackWitness::payload(second, len), witness.target())
                        .await;
                }
            }
        }
    }

    /// Sends one datagram, and counts it, with its canary, or its error.
    async fn send(&self, socket: &UdpSocket, payload: &[u8]) {
        let result = socket.send_to(payload, self.target).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(seq: u64) -> String {
        datagram_of(seq, TrafficMix::Full, 0)
    }

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
        expected.push("demo.canary:1|c|#shard:3".to_string());
        assert_eq!(lines, expected);
        assert!(!payload.ends_with('\n'));
        assert_eq!(payload.matches("demo.canary:").count(), 1);
    }

    #[test]
    fn the_canary_cycles_through_its_shards_and_the_canary_only_mix_keeps_to_one() {
        let shard = |seq: u64, mix: TrafficMix| {
            let payload = datagram_of(seq, mix, 0);
            let canary = payload.lines().last().unwrap().to_string();
            canary
                .strip_prefix("demo.canary:1|c|#shard:")
                .unwrap()
                .parse::<u64>()
                .unwrap()
        };
        // Sixteen datagrams in a row carry each shard once, and the next sixteen again.
        let shards = (0..2 * CANARY_SHARDS)
            .map(|seq| shard(seq, TrafficMix::Full))
            .collect::<Vec<_>>();
        let once = (0..CANARY_SHARDS).collect::<Vec<_>>();
        assert_eq!(shards[..16], once[..]);
        assert_eq!(shards[16..], once[..]);
        assert_eq!(shard(1_000_005, TrafficMix::Full), 5);
        // The canary-only mix: one context, whatever the datagram.
        assert!((0..2 * CANARY_SHARDS).all(|seq| shard(seq, TrafficMix::CanaryOnly) == 0));
    }

    #[test]
    fn load_contexts_cycle_ten_per_datagram_and_the_canary_only_mix_carries_the_canary_alone() {
        let load = |seq: u64, contexts: u64| {
            datagram_of(seq, TrafficMix::Full, contexts)
                .lines()
                .filter_map(|line| line.strip_prefix("demo.load:1|c|#context:"))
                .map(|context| context.parse::<u64>().unwrap())
                .collect::<Vec<_>>()
        };
        // 25 contexts: datagram 0 carries 0 to 9, datagram 1 carries 10 to 19, datagram 2 carries 20 to 24 and 0 to 4.
        assert_eq!(load(0, 25), (0..10).collect::<Vec<_>>());
        assert_eq!(load(1, 25), (10..20).collect::<Vec<_>>());
        assert_eq!(load(2, 25), vec![20, 21, 22, 23, 24, 0, 1, 2, 3, 4]);
        // Fewer contexts than lines: each datagram carries each context once.
        assert_eq!(load(7, 3), vec![1, 2, 0]);
        assert!(load(7, 0).is_empty());
        // The load lines come after the metric set and before the canary, which stays last and alone of its kind.
        let payload = datagram_of(1, TrafficMix::Full, 25);
        assert_eq!(payload.lines().count(), 24 + 10 + 1);
        assert!(payload.ends_with("\ndemo.canary:1|c|#shard:1"));
        assert!(payload.len() < 1024, "{} bytes", payload.len());

        assert_eq!(datagram_of(5, TrafficMix::CanaryOnly, 25), "demo.canary:1|c|#shard:0");
    }

    #[test]
    fn rates_above_the_burst_rate_are_sent_in_bursts_and_below_it_one_datagram_at_a_time() {
        assert_eq!(send_schedule(20.0), (Duration::from_millis(50), 1.0));
        assert_eq!(send_schedule(BURST_RATE), (Duration::from_millis(5), 1.0));
        assert_eq!(send_schedule(20_000.0), (BURST_PERIOD, 100.0));
        assert_eq!(send_schedule(MAX_RATE), (BURST_PERIOD, 250.0));
    }

    #[tokio::test]
    async fn the_generator_sends_one_canary_per_datagram_and_pauses_on_request() {
        let receiver = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let generator = TrafficGenerator::new(receiver.local_addr().unwrap(), 200.0, true, 0);
        let task = tokio::spawn(generator.clone().run());

        let mut buffer = vec![0; 2048];
        for expected_seq in 0..3 {
            let (len, _) = receiver.recv_from(&mut buffer).await.unwrap();
            assert_eq!(
                String::from_utf8(buffer[..len].to_vec()).unwrap(),
                datagram(expected_seq)
            );
        }

        // The canary-only mix applies from the next datagram on.
        let canary_only = generator.apply(TrafficChange {
            mix: Some(TrafficMix::CanaryOnly),
            ..TrafficChange::default()
        });
        assert_eq!(canary_only.mix, TrafficMix::CanaryOnly);
        let canary_only = async {
            loop {
                let (len, _) = receiver.recv_from(&mut buffer).await.unwrap();
                if buffer[..len] == *b"demo.canary:1|c|#shard:0" {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), canary_only)
            .await
            .expect("a datagram carries the canary alone once the mix is canary-only");

        let paused = generator.apply(TrafficChange {
            running: Some(false),
            rate: Some(500_000.0),
            mix: None,
        });
        assert!(!paused.running);
        assert_eq!(paused.rate, MAX_RATE);
        assert_eq!(paused.mix, TrafficMix::CanaryOnly);
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

    #[tokio::test]
    async fn with_a_witness_the_generator_sends_a_control_datagram_per_datagram_up_to_the_burst_rate() {
        let receiver = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let witness = LoopbackWitness::bind().await.unwrap();
        let witness_task = tokio::spawn(witness.clone().run());
        let generator = TrafficGenerator::new(receiver.local_addr().unwrap(), 200.0, true, 0).with_witness(witness);
        let task = tokio::spawn(generator.clone().run());

        let mut buffer = vec![0; 2048];
        for _ in 0..5 {
            receiver.recv_from(&mut buffer).await.unwrap();
        }
        generator.apply(TrafficChange {
            running: Some(false),
            ..TrafficChange::default()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sent = generator.state().datagrams_sent;
        let now_s = unix_millis() / 1000;
        let arrived = async {
            while generator.control_between(now_s - 10, now_s + 1).unwrap().received < sent {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .expect("every control datagram arrives over loopback");
        // One control datagram per datagram at this rate, every one received: the host dropped none.
        let control = generator.control_between(now_s - 10, now_s + 1).unwrap();
        assert_eq!(control.sent, sent);
        assert_eq!(control.lost(), 0);
        // Without a witness, the generator has no control counts.
        assert_eq!(
            TrafficGenerator::new(receiver.local_addr().unwrap(), 20.0, false, 0).control_between(0, now_s + 1),
            None
        );
        task.abort();
        witness_task.abort();
    }

    #[test]
    fn datagrams_sent_over_milliseconds_count_seconds_covered_in_part_in_proportion() {
        let mut counts = CanaryCounts::default();
        for _ in 0..20 {
            counts.record(100);
        }
        for _ in 0..10 {
            counts.record(101);
        }
        assert_eq!(counts.between_ms(100_000, 102_000), 30.0);
        // Half of second 100's twenty, and half of second 101's ten.
        assert_eq!(counts.between_ms(100_500, 101_500), 15.0);
        assert_eq!(counts.between_ms(99_000, 100_250), 5.0);
        assert_eq!(counts.between_ms(100_750, 100_750), 0.0);
        assert_eq!(counts.between_ms(101_500, 100_500), 0.0);
        assert_eq!(counts.between_ms(102_000, 109_000), 0.0);
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
        assert_eq!(counts.by_second(100, 104), [(100, 2), (101, 1), (103, 3)]);

        // Old seconds leave the history.
        counts.record(100 + CANARY_HISTORY_SECS);
        assert_eq!(counts.between(0, 101), 0);
        assert_eq!(counts.between(101, 104), 4);
        assert_eq!(counts.0.len(), 3);
    }
}

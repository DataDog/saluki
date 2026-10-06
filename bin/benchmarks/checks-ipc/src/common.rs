use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use datadog_checks_protocol::{Log, Message, Metric};
use datadog_protos::checks::{self as proto, check_data};
use serde::{Deserialize, Serialize};

pub const METRIC_NAME: &str = "ipc.benchmark.gauge";
pub const TAGS: [&str; 5] = [
    "env:benchmark",
    "team:agent",
    "region:local",
    "source:checks",
    "kind:ipc",
];
pub const HOSTNAME: &str = "ipc-benchmark-host";
pub const TIMESTAMP: u64 = 1_700_000_000;
pub const MAX_SEQUENCE: u64 = 1 << 53;
pub const CATCH_UP_NS: u64 = 10_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Fit,
    Grpc,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Workload {
    Metrics,
    Logs,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Phase {
    pub start_ns: u64,
    pub duration_ns: u64,
    pub rate: Option<u64>,
    pub batch: usize,
    pub first_sequence: u64,
}

impl Phase {
    pub fn scheduled(&self) -> u64 {
        match self.rate {
            Some(rate) => ((u128::from(rate) * u128::from(self.duration_ns)) / 1_000_000_000) as u64,
            None => u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub scheduled: u64,
    pub attempted: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub schedule_missed: u64,
    pub batches: u64,
    pub decoded: u64,
    pub invalid: u64,
    pub duplicate: u64,
    pub out_of_order: u64,
    pub checksum_sum: u64,
    pub checksum_xor: u64,
    pub cpu_us: u64,
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub timestamp_ns: u64,
    pub done: bool,
    pub fatal: bool,
}

impl Snapshot {
    pub fn delta(self, base: Self) -> Self {
        Self {
            scheduled: self.scheduled - base.scheduled,
            attempted: self.attempted - base.attempted,
            accepted: self.accepted - base.accepted,
            rejected: self.rejected - base.rejected,
            schedule_missed: self.schedule_missed - base.schedule_missed,
            batches: self.batches - base.batches,
            decoded: self.decoded - base.decoded,
            invalid: self.invalid - base.invalid,
            duplicate: self.duplicate - base.duplicate,
            out_of_order: self.out_of_order - base.out_of_order,
            checksum_sum: self.checksum_sum.wrapping_sub(base.checksum_sum),
            checksum_xor: self.checksum_xor ^ base.checksum_xor,
            cpu_us: self.cpu_us.saturating_sub(base.cpu_us),
            rss_bytes: self.rss_bytes,
            peak_rss_bytes: self.peak_rss_bytes,
            timestamp_ns: self.timestamp_ns,
            done: self.done,
            fatal: self.fatal,
        }
    }
}

#[derive(Default)]
pub struct SharedStats {
    pub scheduled: AtomicU64,
    pub attempted: AtomicU64,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub schedule_missed: AtomicU64,
    pub batches: AtomicU64,
    pub decoded: AtomicU64,
    pub invalid: AtomicU64,
    pub duplicate: AtomicU64,
    pub out_of_order: AtomicU64,
    pub checksum_sum: AtomicU64,
    pub checksum_xor: AtomicU64,
    pub done: AtomicBool,
    pub fatal: AtomicBool,
}

impl SharedStats {
    pub fn snapshot(&self) -> Snapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let (cpu_us, peak_rss_bytes) = process_usage();
        Snapshot {
            scheduled: load(&self.scheduled),
            attempted: load(&self.attempted),
            accepted: load(&self.accepted),
            rejected: load(&self.rejected),
            schedule_missed: load(&self.schedule_missed),
            batches: load(&self.batches),
            decoded: load(&self.decoded),
            invalid: load(&self.invalid),
            duplicate: load(&self.duplicate),
            out_of_order: load(&self.out_of_order),
            checksum_sum: load(&self.checksum_sum),
            checksum_xor: load(&self.checksum_xor),
            cpu_us,
            rss_bytes: process_memory::Querier::default().resident_set_size().unwrap_or(0) as u64,
            peak_rss_bytes,
            timestamp_ns: clock_ns(),
            done: self.done.load(Ordering::Relaxed),
            fatal: self.fatal.load(Ordering::Relaxed),
        }
    }

    pub fn accepted_range(&self, first: u64, count: u64) {
        self.accepted.fetch_add(count, Ordering::Relaxed);
        let sum = (0..count).fold(0u64, |sum, offset| sum.wrapping_add(first + offset));
        let xor = (0..count).fold(0u64, |xor, offset| xor ^ (first + offset));
        self.checksum_sum.fetch_add(sum, Ordering::Relaxed);
        self.checksum_xor.fetch_xor(xor, Ordering::Relaxed);
    }

    pub fn received(&self, sequence: u64) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        self.checksum_sum.fetch_add(sequence, Ordering::Relaxed);
        self.checksum_xor.fetch_xor(sequence, Ordering::Relaxed);
    }
}

pub fn clock_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid writable timespec and the monotonic clock exists on macOS/Linux.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(status, 0, "monotonic clock failed");
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn sleep_until(target_ns: u64) {
    let now = clock_ns();
    if target_ns > now {
        std::thread::sleep(Duration::from_nanos(target_ns - now));
    }
}

pub fn deadline_missed(now_ns: u64, deadline_ns: u64) -> bool {
    now_ns > deadline_ns.saturating_add(CATCH_UP_NS)
}

fn process_usage() -> (u64, u64) {
    // SAFETY: `usage` is writable and `RUSAGE_SELF` is valid on macOS/Linux.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return (0, 0);
    }
    let micros = |time: libc::timeval| (time.tv_sec as u64) * 1_000_000 + time.tv_usec as u64;
    #[cfg(target_os = "macos")]
    let peak_rss_bytes = usage.ru_maxrss as u64;
    #[cfg(not(target_os = "macos"))]
    let peak_rss_bytes = (usage.ru_maxrss as u64) * 1024;
    (micros(usage.ru_utime) + micros(usage.ru_stime), peak_rss_bytes)
}

pub fn fixture(workload: Workload, sequence: u64) -> Message {
    match workload {
        Workload::Metrics => Message::Metric(Metric {
            metric_type: 3,
            name: METRIC_NAME.to_owned(),
            value: sequence as f64,
            timestamp: TIMESTAMP,
            tags: TAGS.iter().map(|tag| (*tag).to_owned()).collect(),
            hostname: HOSTNAME.to_owned(),
            interval_secs: 0,
        }),
        Workload::Logs => Message::Log(Log {
            message: format!("{sequence:016x}{}", "x".repeat(240)),
            level: 20,
        }),
    }
}

pub fn validate(workload: Workload, message: &Message) -> Option<u64> {
    match (workload, message) {
        (Workload::Metrics, Message::Metric(metric))
            if metric.metric_type == 3
                && metric.name == METRIC_NAME
                && metric.timestamp == TIMESTAMP
                && metric.hostname == HOSTNAME
                && metric.interval_secs == 0
                && metric.tags.iter().map(String::as_str).eq(TAGS) =>
        {
            let seq = metric.value as u64;
            (seq < MAX_SEQUENCE && metric.value == seq as f64).then_some(seq)
        }
        (Workload::Logs, Message::Log(log)) if log.level == 20 && log.message.len() == 256 => {
            let (prefix, padding) = log.message.split_at(16);
            (padding.bytes().all(|byte| byte == b'x'))
                .then(|| u64::from_str_radix(prefix, 16).ok())
                .flatten()
        }
        _ => None,
    }
}

pub fn into_proto(message: Message) -> proto::CheckData {
    let data = match message {
        Message::Metric(metric) => check_data::Data::Metric(proto::metric::Metric {
            r#type: metric.metric_type,
            name: metric.name,
            value: metric.value,
            timestamp: metric.timestamp,
            tags: metric.tags,
            hostname: metric.hostname,
            interval_secs: metric.interval_secs,
        }),
        Message::Log(log) => check_data::Data::Log(proto::log::Log {
            message: log.message,
            level: log.level,
        }),
        _ => unreachable!("v1 benchmark only generates metrics and logs"),
    };
    proto::CheckData { data: Some(data) }
}

pub fn from_proto(message: proto::CheckData) -> Option<Message> {
    match message.data? {
        check_data::Data::Metric(metric) => Some(Message::Metric(Metric {
            metric_type: metric.r#type,
            name: metric.name,
            value: metric.value,
            timestamp: metric.timestamp,
            tags: metric.tags,
            hostname: metric.hostname,
            interval_secs: metric.interval_secs,
        })),
        check_data::Data::Log(log) => Some(Message::Log(Log {
            message: log.message,
            level: log.level,
        })),
        _ => None,
    }
}

pub struct Validator {
    previous: Option<u64>,
    workload: Workload,
}

impl Validator {
    pub fn new(workload: Workload) -> Self {
        Self {
            previous: None,
            workload,
        }
    }

    pub fn record(&mut self, message: &Message, stats: &SharedStats) {
        let Some(sequence) = validate(self.workload, message) else {
            stats.invalid.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if let Some(previous) = self.previous {
            if previous == sequence {
                stats.duplicate.fetch_add(1, Ordering::Relaxed);
            } else if previous > sequence {
                stats.out_of_order.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.previous = Some(sequence);
        stats.received(sequence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_keep_size_and_identity() {
        use prost::Message as _;
        for workload in [Workload::Metrics, Workload::Logs] {
            for sequence in [0, 1, 123_456, MAX_SEQUENCE - 1] {
                let message = fixture(workload, sequence);
                assert_eq!(validate(workload, &message), Some(sequence));
                assert_eq!(
                    validate(workload, &from_proto(into_proto(message.clone())).unwrap()),
                    Some(sequence)
                );
                if let Message::Log(log) = message {
                    assert_eq!(log.message.len(), 256);
                }
            }
            let example = fixture(workload, 1);
            let fit_size = example.encode_payload().unwrap().1.len();
            let grpc_size = proto::SendCheckPayloadRequest {
                data: vec![into_proto(example)],
            }
            .encoded_len();
            let expected = match workload {
                Workload::Metrics => (153, 128),
                Workload::Logs => (264, 267),
            };
            assert_eq!((fit_size, grpc_size), expected);
        }
    }

    #[test]
    fn partial_final_batch_and_snapshot_delta() {
        let phase = Phase {
            start_ns: 0,
            duration_ns: 1_000_000_000,
            rate: Some(130),
            batch: 64,
            first_sequence: 0,
        };
        assert_eq!(phase.scheduled(), 130);
        assert_eq!(phase.scheduled().div_ceil(64), 3);
        let stats = SharedStats::default();
        stats.accepted_range(100, 3);
        let snapshot = stats.snapshot().delta(Snapshot::default());
        assert_eq!(snapshot.accepted, 3);
        assert_eq!(snapshot.checksum_sum, 303);
        assert_eq!(snapshot.checksum_xor, 100 ^ 101 ^ 102);
    }

    #[test]
    fn pacing_discards_only_batches_more_than_ten_ms_late() {
        let deadline = 1_000_000_000;
        assert!(!deadline_missed(deadline + CATCH_UP_NS, deadline));
        assert!(deadline_missed(deadline + CATCH_UP_NS + 1, deadline));
    }
}

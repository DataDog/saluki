//! Both topologies fed through the real DogStatsD source, so that its listeners, the datagram queue and the receiver
//! mutex its decoders share, the decoders, the context resolvers, and the admission of `dsd_in.metrics` are in the
//! path, which the scripted feed of the parent module bypasses (plan section 8.4, P9.0).
//!
//! Generator threads, outside the runtime of the topology, send pre-encoded DogStatsD payloads to the source's sockets:
//! UDP datagrams on loopback (up to 8 KiB), Unix stream connections (length-prefixed frames, as the source's stream
//! framer reads them), or Unix datagram sockets. Unix datagram sockets are used on Linux only: on macOS they cap a
//! datagram at 2 KiB with a 4 KiB receive buffer (`net.local.dgram.maxdgram`, `net.local.dgram.recvspace`), which would
//! measure the kernel's socket buffer, so `uds` means stream connections there. The topology runs on a runtime of `W`
//! workers with the source's decoder count set to `max(2, W - 2)` explicitly, since the native default reads the host's
//! vCPUs rather than the workers, and with the real encoders and a global pool of eight threads (see the parent
//! module).
//!
//! Each measured phase runs for a window after a warm-up, with the generators flooding (saturated) or paced:
//!
//! - **Throughput** is the metrics the source dispatched on `dsd_in.metrics` per second over the window, which the
//!   `dispatch_items` probe counts (`saluki_core::probes`). At saturation every queue downstream is full at both ends
//!   of the window, so this is the rate of the slowest stage. With datagrams, what the source could not take was
//!   dropped by the kernel, which the drop share reports (sent less dispatched).
//! - **CPU** is the process's CPU time per dispatched metric, less the generator threads' own and the test thread's.
//! - **Latency** (paced phases) is the time from a generator sending a sentinel, a timestamped counter that takes the
//!   passthrough to the main output, to the main encoder receiving it; sentinels are sent every
//!   [`SENTINEL_EVERY`] metrics of a generator.
//! - **Memory** is what the process holds at the end of the window, less what it held before the topology started (the
//!   generators' payloads), from the test binary's tracking allocator.
//!
//! A datagram generator waits for room in the socket by default, as lading's generators do, so that a saturated run
//! measures what the source takes rather than how fast the generators can drop; with `drop_when_full` (M8's default) it
//! sends without waiting, as DogStatsD clients do, and counts what the socket refused. Each paced phase paces afresh
//! from its start, whatever the phase before it was.
//!
//! The probes' durations (lock waits, holds, send waits, clones) are recorded only by the measurements that turn timing
//! on, M2, M7, and M8, which report them; the scaling curves leave timing off.
//!
//! The measurements here are ignored: run them in release mode on an otherwise idle machine, one at a time
//! (`--test-threads 1`), as the parent module says, for example
//! `SALUKI_MEASURE_WORKERS=1,2,4 cargo nextest run -p agent-data-plane --release --features measurement --run-ignored
//! only -E 'test(/source_feed::scaling_curves/)' --no-capture --test-threads 1`. Each reads its parameters from the
//! environment (`SALUKI_MEASURE_*`, see [`Knobs`]), with defaults of its own. The in-process curves on a laptop share
//! its cores between the generators and the topology, and the performance and efficiency cores of macOS differ, so they
//! show the shape of the curves; the gate's homogeneous scaling is measured on Linux (`test/scaling/`). The tests that
//! are not ignored check the harness itself in a few seconds each, and the pacing of the generators.

use std::{
    collections::BTreeMap,
    io::Write as _,
    net::{SocketAddr, UdpSocket},
    os::unix::net::{UnixDatagram, UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceStatsSnapshot};
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::probes::{self, HistogramSnapshot};
use serde_json::json;
use tokio::{runtime::Runtime, sync::mpsc};

use super::{allocations, busy, global_pool, process_cpu, thread_cpu, CONFIG, GATEWAYS, SENTINEL};
use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        test_support::{connected_configuration_system, update},
        test_topology::{Aggregation, Encoders, MetricsPath, Options, TestTopology},
    },
};

/// The largest payload a generator sends, in bytes: the source's default receive buffer.
const PAYLOAD_BYTES: usize = 8_192;

/// The metrics of a generator between two of its sentinels, in paced phases.
const SENTINEL_EVERY: u64 = 4_096;

/// The probe that counts the metrics the source dispatches.
const DECODED: &str = "dispatch_items/dsd_in.metrics";

/// How generators reach the source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transport {
    /// UDP datagrams on loopback, through the source's datagram decoders.
    Udp,

    /// Unix stream connections, each decoded by a task of its own.
    UnixStream,

    /// Unix datagram sockets, through the source's datagram decoders (Linux only).
    UnixDatagram,
}

impl Transport {
    /// Returns the transport named `name`: `udp`, `uds` (Unix datagrams on Linux, Unix streams elsewhere),
    /// `uds-stream`, or `uds-dgram`.
    fn named(name: &str) -> Self {
        match name {
            "udp" => Self::Udp,
            "uds" if cfg!(target_os = "linux") => Self::UnixDatagram,
            "uds" | "uds-stream" => Self::UnixStream,
            "uds-dgram" => Self::UnixDatagram,
            other => panic!("unknown transport `{}`: use udp, uds, uds-stream, or uds-dgram", other),
        }
    }

    /// Returns the datagram transport of this platform: Unix datagrams on Linux, UDP elsewhere (see the module
    /// documentation).
    fn datagram() -> Self {
        if cfg!(target_os = "linux") {
            Self::UnixDatagram
        } else {
            Self::Udp
        }
    }

    fn is_datagram(self) -> bool {
        !matches!(self, Self::UnixStream)
    }

    /// Returns `body`, newline-separated lines, as one payload of this transport: stream frames carry their length
    /// first, as a little-endian `u32`.
    fn payload(self, body: &[u8]) -> Vec<u8> {
        match self {
            Self::UnixStream => {
                let mut payload = (body.len() as u32).to_le_bytes().to_vec();
                payload.extend_from_slice(body);
                payload
            }
            Self::Udp | Self::UnixDatagram => body.to_vec(),
        }
    }
}

/// The configuration both paths run, beside the keys that make the source listen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Profile {
    /// The parent module's [`CONFIG`]: a mapper profile, a metric filterlist, and a tag filterlist.
    Mapper,

    /// Nothing beyond the listener: every default.
    Defaults,

    /// The parent module's [`GATEWAYS`]: [`CONFIG`] with two endpoint routing groups and Multi-Region Failover.
    Gateways,
}

impl Profile {
    /// Returns the profile named `name`: `mapper`, `defaults`, or `gateways`.
    fn named(name: &str) -> Self {
        match name {
            "mapper" => Self::Mapper,
            "defaults" => Self::Defaults,
            "gateways" => Self::Gateways,
            other => panic!("unknown configuration `{}`: use mapper, defaults, or gateways", other),
        }
    }

    /// Returns the profile's configuration, as YAML.
    fn config(self) -> &'static str {
        match self {
            Self::Mapper => CONFIG,
            Self::Defaults => "",
            Self::Gateways => GATEWAYS,
        }
    }
}

/// What the generators' lines carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Workload {
    /// The parent module's batch as DogStatsD text, eight kinds of line (see [`line`]): one line in eight carries a
    /// context of its own for each context index.
    Mixed,

    /// A context of its own on seven lines in eight: counters, gauges, and distributions, with a timestamped counter
    /// on the eighth (see [`distinct_line`]), so that aggregation holds every context the generators sent since its
    /// last flush.
    Distinct,
}

impl Workload {
    /// Returns the workload named `name`: `mixed` or `distinct`.
    fn named(name: &str) -> Self {
        match name {
            "mixed" => Self::Mixed,
            "distinct" => Self::Distinct,
            other => panic!("unknown workload `{}`: use mixed or distinct", other),
        }
    }

    /// Returns the line of metric `index` over `contexts` contexts.
    fn line(self, index: usize, contexts: usize) -> String {
        match self {
            Self::Mixed => line(index, contexts),
            Self::Distinct => distinct_line(index, contexts),
        }
    }

    /// Returns the lines of one rotation of the generators: every context at least once.
    fn lines(self, contexts: usize) -> usize {
        let per_rotation = match self {
            Self::Mixed => 8 * contexts,
            Self::Distinct => contexts.div_ceil(7) * 8,
        };
        per_rotation.max(8 * 1_024)
    }
}

/// The parameters of a measurement, from the environment, with the defaults each test gives.
#[derive(Clone, Debug)]
struct Knobs {
    /// Primary runtime workers to measure at (`SALUKI_MEASURE_WORKERS`, comma-separated).
    workers: Vec<usize>,

    /// The transport (`SALUKI_MEASURE_TRANSPORT`).
    transport: Transport,

    /// Whether a datagram generator drops a datagram the socket has no room for, as DogStatsD clients do, rather than
    /// wait for room, as lading does (`SALUKI_MEASURE_DROP_WHEN_FULL`, `1` or `0`).
    drop_when_full: bool,

    /// The configuration (`SALUKI_MEASURE_CONFIG`: `mapper`, `defaults`, or `gateways`).
    profile: Profile,

    /// What the lines carry (`SALUKI_MEASURE_WORKLOAD`: `mixed` or `distinct`).
    workload: Workload,

    /// Rounds per path, alternating (`SALUKI_MEASURE_ROUNDS`).
    rounds: usize,

    /// The measured window of a phase, after its warm-up (`SALUKI_MEASURE_WINDOW_SECS`).
    window: Duration,

    /// Metrics per payload (`SALUKI_MEASURE_PER_DATAGRAM`), within [`PAYLOAD_BYTES`].
    per_datagram: usize,

    /// The context indexes the generators rotate through (`SALUKI_MEASURE_CONTEXTS`): about the contexts aggregation
    /// holds, with either workload (see [`line`] and [`distinct_line`]).
    contexts: usize,

    /// Generator threads (`SALUKI_MEASURE_GENERATORS`); by default half the decoders, at least two.
    generators: Option<usize>,

    /// Connections or sockets per generator (`SALUKI_MEASURE_CONNECTIONS_PER_GENERATOR`).
    connections_per_generator: usize,

    /// The source's decoder count (`SALUKI_MEASURE_DECODERS`); by default `max(2, W - 2)`.
    decoders: Option<usize>,

    /// A paced rate in metrics per second (`SALUKI_MEASURE_RATE`), instead of a share of the saturated rate.
    rate: Option<f64>,
}

impl Knobs {
    /// Reads the knobs, with `defaults` for those the environment does not set.
    fn from_env(defaults: Knobs) -> Self {
        let read = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let number = |name: &str| {
            read(name).map(|value| {
                value
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("{} is a number, not `{}`", name, value))
            })
        };
        Self {
            workers: read("SALUKI_MEASURE_WORKERS")
                .map(|list| {
                    list.split(',')
                        .map(|workers| workers.trim().parse().expect("SALUKI_MEASURE_WORKERS lists numbers"))
                        .collect()
                })
                .unwrap_or(defaults.workers),
            transport: read("SALUKI_MEASURE_TRANSPORT")
                .map(|name| Transport::named(&name))
                .unwrap_or(defaults.transport),
            drop_when_full: number("SALUKI_MEASURE_DROP_WHEN_FULL")
                .map(|flag| flag != 0)
                .unwrap_or(defaults.drop_when_full),
            profile: read("SALUKI_MEASURE_CONFIG")
                .map(|name| Profile::named(&name))
                .unwrap_or(defaults.profile),
            workload: read("SALUKI_MEASURE_WORKLOAD")
                .map(|name| Workload::named(&name))
                .unwrap_or(defaults.workload),
            rounds: number("SALUKI_MEASURE_ROUNDS").unwrap_or(defaults.rounds),
            window: number("SALUKI_MEASURE_WINDOW_SECS")
                .map(|secs| Duration::from_secs(secs as u64))
                .unwrap_or(defaults.window),
            per_datagram: number("SALUKI_MEASURE_PER_DATAGRAM").unwrap_or(defaults.per_datagram),
            contexts: number("SALUKI_MEASURE_CONTEXTS").unwrap_or(defaults.contexts),
            generators: number("SALUKI_MEASURE_GENERATORS").or(defaults.generators),
            connections_per_generator: number("SALUKI_MEASURE_CONNECTIONS_PER_GENERATOR")
                .unwrap_or(defaults.connections_per_generator),
            decoders: number("SALUKI_MEASURE_DECODERS").or(defaults.decoders),
            rate: read("SALUKI_MEASURE_RATE")
                .map(|rate| rate.parse().expect("SALUKI_MEASURE_RATE is a number"))
                .or(defaults.rate),
        }
    }

    /// The defaults of the scaling curves.
    fn curves() -> Self {
        Self {
            workers: vec![1, 2, 4, 8, 16],
            transport: Transport::named("uds"),
            drop_when_full: false,
            profile: Profile::Mapper,
            workload: Workload::Mixed,
            rounds: 5,
            window: Duration::from_secs(5),
            per_datagram: 50,
            contexts: 256,
            generators: None,
            connections_per_generator: 1,
            decoders: None,
            rate: None,
        }
    }

    /// Returns the decoder count at `workers` workers.
    fn decoders_at(&self, workers: usize) -> usize {
        self.decoders.unwrap_or_else(|| workers.saturating_sub(2).max(2))
    }

    /// Returns the generator threads at `workers` workers.
    fn generators_at(&self, workers: usize) -> usize {
        self.generators
            .unwrap_or_else(|| (self.decoders_at(workers) / 2).max(2))
    }
}

/// Returns the DogStatsD line of metric `index` of the workload over `contexts` contexts: the parent module's batch
/// (mapped counters, a blocked name, distributions whose `user_id` tag the tag filterlist removes, gauges, and
/// timestamped counters) as DogStatsD text. Every eight lines carry one metric of each kind, and the `n`th eight lines
/// use context `n mod contexts` of the kinds with many: the gauges `app.queue<n>` and the distributions' `user_id`. So
/// the source resolves about twice `contexts` contexts, and aggregation, after the tag filterlist collapses the
/// distributions, holds about `contexts` gauges and a few dozen others.
fn line(index: usize, contexts: usize) -> String {
    let context = (index / 8) % contexts;
    match index % 8 {
        0 | 1 => format!("raw.requests.route{}:1|c|#env:prod", context % 16),
        2 => "raw.blocked:1|c".to_string(),
        3 | 4 => format!("checkout.latency:10|d|#user_id:{},env:dev", context),
        5 => format!("system.disk{}:9|g", context % 8),
        6 => format!("app.queue{}:7|g|#team:core", context),
        _ => format!("app.jobs{}:2|c|T1700000000", context % 32),
    }
}

/// Returns the DogStatsD line of metric `index` of the distinct workload over `contexts` contexts. Of every eight
/// lines, seven carry a context of their own, the next of `contexts` in turn, as a counter for half of the contexts and
/// a gauge or a distribution for a quarter each, and the eighth is a timestamped counter, so that the passthrough keeps
/// flowing as with the mixed workload: the legacy passthrough batcher sends on what it holds only once its buffer is
/// full, or after a second without timestamped metrics, which the sentinels alone would never give it. Aggregation
/// holds every context sent since its last flush: a rate that sends each context at least once between two flushes
/// makes a flush of every context.
fn distinct_line(index: usize, contexts: usize) -> String {
    let (group, position) = (index / 8, index % 8);
    if position == 7 {
        return format!("distinct.jobs{}:2|c|T1700000000", group % 32);
    }
    let context = (group * 7 + position) % contexts;
    match context % 4 {
        0 | 1 => format!("distinct.count{}:1|c|#team:core", context),
        2 => format!("distinct.gauge{}:7|g|#team:core", context),
        _ => format!("distinct.latency{}:10|d|#team:core", context),
    }
}

/// Returns the sentinel line with identifier `id`: a timestamped counter, which takes the passthrough to the main
/// output.
fn sentinel_line(id: u64) -> String {
    format!("{}:1|c|#batch:{}|T1700000000", SENTINEL, id)
}

/// Returns the identifier a sentinel's `batch` tag carries.
fn sentinel_id(metric: &saluki_core::data_model::event::metric::Metric) -> Option<u64> {
    metric
        .context()
        .tags()
        .into_iter()
        .find_map(|tag| tag.as_str().strip_prefix("batch:").and_then(|id| id.parse().ok()))
}

/// A pre-encoded payload, and the metrics it carries.
struct Payload {
    bytes: Vec<u8>,
    metrics: u64,
}

/// Returns the payloads of `workload`: every context at least once, `per_datagram` lines to a payload at most, within
/// [`PAYLOAD_BYTES`].
fn payloads(transport: Transport, workload: Workload, contexts: usize, per_datagram: usize) -> Vec<Payload> {
    let limit = PAYLOAD_BYTES - if transport.is_datagram() { 0 } else { 4 };
    let mut payloads = Vec::new();
    let (mut body, mut held) = (Vec::new(), 0u64);
    for index in 0..workload.lines(contexts) {
        let line = workload.line(index, contexts);
        if held > 0 && (held as usize == per_datagram || body.len() + 1 + line.len() > limit) {
            payloads.push(Payload {
                bytes: transport.payload(&body),
                metrics: held,
            });
            body.clear();
            held = 0;
        }
        if !body.is_empty() {
            body.push(b'\n');
        }
        body.extend_from_slice(line.as_bytes());
        held += 1;
    }
    payloads.push(Payload {
        bytes: transport.payload(&body),
        metrics: held,
    });
    payloads
}

/// Where the source listens, and the configuration that makes it listen there.
struct Listener {
    transport: Transport,
    port: u16,
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl Listener {
    fn new(transport: Transport) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory is created");
        let path = directory.path().join("dsd.sock");
        let port = match transport {
            Transport::Udp => {
                // A free port: the source binds it again once this socket is closed.
                let socket = UdpSocket::bind("127.0.0.1:0").expect("a loopback UDP socket binds");
                socket.local_addr().expect("a bound socket has an address").port()
            }
            Transport::UnixStream | Transport::UnixDatagram => 0,
        };
        Self {
            transport,
            port,
            path,
            _directory: directory,
        }
    }

    /// Returns the configuration keys that make the source listen here only, with `decoders` datagram decoders.
    fn config(&self, decoders: usize) -> String {
        let socket = match self.transport {
            Transport::Udp => String::new(),
            Transport::UnixStream => format!("dogstatsd_stream_socket: {}\n", self.path.display()),
            Transport::UnixDatagram => format!("dogstatsd_socket: {}\n", self.path.display()),
        };
        format!(
            "dogstatsd_port: {}\ndogstatsd_workers_count: {}\n{}",
            self.port, decoders, socket
        )
    }

    /// Waits until the source's Unix socket accepts a connection. A UDP port needs no wait: what is sent before the
    /// source binds it is lost, and the harness waits for a sentinel to arrive before it measures anything.
    fn wait_until_bound(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let bound = match self.transport {
                Transport::Udp => true,
                Transport::UnixStream => UnixStream::connect(&self.path).is_ok(),
                Transport::UnixDatagram => UnixDatagram::unbound()
                    .and_then(|socket| socket.connect(&self.path))
                    .is_ok(),
            };
            if bound {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the DogStatsD source binds its socket in time"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Returns a connected sender to the source. With `drop_when_full`, a datagram socket does not wait for room: a
    /// send that would wait fails, and the datagram is dropped.
    fn sender(&self, drop_when_full: bool) -> Sender {
        match self.transport {
            Transport::Udp => {
                let socket = UdpSocket::bind("127.0.0.1:0").expect("a loopback UDP socket binds");
                socket
                    .connect(SocketAddr::from(([127, 0, 0, 1], self.port)))
                    .expect("a UDP socket connects");
                socket
                    .set_nonblocking(drop_when_full)
                    .expect("a UDP socket's blocking mode is set");
                Sender::Udp(socket)
            }
            Transport::UnixStream => {
                // Many connections at once can fill the listener's backlog, which refuses connections until the source
                // accepts the earlier ones.
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    match UnixStream::connect(&self.path) {
                        Ok(stream) => break Sender::Stream(stream),
                        Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                        Err(error) => panic!("the source accepts a connection in time: {}", error),
                    }
                }
            }
            Transport::UnixDatagram => {
                let socket = UnixDatagram::unbound().expect("a Unix datagram socket is created");
                socket.connect(&self.path).expect("a Unix datagram socket connects");
                socket
                    .set_nonblocking(drop_when_full)
                    .expect("a Unix datagram socket's blocking mode is set");
                Sender::Datagram(socket)
            }
        }
    }
}

/// A generator's socket.
enum Sender {
    Udp(UdpSocket),
    Stream(UnixStream),
    Datagram(UnixDatagram),
}

impl Sender {
    /// Sends `payload`, and returns whether the socket took it. A datagram the kernel refuses, for want of buffer space
    /// or, on a socket that does not wait, of room in the source's queue, is dropped, as a client's would be; a stream
    /// connection waits for room.
    fn send(&mut self, payload: &[u8]) -> bool {
        match self {
            Self::Udp(socket) => socket.send(payload).is_ok(),
            Self::Datagram(socket) => socket.send(payload).is_ok(),
            Self::Stream(stream) => {
                stream.write_all(payload).expect("the source reads its connections");
                true
            }
        }
    }
}

/// How many bits of the [`Pace`] word hold the rate.
const RATE_BITS: u32 = 48;

/// The rate that floods: the generators send as fast as their sockets take payloads.
const FLOOD: u64 = (1 << RATE_BITS) - 1;

/// The rate the harness sets for the generators, in metrics per second (0 pauses them, [`FLOOD`] floods), with the
/// epoch of the setting, in one word, so that a generator reads both at once: every setting starts a new epoch, even at
/// the rate of the one before, and a generator paces each epoch from when it first sees it (see [`Pacer`]).
struct Pace(AtomicU64);

impl Pace {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Sets the rate to `rate` metrics per second (at most [`FLOOD`]), in a new epoch. Only the harness's thread sets
    /// it.
    fn set(&self, rate: u64) {
        let epoch = (self.0.load(Ordering::Relaxed) >> RATE_BITS).wrapping_add(1);
        self.0.store((epoch << RATE_BITS) | rate.min(FLOOD), Ordering::Relaxed);
    }

    /// Returns the epoch of the current setting and its rate.
    fn get(&self) -> (u64, u64) {
        let word = self.0.load(Ordering::Relaxed);
        (word >> RATE_BITS, word & FLOOD)
    }
}

/// What a generator does next.
#[derive(Debug, Eq, PartialEq)]
enum Step {
    /// Sends its next payload.
    Send,

    /// Waits this long, being ahead of its pace.
    Wait(Duration),

    /// Waits for the harness to set a rate.
    Pause,
}

/// Paces one generator at its share of the rate the harness set, counted from when the generator first saw the setting,
/// so that a phase starts afresh, even at the rate of the phase before it, rather than catching up on the pause between
/// them.
#[derive(Debug, Default)]
struct Pacer {
    /// The epoch of the setting being paced, and when this generator first saw it.
    setting: Option<(u64, Instant)>,

    /// The metrics sent since then.
    sent: u64,
}

impl Pacer {
    /// Returns what the generator does at `now`, under the setting `(epoch, rate)` of [`Pace::get`], when `generators`
    /// share the rate.
    fn step(&mut self, (epoch, rate): (u64, u64), generators: usize, now: Instant) -> Step {
        let since = match self.setting {
            Some((paced, since)) if paced == epoch => since,
            _ => {
                self.setting = Some((epoch, now));
                self.sent = 0;
                now
            }
        };
        match rate {
            0 => Step::Pause,
            FLOOD => Step::Send,
            rate => {
                let due = since + Duration::from_secs_f64(self.sent as f64 * generators as f64 / rate as f64);
                match due.checked_duration_since(now) {
                    Some(wait) if !wait.is_zero() => Step::Wait(wait),
                    _ => Step::Send,
                }
            }
        }
    }

    /// Records that the generator sent `metrics` metrics.
    fn sent(&mut self, metrics: u64) {
        self.sent += metrics;
    }
}

/// What the generators share with the harness.
struct Generators {
    /// The total rate, which the generators share.
    pace: Pace,

    /// Whether the generators send sentinels.
    sentinels: AtomicBool,

    stop: AtomicBool,

    /// Metrics sent so far, sentinels excluded, whether or not the socket took them.
    sent: AtomicU64,

    /// Metrics of the payloads the sockets refused so far (datagrams only).
    refused: AtomicU64,

    /// The generator threads' CPU time so far, in nanoseconds, published as they run.
    cpu_nanos: AtomicU64,

    /// When each generator sent each of its sentinels, by sequence number.
    sentinel_times: Vec<Mutex<Vec<Instant>>>,

    count: usize,
}

impl Generators {
    /// Starts `count` generator threads, each with `connections` senders (see [`Listener::sender`] for
    /// `drop_when_full`), rotating through `payloads` from offsets of their own.
    fn start(
        listener: &Listener, payloads: Arc<Vec<Payload>>, count: usize, connections: usize, drop_when_full: bool,
    ) -> (Arc<Self>, Vec<JoinHandle<()>>) {
        let shared = Arc::new(Self {
            pace: Pace::new(),
            sentinels: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            cpu_nanos: AtomicU64::new(0),
            sentinel_times: (0..count).map(|_| Mutex::new(Vec::new())).collect(),
            count,
        });
        let threads = (0..count)
            .map(|generator| {
                let senders = (0..connections)
                    .map(|_| listener.sender(drop_when_full))
                    .collect::<Vec<_>>();
                let (shared, payloads, transport) = (Arc::clone(&shared), Arc::clone(&payloads), listener.transport);
                thread::Builder::new()
                    .name(format!("dsd-generator-{}", generator))
                    .spawn(move || shared.generate(generator, senders, &payloads, transport))
                    .expect("a generator thread starts")
            })
            .collect();
        (shared, threads)
    }

    fn generate(&self, generator: usize, mut senders: Vec<Sender>, payloads: &[Payload], transport: Transport) {
        let mut next = generator * payloads.len() / self.count;
        let mut connection = 0;
        let mut pacer = Pacer::default();
        let (mut since_sentinel, mut sends) = (0u64, 0u64);
        let mut cpu_published = Duration::ZERO;
        let publish = |cpu_published: &mut Duration| {
            let cpu = thread_cpu();
            self.cpu_nanos
                .fetch_add((cpu - *cpu_published).as_nanos() as u64, Ordering::Relaxed);
            *cpu_published = cpu;
        };
        while !self.stop.load(Ordering::Relaxed) {
            // Each generator sends its share of the rate; ahead of it, it waits.
            match pacer.step(self.pace.get(), self.count, Instant::now()) {
                Step::Send => {}
                Step::Wait(wait) => {
                    if wait > Duration::from_millis(2) {
                        thread::sleep(wait - Duration::from_millis(1));
                    } else {
                        thread::yield_now();
                    }
                    continue;
                }
                Step::Pause => {
                    publish(&mut cpu_published);
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
            if self.sentinels.load(Ordering::Relaxed) && since_sentinel >= SENTINEL_EVERY {
                let mut times = self.sentinel_times[generator].lock().unwrap();
                let id = ((generator as u64) << 32) | times.len() as u64;
                let sentinel = transport.payload(sentinel_line(id).as_bytes());
                times.push(Instant::now());
                drop(times);
                senders[connection].send(&sentinel);
                since_sentinel = 0;
            }
            let payload = &payloads[next];
            if !senders[connection].send(&payload.bytes) {
                self.refused.fetch_add(payload.metrics, Ordering::Relaxed);
            }
            next = (next + 1) % payloads.len();
            connection = (connection + 1) % senders.len();
            since_sentinel += payload.metrics;
            pacer.sent(payload.metrics);
            self.sent.fetch_add(payload.metrics, Ordering::Relaxed);
            sends += 1;
            if sends % 64 == 0 {
                publish(&mut cpu_published);
            }
        }
        publish(&mut cpu_published);
    }

    /// Returns when the sentinel `id` was sent.
    fn sent_at(&self, id: u64) -> Option<Instant> {
        let times = self.sentinel_times.get((id >> 32) as usize)?.lock().unwrap();
        times.get((id & u64::from(u32::MAX)) as usize).copied()
    }

    /// Returns the sentinels sent so far.
    fn sentinels_sent(&self) -> usize {
        self.sentinel_times
            .iter()
            .map(|times| times.lock().unwrap().len())
            .sum()
    }
}

/// The counters a phase reads at both ends of its window.
struct Snapshot {
    at: Instant,
    decoded: u64,
    encoded: u64,
    sent: u64,
    refused: u64,
    process_cpu: Duration,
    generator_cpu: u64,
    test_cpu: Duration,
    busy: Duration,
    allocated: u64,
    live: u64,
    probes: BTreeMap<String, HistogramSnapshot>,
    groups_cpu: BTreeMap<String, u64>,
}

/// What a phase measured.
#[derive(Clone, Debug, Default)]
struct Measured {
    /// Metrics the source dispatched per second.
    throughput: f64,

    /// Metrics per second that the encoders' payloads carried to the forwarders, after aggregation.
    encoded: f64,

    /// The process's CPU time per dispatched metric, less the generators' and the test thread's, in nanoseconds.
    cpu_per_metric: f64,

    /// The primary runtime's worker busy time per dispatched metric, in nanoseconds.
    busy_per_metric: f64,

    /// Bytes allocated per dispatched metric.
    bytes_per_metric: f64,

    /// The share of the metrics sent that the source did not dispatch, in the window.
    dropped: f64,

    /// The share of the metrics sent that the sockets refused, in the window: part of `dropped`.
    refused: f64,

    /// What the process holds at the end of the window, less what it held before the topology started, in MiB.
    live_mib: f64,

    /// Sentinel latency percentiles and the worst, in microseconds, and how many sentinels arrived and were lost.
    p50: f64,
    p99: f64,
    worst: f64,
    sentinels: usize,
    lost: usize,

    /// The window's length.
    window: Duration,

    /// What each probe recorded in the window.
    probes: BTreeMap<String, HistogramSnapshot>,

    /// The CPU time each resource group used in the window, in nanoseconds (Linux only).
    groups_cpu: BTreeMap<String, u64>,
}

/// One topology fed through the real source, and its generators.
struct Fed {
    runtime: Runtime,
    pool: Runtime,
    topology: Option<TestTopology>,
    stream: mpsc::Sender<ConfigUpdate>,
    generators: Arc<Generators>,
    threads: Vec<JoinHandle<()>>,
    arrivals: BTreeMap<u64, Instant>,

    /// What the process held before the topology started, in bytes: the generators' payloads, mostly.
    baseline_live: u64,
    _listener: Listener,
}

impl Fed {
    /// Starts `path` on a runtime of `workers` workers, configured with `config`, fed by the generators the knobs
    /// describe through the transport they name.
    fn start(path: MetricsPath, workers: usize, config: &str, knobs: &Knobs) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("primary")
            .enable_all()
            .build()
            .expect("the primary runtime builds");
        let pool = global_pool();
        let payloads = Arc::new(payloads(
            knobs.transport,
            knobs.workload,
            knobs.contexts,
            knobs.per_datagram,
        ));
        let baseline_live = allocations().2;
        let listener = Listener::new(knobs.transport);
        let config = format!("{}{}", config, listener.config(knobs.decoders_at(workers)));
        let (topology, stream) = runtime.block_on(async {
            let (system, updates, stream) = connected_configuration_system(&config).await;
            let mut options = Options::new(path, Aggregation::Native);
            options.keep = |metric| metric.context().name() == SENTINEL;
            options.encoders = Encoders::Real;
            options.worker_pool = Some(pool.handle().clone());
            options.real_sources.insert("dsd_in".to_string());
            let topology = TestTopology::start(&system, options, |supervisor| {
                supervisor.add_worker(ConfigUpdatesWorker::new(updates))
            })
            .await;
            (topology, stream)
        });
        listener.wait_until_bound();
        let (generators, threads) = Generators::start(
            &listener,
            payloads,
            knobs.generators_at(workers),
            knobs.connections_per_generator,
            knobs.drop_when_full,
        );
        let mut fed = Self {
            runtime,
            pool,
            topology: Some(topology),
            stream,
            generators,
            threads,
            arrivals: BTreeMap::new(),
            baseline_live,
            _listener: listener,
        };
        fed.wait_until_flowing();
        fed
    }

    /// Records the sentinels that arrive at the main encoder until `deadline`.
    fn collect_until(&mut self, deadline: Instant) {
        let Self {
            runtime,
            topology,
            arrivals,
            ..
        } = self;
        let topology = topology.as_mut().expect("the topology runs");
        runtime.block_on(async {
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return;
                }
                match tokio::time::timeout(left, topology.next_delivery()).await {
                    Ok(Some(delivery)) if delivery.output == "dd_metrics_encode" => {
                        let at = delivery.at.into_std();
                        for id in delivery.metrics.iter().filter_map(sentinel_id) {
                            arrivals.insert(id, at);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return,
                }
            }
        });
    }

    /// Paces the generators slowly, with sentinels, until one arrives: every component, the installer included, is
    /// then running.
    fn wait_until_flowing(&mut self) {
        self.generators.sentinels.store(true, Ordering::Relaxed);
        self.generators.pace.set(100_000);
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.arrivals.is_empty() {
            assert!(Instant::now() < deadline, "a sentinel reaches the main encoder in time");
            self.collect_until(Instant::now() + Duration::from_millis(100));
        }
        self.pause();
    }

    /// Pauses the generators, and waits until the source has dispatched everything it holds: until its count stays
    /// put for 300 ms, for 10 s at most.
    fn pause(&mut self) {
        self.generators.pace.set(0);
        self.generators.sentinels.store(false, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = decoded();
        while Instant::now() < deadline {
            self.collect_until(Instant::now() + Duration::from_millis(300));
            let now = decoded();
            if now == last {
                return;
            }
            last = now;
        }
    }

    fn snapshot(&self) -> Snapshot {
        let probes = probes::snapshot();
        let (allocated, _, live) = allocations();
        Snapshot {
            at: Instant::now(),
            decoded: probes.get(DECODED).map_or(0, |probe| probe.sum),
            encoded: self.forwarded(),
            sent: self.generators.sent.load(Ordering::Relaxed),
            refused: self.generators.refused.load(Ordering::Relaxed),
            process_cpu: process_cpu(),
            generator_cpu: self.generators.cpu_nanos.load(Ordering::Relaxed),
            test_cpu: thread_cpu(),
            busy: busy(self.runtime.handle()),
            allocated,
            live,
            probes,
            groups_cpu: groups_cpu(),
        }
    }

    /// Returns how many metrics the payloads the encoders sent to the forwarders carried so far.
    fn forwarded(&self) -> u64 {
        self.topology.as_ref().map_or(0, TestTopology::forwarded)
    }

    /// Runs one phase: the generators at `rate` metrics per second (flooding with `None`), with sentinels when
    /// `latency`, for a warm-up of `warm_up` and then the measured `window`.
    fn phase(&mut self, rate: Option<f64>, latency: bool, warm_up: Duration, window: Duration) -> Measured {
        self.arrivals.clear();
        let rate = rate.map_or(FLOOD, |rate| rate.max(1.0) as u64);
        self.generators.sentinels.store(latency, Ordering::Relaxed);
        self.generators.pace.set(rate);
        self.collect_until(Instant::now() + warm_up);
        let first = self.snapshot();
        self.collect_until(first.at + window);
        let last = self.snapshot();
        self.pause();
        if latency {
            self.wait_for_sentinels(first.at, last.at);
        }

        let decoded = last.decoded.saturating_sub(first.decoded).max(1) as f64;
        let elapsed = last.at - first.at;
        let cpu = last.process_cpu.saturating_sub(first.process_cpu).as_nanos() as f64
            - last.generator_cpu.saturating_sub(first.generator_cpu) as f64
            - last.test_cpu.saturating_sub(first.test_cpu).as_nanos() as f64;
        let sent = last.sent.saturating_sub(first.sent).max(1) as f64;
        let mut latencies = self
            .arrivals
            .iter()
            .filter_map(|(id, arrived)| {
                let sent = self.generators.sent_at(*id)?;
                (sent >= first.at && sent <= last.at).then(|| arrived.saturating_duration_since(sent))
            })
            .collect::<Vec<_>>();
        latencies.sort();
        let micros = |at: f64| {
            latencies
                .get(((latencies.len().max(1) - 1) as f64 * at) as usize)
                .map_or(0.0, |latency| latency.as_secs_f64() * 1e6)
        };
        let sent_sentinels = self.sentinels_sent_between(first.at, last.at);
        Measured {
            throughput: decoded / elapsed.as_secs_f64(),
            encoded: last.encoded.saturating_sub(first.encoded) as f64 / elapsed.as_secs_f64(),
            cpu_per_metric: cpu.max(0.0) / decoded,
            busy_per_metric: last.busy.saturating_sub(first.busy).as_nanos() as f64 / decoded,
            bytes_per_metric: last.allocated.saturating_sub(first.allocated) as f64 / decoded,
            dropped: (1.0 - decoded / sent).max(0.0),
            refused: last.refused.saturating_sub(first.refused) as f64 / sent,
            live_mib: last.live.saturating_sub(self.baseline_live) as f64 / (1024.0 * 1024.0),
            p50: micros(0.5),
            p99: micros(0.99),
            worst: micros(1.0),
            sentinels: latencies.len(),
            lost: sent_sentinels.saturating_sub(latencies.len()),
            window: elapsed,
            probes: last
                .probes
                .iter()
                .map(|(name, probe)| {
                    let earlier = first.probes.get(name).cloned().unwrap_or_default();
                    (name.clone(), probe.since(&earlier))
                })
                .filter(|(_, probe)| probe.count > 0)
                .collect(),
            groups_cpu: last
                .groups_cpu
                .iter()
                .map(|(name, cpu)| {
                    (
                        name.clone(),
                        cpu.saturating_sub(first.groups_cpu.get(name).copied().unwrap_or(0)),
                    )
                })
                .filter(|(_, cpu)| *cpu > 0)
                .collect(),
        }
    }

    /// Returns how many sentinels the generators sent from `from` to `to`.
    fn sentinels_sent_between(&self, from: Instant, to: Instant) -> usize {
        self.generators
            .sentinel_times
            .iter()
            .map(|times| {
                times
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|sent| **sent >= from && **sent <= to)
                    .count()
            })
            .sum()
    }

    /// Waits, for 5 s at most, until every sentinel sent from `from` to `to` has arrived: the legacy path's passthrough
    /// batcher holds timestamped metrics until its buffer fills or it has been idle for a second, so the last sentinels
    /// of a paced phase can arrive after the source has dispatched everything.
    fn wait_for_sentinels(&mut self, from: Instant, to: Instant) {
        let expected = self.sentinels_sent_between(from, to);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let arrived = self
                .arrivals
                .keys()
                .filter_map(|id| self.generators.sent_at(*id))
                .filter(|sent| *sent >= from && *sent <= to)
                .count();
            if arrived >= expected {
                return;
            }
            self.collect_until(Instant::now() + Duration::from_millis(100));
        }
    }

    /// Runs a paced phase while the metric filterlist changes every 50 ms.
    fn phase_with_updates(&mut self, rate: f64, warm_up: Duration, window: Duration) -> Measured {
        let stream = self.stream.clone();
        let changes = self.runtime.spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(50));
            for change in 0.. {
                ticks.tick().await;
                let blocked = if change % 2 == 0 {
                    json!(["raw.blocked", "app.queue1"])
                } else {
                    json!(["raw.blocked"])
                };
                if stream.send(update("metric_filterlist", blocked)).await.is_err() {
                    break;
                }
            }
        });
        let measured = self.phase(Some(rate), true, warm_up, window);
        changes.abort();
        measured
    }

    /// Stops the generators and the topology.
    fn stop(mut self) {
        self.generators.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            thread.join().expect("a generator does not panic");
        }
        let topology = self.topology.take().expect("the topology runs");
        self.runtime.block_on(async {
            drop(topology.stop().await);
        });
        drop(self.stream);
        self.pool.shutdown_background();
        self.runtime.shutdown_background();
    }
}

/// Returns the metrics the source has dispatched so far.
fn decoded() -> u64 {
    probes::snapshot().get(DECODED).map_or(0, |probe| probe.sum)
}

/// Returns the CPU time every resource group has used so far, in nanoseconds: the groups of the tracking allocator read
/// their threads' CPU time on Linux, and nothing elsewhere.
fn groups_cpu() -> BTreeMap<String, u64> {
    let empty = ResourceStatsSnapshot::empty();
    let mut groups = BTreeMap::new();
    ResourceGroupRegistry::global().visit_resource_groups(|name, stats| {
        groups.insert(name.to_string(), stats.snapshot_delta(&empty).cpu_time_nanos);
    });
    groups
}

/// The phases of a scaling round.
const PHASES: [&str; 3] = [
    "saturated",
    "paced at half of legacy's saturation",
    "paced, live updates",
];

/// Returns the median of `value` over `rounds`, for phase `phase`.
fn median(rounds: &[Vec<Measured>], phase: usize, value: impl Fn(&Measured) -> f64) -> f64 {
    let mut values = rounds.iter().map(|round| value(&round[phase])).collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Runs one scaling round of `path` at `workers` workers: saturated, then paced at `paced` metrics per second (half
/// the saturated rate when `None`), then paced with live updates.
fn scaling_round(path: MetricsPath, workers: usize, knobs: &Knobs, paced: Option<f64>) -> Vec<Measured> {
    let mut fed = Fed::start(path, workers, knobs.profile.config(), knobs);
    let warm_up = Duration::from_secs(2);
    let saturated = fed.phase(None, false, warm_up, knobs.window);
    let rate = paced.unwrap_or(saturated.throughput / 2.0);
    let paced = fed.phase(Some(rate), true, Duration::from_secs(1), knobs.window);
    let updating = fed.phase_with_updates(rate, Duration::from_secs(1), knobs.window);
    fed.stop();
    vec![saturated, paced, updating]
}

/// The scaling curves of legacy and plan through the real DogStatsD source, at each number of workers: throughput,
/// throughput per worker, process CPU and allocated bytes per metric, memory, drops, and paced latency, as medians over
/// the rounds, with the plan's ratio to legacy and each path's scaling against its own one-worker throughput. The
/// configuration is the knobs' profile: the mapper profile by default, `SALUKI_MEASURE_CONFIG=defaults` or `gateways`
/// for the other configurations of the gate.
#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn scaling_curves_through_the_dogstatsd_source() {
    let knobs = Knobs::from_env(Knobs::curves());
    println!(
        "Scaling curves, {:?}, configuration {:?}, knobs {:?}",
        knobs.transport, knobs.profile, knobs
    );
    let mut first_throughput: BTreeMap<&str, f64> = BTreeMap::new();
    for &workers in &knobs.workers {
        let (mut legacy, mut plan) = (Vec::new(), Vec::new());
        let mut paced = knobs.rate;
        for _ in 0..knobs.rounds {
            let round = scaling_round(MetricsPath::Legacy, workers, &knobs, paced);
            paced.get_or_insert(round[0].throughput / 2.0);
            legacy.push(round);
            plan.push(scaling_round(MetricsPath::Plan, workers, &knobs, paced));
        }
        println!(
            "\nW = {} workers, D = {} decoders, G = {} generators x {} connections:",
            workers,
            knobs.decoders_at(workers),
            knobs.generators_at(workers),
            knobs.connections_per_generator
        );
        println!("| Phase | Measure | Legacy | Plan | ratio |");
        println!("|---|---|---|---|---|");
        type Value = fn(&Measured) -> f64;
        let measures: [(&str, Value); 12] = [
            ("throughput (metrics/s)", |m| m.throughput),
            ("encoded (metrics/s)", |m| m.encoded),
            ("process CPU ns/metric", |m| m.cpu_per_metric),
            ("allocated bytes/metric", |m| m.bytes_per_metric),
            ("live MiB at the end", |m| m.live_mib),
            ("primary busy ns/metric", |m| m.busy_per_metric),
            ("dropped share", |m| m.dropped),
            ("refused share", |m| m.refused),
            ("p50 latency (µs)", |m| m.p50),
            ("p99 latency (µs)", |m| m.p99),
            ("worst latency (µs)", |m| m.worst),
            ("sentinels lost", |m| m.lost as f64),
        ];
        for (index, phase) in PHASES.iter().enumerate() {
            for (measure, value) in measures {
                if index == 0 && (measure.contains("latency") || measure.contains("sentinels")) {
                    continue;
                }
                let (a, b) = (median(&legacy, index, value), median(&plan, index, value));
                println!("| {} | {} | {:.1} | {:.1} | {} |", phase, measure, a, b, ratio(a, b));
            }
        }
        for (name, rounds) in [("legacy", &legacy), ("plan", &plan)] {
            let throughput = median(rounds, 0, |m| m.throughput);
            let one = *first_throughput.entry(name).or_insert(throughput);
            println!(
                "{}: {:.0} metrics/s per worker; T(W) / T(W_first) = {:.2}",
                name,
                throughput / workers as f64,
                throughput / one
            );
        }
    }
}

/// Returns `b / a` for a table, or `-` when `a` is zero.
fn ratio(a: f64, b: f64) -> String {
    if a == 0.0 {
        "-".to_string()
    } else {
        format!("{:.3}", b / a)
    }
}

/// Prints what each probe whose name starts with one of `prefixes` recorded in `measured`: durations as the share of
/// the window they add up to (more than 1 when several tasks wait at once), counts, means, and percentiles.
fn print_probes(label: &str, measured: &Measured, prefixes: &[&str]) {
    let window = measured.window.as_nanos() as f64;
    for (name, probe) in measured
        .probes
        .iter()
        .filter(|(name, _)| prefixes.iter().any(|prefix| name.starts_with(prefix)))
    {
        if name.starts_with("dispatch_items") {
            println!(
                "| {} | {} | {} buffers | mean {:.1} items | p50 {} | p99 {} |",
                label,
                name,
                probe.count,
                probe.mean(),
                probe.quantile(0.5),
                probe.quantile(0.99)
            );
        } else {
            println!(
                "| {} | {} | {:.3} of the window | {} records | mean {:.0} ns | p99 {} ns |",
                label,
                name,
                probe.sum as f64 / window,
                probe.count,
                probe.mean(),
                probe.quantile(0.99)
            );
        }
    }
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn one_saturated_stream_connection() {
    // M1: one Unix stream connection, decoded by its own task, saturated. Legacy pipelines the pre-aggregation chain
    // over its transforms' tasks, and the plan over R1's regions today; without R1 (stage 9), the plan runs the chain
    // on one lane per connection.
    let knobs = Knobs::from_env(Knobs {
        workers: vec![4],
        transport: Transport::UnixStream,
        rounds: 3,
        generators: Some(1),
        ..Knobs::curves()
    });
    for &workers in &knobs.workers {
        let (mut legacy, mut plan) = (Vec::new(), Vec::new());
        for _ in 0..knobs.rounds {
            for (path, rounds) in [(MetricsPath::Legacy, &mut legacy), (MetricsPath::Plan, &mut plan)] {
                let mut fed = Fed::start(path, workers, knobs.profile.config(), &knobs);
                rounds.push(vec![fed.phase(None, false, Duration::from_secs(2), knobs.window)]);
                fed.stop();
            }
        }
        let (a, b) = (median(&legacy, 0, |m| m.throughput), median(&plan, 0, |m| m.throughput));
        let (ca, cb) = (
            median(&legacy, 0, |m| m.cpu_per_metric),
            median(&plan, 0, |m| m.cpu_per_metric),
        );
        println!(
            "M1, one stream connection, W = {}: legacy {:.0} metrics/s ({:.0} ns/metric), plan {:.0} metrics/s ({:.0} \
             ns/metric), ratio {:.3}",
            workers,
            a,
            ca,
            b,
            cb,
            b / a
        );
    }
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn decoder_pool_waits() {
    // M2: the datagram decoders' waits on the receiver mutex they share, and, on the plan's path, on the admission
    // point of `dsd_in.metrics`, saturated, at D decoders with one and 50 metrics per datagram. The admission lock's
    // hold share is the utilization of the serial point; the receiver's lock waits, summed over decoders, against D
    // windows, decide O4 (above 10%).
    let knobs = Knobs::from_env(Knobs {
        transport: Transport::datagram(),
        rounds: 1,
        ..Knobs::curves()
    });
    let decoder_counts = std::env::var("SALUKI_MEASURE_DECODER_COUNTS")
        .ok()
        .map(|list| list.split(',').map(|count| count.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1usize, 2, 4, 8]);
    let per_datagram = std::env::var("SALUKI_MEASURE_PER_DATAGRAM_LIST")
        .ok()
        .map(|list| list.split(',').map(|count| count.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1usize, 50]);
    let workers_override = std::env::var("SALUKI_MEASURE_WORKERS")
        .is_ok()
        .then(|| knobs.workers[0]);
    probes::set_timing(true);
    println!(
        "M2, {:?}: | D, metrics per datagram, path | probe | share of the window | ... |",
        knobs.transport
    );
    for &decoders in &decoder_counts {
        for &per in &per_datagram {
            let knobs = Knobs {
                decoders: Some(decoders),
                per_datagram: per,
                ..knobs.clone()
            };
            // Two workers beyond the decoders by default, so that the topology downstream of them stays busy too.
            let workers = workers_override.unwrap_or(decoders + 2);
            for path in [MetricsPath::Legacy, MetricsPath::Plan] {
                let mut fed = Fed::start(path, workers, knobs.profile.config(), &knobs);
                let measured = fed.phase(None, false, Duration::from_secs(2), knobs.window);
                fed.stop();
                let label = format!("D = {}, {} per datagram, W = {}, {:?}", decoders, per, workers, path);
                println!(
                    "| {} | throughput | {:.0} metrics/s | dropped {:.3} (refused {:.3}) | {:.0} ns/metric |",
                    label, measured.throughput, measured.dropped, measured.refused, measured.cpu_per_metric
                );
                print_probes(
                    &label,
                    &measured,
                    &[
                        "dogstatsd_receiver_lock_wait/dsd_in",
                        "dogstatsd_receiver_recv_wait/dsd_in",
                        "admission_lock_wait/dsd_in.metrics",
                        "admission_lock_hold/dsd_in.metrics",
                        "dispatch_send_wait/dsd_in.metrics",
                        DECODED,
                    ],
                );
            }
        }
    }
    probes::set_timing(false);
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn source_buffer_fill() {
    // M6: how full the source's buffers are when it dispatches them, saturated, at 70% of saturation, and with many
    // stream connections at a modest rate each (500 by default: 50 generators of 10 connections); every buffer a
    // decoder flushes on its 100 ms timer is partial. Stage 9's routed buffers inherit this fill.
    let knobs = Knobs::from_env(Knobs {
        workers: vec![4],
        rounds: 1,
        ..Knobs::curves()
    });
    let workers = knobs.workers[0];
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let mut fed = Fed::start(path, workers, knobs.profile.config(), &knobs);
        let saturated = fed.phase(None, false, Duration::from_secs(2), knobs.window);
        let seventy = fed.phase(
            Some(saturated.throughput * 0.7),
            true,
            Duration::from_secs(1),
            knobs.window,
        );
        fed.stop();
        print_probes(&format!("{:?}, saturated", path), &saturated, &[DECODED]);
        print_probes(&format!("{:?}, 70% of saturation", path), &seventy, &[DECODED]);
    }
    let many = Knobs::from_env(Knobs {
        workers: vec![workers],
        transport: Transport::UnixStream,
        generators: Some(50),
        connections_per_generator: 10,
        rate: Some(500_000.0),
        rounds: 1,
        ..Knobs::curves()
    });
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let mut fed = Fed::start(path, workers, many.profile.config(), &many);
        let measured = fed.phase(many.rate, true, Duration::from_secs(1), many.window);
        fed.stop();
        let connections = many.generators_at(workers) * many.connections_per_generator;
        print_probes(
            &format!(
                "{:?}, {} stream connections at {:.0} metrics/s",
                path,
                connections,
                many.rate.unwrap_or(0.0)
            ),
            &measured,
            &[DECODED],
        );
        println!(
            "| {:?} | p99 latency {:.0} µs over {} sentinels |",
            path, measured.p99, measured.sentinels
        );
    }
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn tap_cost() {
    // M7: what the always-connected tap `dsd_stats_out` costs at saturation: the decoders' time cloning each buffer for
    // it (`dispatch_clone`), and the destination's own CPU time dropping the clones, which its resource group records
    // on Linux only. The tap gate (P9.F) removes both while no statistics collection runs; rerun this after it lands.
    let knobs = Knobs::from_env(Knobs {
        workers: vec![4],
        rounds: 1,
        ..Knobs::curves()
    });
    probes::set_timing(true);
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let mut fed = Fed::start(path, knobs.workers[0], knobs.profile.config(), &knobs);
        let measured = fed.phase(None, false, Duration::from_secs(2), knobs.window);
        fed.stop();
        let label = format!("{:?}, saturated at {:.0} metrics/s", path, measured.throughput);
        print_probes(
            &label,
            &measured,
            &["dispatch_clone/dsd_in.metrics", "dispatch_send_wait/dsd_in.metrics"],
        );
        for (group, cpu) in measured
            .groups_cpu
            .iter()
            .filter(|(group, _)| group.contains("dsd_stats_out"))
        {
            println!(
                "| {} | resource group {} | {:.3} of a core |",
                label,
                group,
                *cpu as f64 / measured.window.as_nanos() as f64
            );
        }
    }
    probes::set_timing(false);
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn flush_storm() {
    // M8: aggregation ticks over many contexts (a million by default), at a paced rate that sends every context several
    // times between two ticks, over datagrams that a full socket drops, as DogStatsD clients' are: UDP on macOS, Unix
    // datagrams on Linux, sent without waiting for room. The distinct workload gives seven lines in eight a context of
    // their own, so each tick flushes every context, one metric per context for each 10 s bucket it closes, and the
    // window covers at least two of the 15 s flush intervals. Every 100 ms the timeline records what the generators
    // sent, what the sockets refused, what the source dispatched, what has gone missing so far (sent less dispatched:
    // drops, and what the queues hold), what aggregation emitted, how long aggregation's output waited on its channel
    // (the flush's time blocked on output), and the sentinels' p99; a tick shows as a burst of aggregation output.
    let knobs = Knobs::from_env(Knobs {
        workers: vec![8],
        transport: Transport::datagram(),
        drop_when_full: true,
        workload: Workload::Distinct,
        contexts: 1_000_000,
        rate: Some(400_000.0),
        window: Duration::from_secs(45),
        rounds: 1,
        ..Knobs::curves()
    });
    let workers = knobs.workers[0];
    let rate = knobs.rate.unwrap_or(400_000.0);
    // Room for every context, above the default limit of a million.
    let config = format!(
        "{}aggregate_context_limit: {}\n",
        knobs.profile.config(),
        2 * knobs.contexts.max(1_000_000)
    );
    probes::set_timing(true);
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let mut fed = Fed::start(path, workers, &config, &knobs);
        fed.generators.sentinels.store(true, Ordering::Relaxed);
        fed.generators.pace.set(rate as u64);
        println!(
            "\nM8, {:?}, {:?} (dropped when full: {}), W = {}, {} contexts ({:?}) at {:.0} metrics/s: | t (s) | sent | \
             refused | dispatched | missing so far | aggregation out | aggregation send wait (ms) | sentinel p99 \
             (µs) |",
            path,
            knobs.transport,
            knobs.drop_when_full,
            workers,
            knobs.contexts,
            knobs.workload,
            rate
        );
        let started = Instant::now();
        let first = fed.snapshot();
        let mut previous = fed.snapshot();
        while started.elapsed() < knobs.window {
            let slot_start = Instant::now();
            fed.arrivals.clear();
            fed.collect_until(slot_start + Duration::from_millis(100));
            let now = fed.snapshot();
            let sum = |snapshot: &Snapshot, prefix: &str| -> u64 {
                snapshot
                    .probes
                    .iter()
                    .filter(|(name, _)| name.starts_with(prefix))
                    .map(|(_, probe)| probe.sum)
                    .sum()
            };
            let mut latencies = fed
                .arrivals
                .iter()
                .filter_map(|(id, arrived)| Some(arrived.saturating_duration_since(fed.generators.sent_at(*id)?)))
                .collect::<Vec<_>>();
            latencies.sort();
            let p99 = latencies
                .get(((latencies.len().max(1) - 1) as f64 * 0.99) as usize)
                .map_or(0.0, |latency| latency.as_secs_f64() * 1e6);
            println!(
                "| {:.1} | {} | {} | {} | {} | {} | {:.1} | {:.0} |",
                started.elapsed().as_secs_f64(),
                now.sent - previous.sent,
                now.refused - previous.refused,
                now.decoded - previous.decoded,
                (now.sent - first.sent).saturating_sub(now.decoded - first.decoded),
                sum(&now, "dispatch_items/dsd_agg") - sum(&previous, "dispatch_items/dsd_agg"),
                (sum(&now, "dispatch_send_wait/dsd_agg") - sum(&previous, "dispatch_send_wait/dsd_agg")) as f64 / 1e6,
                p99
            );
            previous = now;
        }
        // Once the generators pause and the source has dispatched what its queues held, what is still missing was
        // dropped.
        fed.pause();
        let last = fed.snapshot();
        let outputs = last
            .probes
            .keys()
            .filter(|name| name.starts_with("dispatch_items/dsd_agg"))
            .cloned()
            .collect::<Vec<_>>();
        println!(
            "({} metrics sent, {} refused by the sockets, {} dropped in all; {} sentinels sent; aggregation's outputs: \
             {:?})",
            last.sent - first.sent,
            last.refused - first.refused,
            (last.sent - first.sent).saturating_sub(last.decoded - first.decoded),
            fed.generators.sentinels_sent(),
            outputs
        );
        fed.stop();
    }
    probes::set_timing(false);
}

/// Runs `pacer` under the setting `pace` from `from` for `length` on a simulated clock, as a generator does, sending a
/// payload of `payload` metrics whenever it says to, and returns the metrics sent.
fn paced_for(
    pacer: &mut Pacer, pace: (u64, u64), generators: usize, payload: u64, from: Instant, length: Duration,
) -> u64 {
    let (mut now, mut sent) = (from, 0);
    while now < from + length {
        match pacer.step(pace, generators, now) {
            Step::Send => {
                pacer.sent(payload);
                sent += payload;
            }
            Step::Wait(wait) => now += wait,
            Step::Pause => now += Duration::from_millis(1),
        }
    }
    sent
}

#[test]
fn a_generator_resumed_at_the_rate_it_paused_at_does_not_catch_up_on_the_pause() {
    // Two generators share 10,000 metrics per second, 5,000 each, in payloads of 50.
    let (rate, generators, payload) = (10_000, 2, 50);
    let share = rate / generators as u64;
    let start = Instant::now();
    let mut pacer = Pacer::default();
    let paced = paced_for(
        &mut pacer,
        (1, rate),
        generators,
        payload,
        start,
        Duration::from_secs(1),
    );
    assert!(
        (share..=share + payload).contains(&paced),
        "a second at the rate sends its share, not {}",
        paced
    );

    let paused = paced_for(
        &mut pacer,
        (2, 0),
        generators,
        payload,
        start + Duration::from_secs(1),
        Duration::from_secs(2),
    );
    assert_eq!(paused, 0, "a paused generator sends nothing");

    // Resumed at the same rate, a second sends a second's share, not the three seconds' worth since the first phase
    // started.
    let resumed = paced_for(
        &mut pacer,
        (3, rate),
        generators,
        payload,
        start + Duration::from_secs(3),
        Duration::from_secs(1),
    );
    assert!(
        (share..=share + payload).contains(&resumed),
        "a second after the pause sends its share, not {}",
        resumed
    );
}

#[test]
fn a_flooding_generator_never_waits_and_a_paused_one_never_sends() {
    let mut pacer = Pacer::default();
    let now = Instant::now();
    for _ in 0..1_000 {
        assert_eq!(pacer.step((1, FLOOD), 4, now), Step::Send);
        pacer.sent(50);
    }
    assert_eq!(pacer.step((2, 0), 4, now), Step::Pause);
}

#[test]
fn every_setting_of_the_pace_starts_a_new_epoch_even_at_the_same_rate() {
    let pace = Pace::new();
    pace.set(100_000);
    let (epoch, rate) = pace.get();
    assert_eq!(rate, 100_000);
    pace.set(100_000);
    assert_eq!(pace.get(), (epoch + 1, 100_000));
    pace.set(u64::MAX);
    assert_eq!(pace.get(), (epoch + 2, FLOOD));
    pace.set(0);
    assert_eq!(pace.get(), (epoch + 3, 0));
}

/// Starts `path` at one worker over `transport` with the configuration `profile`, paces a second of traffic with
/// sentinels, and checks the harness's wiring end to end: a sentinel reached the main encoder (which starting waits
/// for), the source dispatched what the generators sent, the topology holds memory, and the encoders' payloads reached
/// the forwarders, which count what they carry.
fn check_the_harness(path: MetricsPath, transport: Transport, profile: Profile) {
    let knobs = Knobs {
        workers: vec![1],
        transport,
        profile,
        rounds: 1,
        window: Duration::from_secs(1),
        contexts: 64,
        generators: Some(1),
        rate: Some(10_000.0),
        ..Knobs::curves()
    };
    let mut fed = Fed::start(path, 1, knobs.profile.config(), &knobs);
    let measured = fed.phase(knobs.rate, true, Duration::from_millis(100), knobs.window);
    assert!(
        measured.throughput > 0.0,
        "the source dispatches what the generators send"
    );
    assert!(
        measured.sentinels + measured.lost > 0,
        "the generators send sentinels in a paced phase"
    );
    assert!(measured.live_mib > 0.0, "the running topology holds memory");
    // The encoders send a payload that is not full when their flush timeout expires.
    let deadline = Instant::now() + Duration::from_secs(30);
    while fed.forwarded() == 0 {
        assert!(Instant::now() < deadline, "the encoders' payloads reach the forwarders");
        fed.collect_until(Instant::now() + Duration::from_millis(100));
    }
    fed.stop();
}

#[test]
fn the_source_fed_harness_drives_the_legacy_path_with_gateways_over_udp() {
    check_the_harness(MetricsPath::Legacy, Transport::Udp, Profile::Gateways);
}

#[test]
fn the_source_fed_harness_drives_the_plan_path_with_the_defaults_over_unix_sockets() {
    check_the_harness(MetricsPath::Plan, Transport::named("uds"), Profile::Defaults);
}

#[test]
fn a_datagram_generator_that_drops_when_full_counts_what_a_full_socket_refuses() {
    // A source that never reads: its socket's queue fills, and a sender that does not wait for room has its datagrams
    // refused, as a DogStatsD client's are. A sender that waits would block here instead, and drops would never show.
    let listener = Listener::new(Transport::UnixDatagram);
    let _source = UnixDatagram::bind(&listener.path).expect("the source's socket binds");
    let mut sender = listener.sender(true);
    let (refused, sends) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let refused_at = (0..100_000).find(|_| !sender.send(b"app.queue1:7|g|#team:core"));
        let _ = refused.send(refused_at);
    });
    let refused_at = sends
        .recv_timeout(Duration::from_secs(10))
        .expect("a sender that drops when full never waits for room");
    assert!(refused_at.is_some(), "a full socket refuses datagrams");
}

#[test]
fn the_distinct_workload_gives_every_context_a_line_of_its_own() {
    // Each rotation of the distinct workload sends every context at least once, so a flush after a rotation holds them
    // all, beside the timestamped counters that keep the passthrough flowing; the mixed workload's rotation sends each
    // context index once per eight lines, with one context of its own per index.
    let contexts = 10_000;
    let names = |workload: Workload| {
        (0..workload.lines(contexts))
            .map(|index| {
                let line = workload.line(index, contexts);
                line[..line.find(':').expect("a line has a value")].to_string()
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(Workload::Distinct.lines(contexts), contexts.div_ceil(7) * 8);
    let distinct = names(Workload::Distinct);
    assert_eq!(
        distinct
            .iter()
            .filter(|name| !name.starts_with("distinct.jobs"))
            .count(),
        contexts
    );
    assert_eq!(
        distinct.iter().filter(|name| name.starts_with("distinct.jobs")).count(),
        32
    );
    assert_eq!(Workload::Mixed.lines(contexts), 8 * contexts);
    let mixed = names(Workload::Mixed);
    assert_eq!(
        mixed.iter().filter(|name| name.starts_with("app.queue")).count(),
        contexts
    );
    // Mapped counters, a blocked name, one distribution, disks, queues, and timestamped jobs.
    assert_eq!(mixed.len(), 16 + 1 + 1 + 8 + contexts + 32);
}

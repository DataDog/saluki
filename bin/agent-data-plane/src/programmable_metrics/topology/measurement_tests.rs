//! Throughput, CPU, and latency of the plan-driven topology against the legacy one, in process, checked against the
//! stage gate: throughput, CPU, and p99 latency no worse than legacy.
//!
//! Each round runs both primary topologies (see [`test_topology`](crate::programmable_metrics::test_topology)) with
//! native aggregation, on the connected configuration path, in the layout production uses: the components of the
//! topology, the configuration updates worker, and the installer share one runtime, as they share ADP's primary
//! runtime, and the compute-heavy subtasks of components, such as the encoders' request builders, run on a dedicated
//! global pool of eight threads, as ADP's do. The metrics encoders are the real ones: they encode and compress what
//! they receive into payloads, which forwarders that discard them count and drop; the test observes, at each encoder's
//! input, when its markers arrive (see [`Encoders::Real`]). The feeding runs on the test's own thread. The workload is
//! the 1,024-metric DogStatsD-like batch of the plan crate's `region_driver` bench (mapped counters, a blocked name,
//! distributions whose `user_id` tag the tag filterlist removes, gauges, and timestamped counters for the passthrough),
//! with one timestamped sentinel per batch, which reaches the main output through every region. Its mapper profile
//! makes the pre-aggregation chain costly enough that R1's cost model pipelines it over two regions. A second
//! configuration, [`GATEWAYS`], adds two endpoint routing groups and Multi-Region Failover mirroring an allowlist,
//! whose gateways the legacy topology runs as components of their own and the plan-driven one as `split`s of its policy
//! region, one decision of the enrichment region. Four phases are measured:
//!
//! - **Saturation**: batches are fed as fast as the topology takes them; throughput is metrics per second until the
//!   last sentinel arrives.
//! - **Paced**: batches are fed at half the saturated rate of the legacy topology; latency is each sentinel's time from
//!   being fed to arriving at the main output (p50, p99, and the worst).
//! - **Paced with live updates**: the same, while the metric filterlist changes every 50 ms, which the legacy
//!   components apply through their live views and the plan-driven topology installs as new generations.
//! - **Paced passthrough**: batches of timestamped counters only, so that the native passthrough batcher fills and
//!   sends one buffer per batch, as regions do: sentinel latency then compares the paths themselves, region hops and
//!   all, rather than the native batching that the earlier phases' latency is mostly made of.
//!
//! CPU is the process's CPU time per metric (`getrusage`), less the feeding thread's own, so that the global pool's
//! threads, where the request builders run, and the blocking threads around kernel builds count, as well as the
//! installer's lowering and preparations; the primary runtime's worker busy time per metric is reported beside it. Off
//! Unix the process's CPU time is not read, and reads zero.
//! Allocations are every byte and object the process allocated per metric, which the test binary's tracking allocator
//! counts; bytes are checked, and objects and the bytes the process holds at the end of each phase are reported.
//!
//! The tolerances are the run-to-run noise of the legacy topology measured against itself, which
//! `the_legacy_topology_against_itself_stays_within_the_tolerances` checks (medians of five rounds, four workers,
//! Apple Silicon): throughput within 7%, CPU and allocated bytes within 7%, and p99 latency within 10%. The
//! measurements are ignored tests: their thresholds only hold in release mode on an otherwise idle machine. Run them
//! with `cargo nextest run -p agent-data-plane --release --features measurement --run-ignored only -E
//! 'test(/measurement_tests/)' --no-capture --test-threads 1`: no two may run at once. The tests that are not ignored
//! check the harness itself, quickly: that the real encoders are observed and their payloads counted.
//!
//! The submodules measure what the plan's stage 9 decides on first (plan section 8.6), and need ADP's `measurement`
//! feature, which compiles the probes of saluki-core and of the DogStatsD source: `source_feed` feeds both topologies
//! through the real DogStatsD source, over loopback UDP and Unix sockets, at any number of workers (the scaling curves,
//! and M1, M2, M6 to M8); `key_skew` measures how DogStatsD contexts spread over key groups and state lanes (M5).
//! `per_buffer_fixed_cost` and `allocator_group_contention` here measure the cost of a buffer apart from its events
//! and what it is made of (M6). `make test-measurement` runs the tests of the harness and the probes that are not
//! ignored, with the feature.

#[cfg(feature = "measurement")]
mod key_skew;
#[cfg(all(unix, feature = "measurement"))]
mod source_feed;

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceGroupToken, ResourceStatsSnapshot};
use saluki_core::data_model::{
    event::metric::{context::Context, Metric, ScalarPoints},
    tags::{Tag, TagSet},
};
use serde_json::json;
use tokio::{
    runtime::Handle,
    time::{interval, Instant, MissedTickBehavior},
};

use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        test_support::{connected_configuration_system, update},
        test_topology::{Aggregation, Encoders, MetricsPath, Options, TestTopology},
    },
};

/// The configuration both topologies run: every fused DogStatsD transform has work to do.
const CONFIG: &str = r#"
dogstatsd_mapper_profiles:
  - name: requests
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: requests
        tags:
          route: $1
metric_filterlist: [raw.blocked]
metric_tag_filterlist:
  - metric_name: checkout.latency
    action: exclude
    tags: [user_id]
"#;

/// The configuration of the gateway measurements: [`CONFIG`], with two endpoint routing groups, one of names and one of
/// prefixes, that select some of the batch, and Multi-Region Failover mirroring the distributions.
const GATEWAYS: &str = r#"
dogstatsd_mapper_profiles:
  - name: requests
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: requests
        tags:
          route: $1
metric_filterlist: [raw.blocked]
metric_tag_filterlist:
  - metric_name: checkout.latency
    action: exclude
    tags: [user_id]
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [checkout.latency, system.disk0, system.disk1]
    metric_prefix_allowlist:
      https://c.example.com: [app.queue, requests.]
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: https://mrf.example.com
  failover_metrics: true
  metric_allowlist: [checkout.latency]
"#;

/// The metrics of a batch, the sentinel included.
const BATCH: usize = 1024;

/// The name of the sentinel of every batch.
const SENTINEL: &str = "zz.sentinel";

/// The rounds each path runs, alternating.
const ROUNDS: usize = 5;

/// The phases of a round.
const PHASES: [&str; 4] = ["saturated", "paced", "paced, live updates", "paced passthrough"];

/// The largest throughput below legacy's, relative to it, that run-to-run noise explains.
const THROUGHPUT_TOLERANCE: f64 = 0.07;

/// The largest CPU or allocated bytes per metric above legacy's, relative to it, that run-to-run noise explains.
const CPU_TOLERANCE: f64 = 0.07;

/// The largest p99 latency above legacy's, relative to it, that run-to-run noise explains.
const LATENCY_TOLERANCE: f64 = 0.10;

/// The threads of the global pool, as ADP's: the request builders of the encoders run there.
const GLOBAL_POOL_THREADS: usize = 8;

/// Returns the CPU time the process has used so far, user and system, over every thread it has run.
#[cfg(unix)]
fn process_cpu() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable memory of the size `getrusage` writes, and `RUSAGE_SELF` is valid.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(result, 0, "getrusage(RUSAGE_SELF) succeeds");
    // SAFETY: `getrusage` succeeded, so it initialized `usage`.
    let usage = unsafe { usage.assume_init() };
    let time = |time: libc::timeval| Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1_000);
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// Returns the CPU time the process has used so far: nothing off Unix, where these measurements do not read it.
#[cfg(not(unix))]
fn process_cpu() -> Duration {
    Duration::ZERO
}

/// Returns the CPU time the calling thread has used so far.
#[cfg(unix)]
fn thread_cpu() -> Duration {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `time` is a valid `timespec` to write to, and `CLOCK_THREAD_CPUTIME_ID` is supported on Linux and macOS.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    assert_eq!(result, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) succeeds");
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

/// Returns the CPU time the calling thread has used so far: nothing off Unix, where these measurements do not read it.
#[cfg(not(unix))]
fn thread_cpu() -> Duration {
    Duration::ZERO
}

/// Returns a runtime standing in for ADP's global thread pool, which the topology runs the encoders' request builders
/// on.
fn global_pool() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(GLOBAL_POOL_THREADS)
        .thread_name("global-pool")
        .enable_all()
        .build()
        .expect("the global pool builds")
}

fn tags(tags: &[String]) -> TagSet {
    tags.iter().map(|tag| Tag::from(tag.as_str())).collect()
}

/// Returns the `region_driver` bench's batch without its last metric, which each batch replaces with its sentinel:
/// [`BATCH`] - 1 metrics over 256 context indexes.
fn sequence() -> Vec<Metric> {
    (0..BATCH - 1)
        .map(|i| {
            let context = i % 256;
            match i % 8 {
                0 | 1 => Metric::counter(
                    Context::from_parts(
                        format!("raw.requests.route{}", context % 16),
                        tags(&["env:prod".to_string()]),
                    ),
                    1.0,
                ),
                2 => Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
                3 | 4 => Metric::distribution(
                    Context::from_parts(
                        "checkout.latency",
                        tags(&[format!("user_id:{}", context), "env:dev".to_string()]),
                    ),
                    10.0,
                ),
                5 => Metric::gauge(
                    Context::from_parts(format!("system.disk{}", context % 8), TagSet::default()),
                    9.0,
                ),
                6 => Metric::gauge(
                    Context::from_parts(format!("app.queue{}", context), tags(&["team:core".to_string()])),
                    7.0,
                ),
                _ => timestamped(format!("app.jobs{}", context % 32)),
            }
        })
        .collect()
}

/// Returns the templates that batches of `batch` metrics, the sentinel included, cycle through: consecutive slices of
/// [`sequence`], wrapping, so that over a cycle batches of any size carry the same metrics and contexts, in the same
/// proportions, as batches of [`BATCH`] metrics, whose one template is the bench's batch.
fn templates(batch: usize) -> Vec<Vec<Metric>> {
    let sequence = sequence();
    let len = batch - 1;
    // `sequence.len() / gcd(sequence.len(), len)` slices of `len` metrics cover the sequence a whole number of times.
    let (mut gcd, mut rest) = (sequence.len(), len);
    while rest != 0 {
        (gcd, rest) = (rest, gcd % rest);
    }
    (0..sequence.len() / gcd)
        .map(|slice| {
            (0..len)
                .map(|i| sequence[(slice * len + i) % sequence.len()].clone())
                .collect()
        })
        .collect()
}

/// Returns a batch of `batch` timestamped counters only, over 256 contexts, without its sentinel: the native
/// passthrough batcher sends each full batch on as soon as it arrives, since it fills one buffer.
fn passthrough_template(batch: usize) -> Vec<Metric> {
    (0..batch - 1)
        .map(|i| timestamped(format!("app.jobs{}", i % 256)))
        .collect()
}

fn timestamped(name: String) -> Metric {
    Metric::counter(
        Context::from_parts(name, TagSet::default()),
        ScalarPoints::from_iter([(std::num::NonZeroU64::new(1_700_000_000), 2.0)]),
    )
}

/// Returns `count` batches from `first` on: copies of the templates in turn, which share their contexts as a source's
/// resolved contexts do, each with its sentinel. They are built before a phase starts, so that building them is not
/// measured.
fn batches(templates: &[Vec<Metric>], first: usize, count: usize) -> Vec<Vec<Metric>> {
    (first..first + count)
        .map(|index| {
            let mut batch = templates[index % templates.len()].clone();
            batch.push(Metric::counter(
                Context::from_parts(SENTINEL, tags(&[format!("batch:{}", index)])),
                (1_700_000_000, 1.0),
            ));
            batch
        })
        .collect()
}

/// Returns a batch of timestamped counters, which fills the native passthrough batcher's buffer, which flushes it.
fn flush_batch() -> Vec<Metric> {
    (0..BATCH)
        .map(|_| Metric::counter(Context::from_static_name("zz.flush"), (1_700_000_000, 1.0)))
        .collect()
}

fn sentinel_batch(metric: &Metric) -> usize {
    metric
        .context()
        .tags()
        .into_iter()
        .find_map(|tag| tag.as_str().strip_prefix("batch:").and_then(|index| index.parse().ok()))
        .expect("a sentinel names its batch")
}

/// Returns what every resource group of the process has allocated so far, and holds now: allocated bytes and objects,
/// and live bytes. The test binary's global allocator is ADP's tracking allocator, so this counts every allocation.
fn allocations() -> (u64, u64, u64) {
    let empty = ResourceStatsSnapshot::empty();
    let mut totals = (0, 0, 0);
    ResourceGroupRegistry::global().visit_resource_groups(|_, stats| {
        let snapshot = stats.snapshot_delta(&empty);
        totals.0 += snapshot.allocated_bytes as u64;
        totals.1 += snapshot.allocated_objects as u64;
        totals.2 += snapshot.live_bytes() as u64;
    });
    totals
}

/// Returns the total time the workers of the runtime `handle` belongs to have spent busy.
fn busy(handle: &Handle) -> Duration {
    let metrics = handle.metrics();
    (0..metrics.num_workers())
        .map(|worker| metrics.worker_total_busy_duration(worker))
        .sum()
}

/// What one phase measured.
#[derive(Clone, Copy, Debug, Default)]
struct Measured {
    /// Metrics per second.
    throughput: f64,

    /// The process's CPU time per metric, less the feeding thread's, in nanoseconds.
    cpu_per_metric: f64,

    /// The primary runtime's worker busy time per metric, in nanoseconds.
    busy_per_metric: f64,

    /// Bytes and objects allocated per metric, by the whole process.
    bytes_per_metric: f64,
    allocations_per_metric: f64,

    /// The bytes the process holds once the phase's last sentinel arrived, in MiB.
    live_mib: f64,

    /// Sentinel latency percentiles and the worst, in microseconds.
    p50: f64,
    p99: f64,
    worst: f64,
}

/// Feeds `count` batches of `templates` in turn, starting at `first`, at most one per `period` (as fast as possible
/// without one), and returns what the phase measured once every sentinel has arrived at the main output.
async fn phase(
    topology: &mut TestTopology, templates: &[Vec<Metric>], first: usize, count: usize, period: Option<Duration>,
) -> Measured {
    let batch = templates[0].len() + 1;
    let batches = batches(templates, first, count);
    let mut fed = Vec::with_capacity(count);
    let started = Instant::now();
    let handle = Handle::current();
    let busy_before = busy(&handle);
    let (cpu_before, feeding_before) = (process_cpu(), thread_cpu());
    let (bytes_before, allocations_before, _) = allocations();
    let mut latencies = Vec::with_capacity(count);
    let mut ticks = period.map(|period| {
        let mut ticks = interval(period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Burst);
        ticks
    });
    let mut arrived = 0;
    for batch in batches {
        if let Some(ticks) = ticks.as_mut() {
            ticks.tick().await;
        }
        fed.push(Instant::now());
        topology.feed("dsd_in.metrics", batch);
        // Take what has arrived meanwhile, so that the collectors' channel stays short.
        while let Some(Some(delivery)) = futures::FutureExt::now_or_never(topology.next_delivery()) {
            for metric in delivery
                .metrics
                .iter()
                .filter(|metric| delivery.output == "dd_metrics_encode" && metric.context().name() == SENTINEL)
            {
                latencies.push(delivery.at - fed[sentinel_batch(metric) - first]);
                arrived += 1;
            }
        }
    }
    // The native passthrough batcher holds timestamped metrics until it has a full buffer of them, or has been idle
    // for a while (checked every two seconds): a full buffer of timestamped counters flushes what it holds, so that
    // the phase measures processing rather than that wait.
    topology.feed("dsd_in.metrics", flush_batch());
    while arrived < count {
        let delivery = tokio::time::timeout(Duration::from_secs(120), topology.next_delivery())
            .await
            .expect("every sentinel arrives")
            .expect("the outputs stay open");
        for metric in delivery
            .metrics
            .iter()
            .filter(|metric| delivery.output == "dd_metrics_encode" && metric.context().name() == SENTINEL)
        {
            latencies.push(delivery.at - fed[sentinel_batch(metric) - first]);
            arrived += 1;
        }
    }
    let elapsed = started.elapsed();
    let cpu = process_cpu().saturating_sub(cpu_before) - thread_cpu().saturating_sub(feeding_before);
    let (bytes_after, allocations_after, live) = allocations();
    let metrics = (count * batch) as f64;
    latencies.sort();
    let micros = |at: f64| latencies[((latencies.len() - 1) as f64 * at) as usize].as_secs_f64() * 1e6;
    Measured {
        throughput: metrics / elapsed.as_secs_f64(),
        cpu_per_metric: cpu.as_nanos() as f64 / metrics,
        busy_per_metric: (busy(&handle) - busy_before).as_nanos() as f64 / metrics,
        bytes_per_metric: (bytes_after - bytes_before) as f64 / metrics,
        allocations_per_metric: (allocations_after - allocations_before) as f64 / metrics,
        live_mib: live as f64 / (1024.0 * 1024.0),
        p50: micros(0.5),
        p99: micros(0.99),
        worst: micros(1.0),
    }
}

/// The measurements of one path in one round, by phase.
type Round = [Measured; 4];

/// Starts the topology of `path` with the configuration `config`, with real encoders whose request builders run on
/// `pool`, returning it with the configuration stream the test changes it through.
async fn start(
    path: MetricsPath, config: &str, pool: &Handle,
) -> (
    TestTopology,
    tokio::sync::mpsc::Sender<saluki_config::dynamic::ConfigUpdate>,
) {
    let (system, updates, stream) = connected_configuration_system(config).await;
    let mut options = Options::new(path, Aggregation::Native);
    options.keep = |metric| metric.context().name() == SENTINEL;
    options.encoders = Encoders::Real;
    options.worker_pool = Some(pool.clone());
    let topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;
    (topology, stream)
}

/// Runs one round of `path` with the configuration `config`, pacing at `paced_rate` batches per second, or at half the
/// measured saturated rate when `None`.
async fn round(path: MetricsPath, config: &str, paced_rate: Option<f64>, pool: &Handle) -> Round {
    let (mut topology, stream) = start(path, config, pool).await;

    // Warm up: caches, allocations, and the first aggregation contexts.
    let templates = templates(BATCH);
    phase(&mut topology, &templates, 0, 200, None).await;
    let saturated = phase(&mut topology, &templates, 200, 2_000, None).await;
    let rate = paced_rate.unwrap_or(saturated.throughput / BATCH as f64 / 2.0);
    let period = Duration::from_secs_f64(1.0 / rate);
    let paced_batches = (rate * 2.0) as usize;
    let mut first = 2_200;
    let paced = phase(&mut topology, &templates, first, paced_batches, Some(period)).await;
    first += paced_batches;

    // The filterlist changes every 50 ms while batches flow. The stream stays open until the topology stops: the
    // configuration updates worker fails, and is restarted, when it closes.
    let changes_stream = stream.clone();
    let changes = tokio::spawn(async move {
        let mut ticks = interval(Duration::from_millis(50));
        for change in 0.. {
            ticks.tick().await;
            let blocked = if change % 2 == 0 {
                json!(["raw.blocked", "app.queue1"])
            } else {
                json!(["raw.blocked"])
            };
            if changes_stream.send(update("metric_filterlist", blocked)).await.is_err() {
                break;
            }
        }
    });
    let updating = phase(&mut topology, &templates, first, paced_batches, Some(period)).await;
    changes.abort();
    first += paced_batches;

    let passthrough = phase(
        &mut topology,
        &[passthrough_template(BATCH)],
        first,
        paced_batches,
        Some(period),
    )
    .await;
    drop(topology.stop().await);
    drop(stream);
    [saturated, paced, updating, passthrough]
}

/// Returns the median of a measurement over rounds.
fn median(rounds: &[Round], phase: usize, value: impl Fn(&Measured) -> f64) -> f64 {
    let mut values = rounds.iter().map(|round| value(&round[phase])).collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Reads one measure of a phase.
type Measure = fn(&Measured) -> f64;

/// The measures reported for each phase. The gate checks the throughput, the process CPU, and the allocated bytes;
/// the primary runtime's busy time is reported for continuity with the earlier stages' measurements.
const MEASURES: [(&str, Measure); 9] = [
    ("throughput (metrics/s)", |m| m.throughput),
    ("process CPU ns/metric", |m| m.cpu_per_metric),
    ("allocated bytes/metric", |m| m.bytes_per_metric),
    ("primary busy ns/metric", |m| m.busy_per_metric),
    ("allocations/metric", |m| m.allocations_per_metric),
    ("live MiB at the end", |m| m.live_mib),
    ("p50 latency (µs)", |m| m.p50),
    ("p99 latency (µs)", |m| m.p99),
    ("worst latency (µs)", |m| m.worst),
];

/// Measures `first` against `second` with the configuration `config` over [`ROUNDS`] rounds each, alternating them, on
/// a runtime of `workers` workers, and prints the medians as a table; returns the rounds of each.
async fn compare(first: MetricsPath, second: MetricsPath, workers: usize, config: &str) -> (Vec<Round>, Vec<Round>) {
    let pool = global_pool();
    let mut firsts = Vec::new();
    let mut seconds = Vec::new();
    // Both are paced at the same rate: half the saturated rate of the first in its first round.
    let mut rate = None;
    for _ in 0..ROUNDS {
        let round_first = round(first, config, rate, pool.handle()).await;
        let paced = *rate.get_or_insert(round_first[0].throughput / BATCH as f64 / 2.0);
        firsts.push(round_first);
        seconds.push(round(second, config, Some(paced), pool.handle()).await);
    }
    pool.shutdown_background();

    let gateways = if config == GATEWAYS { ", with gateways" } else { "" };
    println!(
        "{} runtime worker(s){}, {:?} against {:?}:",
        workers, gateways, second, first
    );
    println!("| Phase | Measure | {:?} | {:?} | ratio |", first, second);
    println!("|---|---|---|---|---|");
    for (index, phase) in PHASES.iter().enumerate() {
        for (measure, value) in MEASURES {
            if index == 0 && measure.contains("latency") {
                // Saturated latency is queueing in front of the source, not processing.
                continue;
            }
            let (a, b) = (median(&firsts, index, value), median(&seconds, index, value));
            println!("| {} | {} | {:.1} | {:.1} | {:.3} |", phase, measure, a, b, b / a);
        }
    }
    (firsts, seconds)
}

/// Checks the stage gate, within the tolerances: saturated throughput no lower than legacy's, process CPU and allocated
/// bytes per metric no higher in any phase, and p99 latency no higher in any paced phase. The number of allocations is
/// reported, not checked: each live update allocates the plans and programs the installer prepares, which the native
/// components' live views do not, but it allocates fewer bytes per metric than legacy even then.
fn check_gate(legacy: &[Round], plan: &[Round]) {
    let ratio = |phase: usize, value: Measure| median(plan, phase, value) / median(legacy, phase, value);
    let throughput = ratio(0, |m| m.throughput);
    assert!(
        throughput >= 1.0 - THROUGHPUT_TOLERANCE,
        "saturated throughput {:.3} of legacy",
        throughput
    );
    for (index, phase) in PHASES.iter().enumerate() {
        for (measure, value) in &MEASURES[1..3] {
            let cost = ratio(index, *value);
            assert!(
                cost <= 1.0 + CPU_TOLERANCE,
                "{}: {} {:.3} of legacy",
                phase,
                measure,
                cost
            );
        }
        if index > 0 {
            let p99 = ratio(index, |m| m.p99);
            assert!(
                p99 <= 1.0 + LATENCY_TOLERANCE,
                "{}: p99 latency {:.3} of legacy",
                phase,
                p99
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
async fn the_plan_driven_topology_is_no_slower_and_no_costlier_than_the_legacy_one_on_one_worker() {
    // With one worker, the topology is CPU-bound, and throughput follows CPU per metric.
    let (legacy, plan) = compare(MetricsPath::Legacy, MetricsPath::Plan, 1, CONFIG).await;
    check_gate(&legacy, &plan);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
async fn the_plan_driven_topology_is_no_slower_and_no_costlier_than_the_legacy_one_on_four_workers() {
    // With workers to spare, one saturated stream is bounded by the slowest stage of the chain: the legacy chain runs
    // its transforms as tasks of their own, and R1's cost model pipelines the regions so that none costs more.
    let (legacy, plan) = compare(MetricsPath::Legacy, MetricsPath::Plan, 4, CONFIG).await;
    check_gate(&legacy, &plan);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
async fn with_gateways_the_plan_driven_topology_is_no_slower_and_no_costlier_than_the_legacy_one_on_one_worker() {
    // The legacy gateways copy and filter each buffer in tasks of their own; the policy region decides where each
    // metric goes in the enrichment region, and copies only what a gateway keeps.
    let (legacy, plan) = compare(MetricsPath::Legacy, MetricsPath::Plan, 1, GATEWAYS).await;
    check_gate(&legacy, &plan);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
async fn with_gateways_the_plan_driven_topology_is_no_slower_and_no_costlier_than_the_legacy_one_on_four_workers() {
    let (legacy, plan) = compare(MetricsPath::Legacy, MetricsPath::Plan, 4, GATEWAYS).await;
    check_gate(&legacy, &plan);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement of the noise the tolerances allow: run it in release mode, as the module documentation says"]
async fn the_legacy_topology_against_itself_stays_within_the_tolerances() {
    let (first, second) = compare(MetricsPath::Legacy, MetricsPath::Legacy, 4, CONFIG).await;
    check_gate(&first, &second);
}

/// The batch sizes the per-buffer cost is measured at: full buffers of a topology, and an eighth of them.
const PER_BUFFER_BATCHES: [usize; 2] = [1_024, 128];

/// What the scripted feed costs per metric at saturation, by what it is made of.
#[derive(Clone, Debug, Default)]
struct BufferCosts {
    /// The process's CPU time, less the feeding thread's, in nanoseconds.
    cpu: f64,

    /// Objects and bytes allocated.
    objects: f64,
    bytes: f64,

    /// Tasks the primary runtime's workers polled: each poll of a component's task enters its resource group, which
    /// reads the thread's CPU time twice on Linux, a system call each.
    polls: f64,

    /// Times a worker of the primary runtime parked: a sleep, and the wake that ends it.
    parks: f64,

    /// Objects allocated, by resource group: each allocation and its free add to four counters of its group.
    groups: BTreeMap<String, f64>,
}

/// Returns the objects each resource group has allocated so far.
fn group_objects() -> BTreeMap<String, u64> {
    let empty = ResourceStatsSnapshot::empty();
    let mut groups = BTreeMap::new();
    ResourceGroupRegistry::global().visit_resource_groups(|name, stats| {
        groups.insert(name.to_string(), stats.snapshot_delta(&empty).allocated_objects as u64);
    });
    groups
}

/// Returns how many tasks the workers of the runtime `handle` belongs to have polled so far, and how many times they
/// parked.
fn polls_and_parks(handle: &Handle) -> (u64, u64) {
    let metrics = handle.metrics();
    (0..metrics.num_workers()).fold((0, 0), |(polls, parks), worker| {
        (
            polls + metrics.worker_poll_count(worker),
            parks + metrics.worker_park_count(worker),
        )
    })
}

/// Returns what `path` costs per metric at saturation with batches of `batch` metrics: the medians over [`ROUNDS`]
/// phases of `metrics` metrics each, on one topology, on the current runtime.
async fn saturated_costs(path: MetricsPath, batch: usize, metrics: usize, pool: &Handle) -> BufferCosts {
    let (mut topology, stream) = start(path, CONFIG, pool).await;
    let templates = templates(batch);
    let count = metrics / batch;
    let handle = Handle::current();
    phase(&mut topology, &templates, 0, count / 10, None).await;
    let mut rounds = Vec::new();
    for round in 0..ROUNDS {
        let first = count / 10 + round * count;
        let (groups_before, (polls_before, parks_before)) = (group_objects(), polls_and_parks(&handle));
        let measured = phase(&mut topology, &templates, first, count, None).await;
        let (groups_after, (polls_after, parks_after)) = (group_objects(), polls_and_parks(&handle));
        let per_metric = |value: u64| value as f64 / (count * batch) as f64;
        rounds.push(BufferCosts {
            cpu: measured.cpu_per_metric,
            objects: measured.allocations_per_metric,
            bytes: measured.bytes_per_metric,
            polls: per_metric(polls_after - polls_before),
            parks: per_metric(parks_after - parks_before),
            groups: groups_after
                .iter()
                .map(|(name, after)| {
                    let before = groups_before.get(name).copied().unwrap_or(0);
                    (name.clone(), per_metric(after.saturating_sub(before)))
                })
                .collect(),
        });
    }
    drop(topology.stop().await);
    drop(stream);

    let median = |value: &dyn Fn(&BufferCosts) -> f64| {
        let mut values = rounds.iter().map(value).collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    let names = rounds
        .iter()
        .flat_map(|round| round.groups.keys().cloned())
        .collect::<BTreeSet<_>>();
    BufferCosts {
        cpu: median(&|costs| costs.cpu),
        objects: median(&|costs| costs.objects),
        bytes: median(&|costs| costs.bytes),
        polls: median(&|costs| costs.polls),
        parks: median(&|costs| costs.parks),
        groups: names
            .into_iter()
            .map(|name| {
                let objects = median(&|costs| costs.groups.get(&name).copied().unwrap_or(0.0));
                (name, objects)
            })
            .collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
async fn per_buffer_fixed_cost() {
    // Plan section 8.6, M6: the cost of a buffer apart from its events, and what it is made of. With a cost per metric
    // `c(B) = a + f / B` at `B` events per buffer, two batch sizes give `f`, the fixed cost per buffer, and `a`, the
    // cost per event, for each measure: CPU, allocations, task polls, and worker parks, and allocations by resource
    // group. Batches of both sizes cycle through the same metrics and contexts (see `templates`). Each batch is one
    // buffer from the scripted source to the first region. On Linux, the CPU includes the CPU-time reads of each task
    // poll (F7); on macOS those reads cost nothing. `allocator_group_contention` measures whether the allocations'
    // counter writes contend.
    let pool = global_pool();
    let metrics = std::env::var("SALUKI_MEASURE_METRICS")
        .ok()
        .and_then(|metrics| metrics.parse().ok())
        .unwrap_or(2_048_000);
    let (inverse_full, inverse_small) = (1.0 / PER_BUFFER_BATCHES[0] as f64, 1.0 / PER_BUFFER_BATCHES[1] as f64);
    let fixed = |full: f64, small: f64| (small - full) / (inverse_small - inverse_full);
    let row = |path: MetricsPath, measure: &str, full: f64, small: f64| {
        let per_buffer = fixed(full, small);
        println!(
            "| {:?} | {} | {:.3} | {:.3} | {:.2} | {:.3} |",
            path,
            measure,
            full,
            small,
            per_buffer,
            full - per_buffer * inverse_full
        );
        per_buffer
    };
    println!("| Path | Measure | per metric at 1,024 | per metric at 128 | fixed per buffer | per event |");
    println!("|---|---|---|---|---|---|");
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let mut costs = Vec::new();
        for batch in PER_BUFFER_BATCHES {
            costs.push(saturated_costs(path, batch, metrics, pool.handle()).await);
        }
        let (full, small) = (&costs[0], &costs[1]);
        row(path, "CPU ns", full.cpu, small.cpu);
        let objects = row(path, "allocations", full.objects, small.objects);
        row(path, "allocated bytes", full.bytes, small.bytes);
        let polls = row(path, "task polls (primary runtime)", full.polls, small.polls);
        row(path, "worker parks (primary runtime)", full.parks, small.parks);
        // The groups that allocate per buffer, the most first.
        let mut groups = full
            .groups
            .iter()
            .map(|(name, objects)| {
                let small = small.groups.get(name).copied().unwrap_or(0.0);
                (fixed(*objects, small), name, *objects, small)
            })
            .filter(|(per_buffer, ..)| *per_buffer >= 0.05)
            .collect::<Vec<_>>();
        groups.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, name, objects, small) in groups.into_iter().take(10) {
            row(path, &format!("allocations in group `{}`", name), objects, small);
        }
        println!(
            "| {:?} | per buffer: {:.1} allocator counter writes (4 per allocation and its free), at most {:.1} \
             CPU-time reads on Linux (2 per poll) |",
            path,
            4.0 * objects,
            2.0 * polls
        );
    }
    pool.shutdown_background();
}

/// Returns the mean time, in nanoseconds, of one allocation and its free on each of `threads` threads allocating at
/// once, each entered into the resource group `group` gives it.
fn allocation_cost(threads: usize, group: impl Fn(usize) -> ResourceGroupToken + Sync) -> f64 {
    const ALLOCATIONS: usize = 2_000_000;
    let start = std::sync::Barrier::new(threads);
    let elapsed = std::thread::scope(|scope| {
        let workers = (0..threads)
            .map(|thread| {
                let (start, group) = (&start, &group);
                scope.spawn(move || {
                    let token = group(thread);
                    let _entered = token.enter();
                    start.wait();
                    let started = std::time::Instant::now();
                    for index in 0..ALLOCATIONS {
                        drop(std::hint::black_box(Box::new([index as u8; 48])));
                    }
                    started.elapsed()
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("an allocating thread does not panic"))
            .sum::<Duration>()
    });
    elapsed.as_nanos() as f64 / (threads * ALLOCATIONS) as f64
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn allocator_group_contention() {
    // Plan section 8.6, M6: whether the tracking allocator's counters contend when several threads allocate in one
    // resource group, as the DogStatsD source's decoders all do in the source's group. Each allocation and its free
    // add to four counters of the group it was allocated in, which share a cache line. The difference between the two
    // columns at D threads, times the allocations per buffer that `per_buffer_fixed_cost` reports (per event and per
    // buffer) and the buffers a decoder dispatches per second, is what per-decoder resource groups would save.
    let registry = ResourceGroupRegistry::global();
    let shared = registry.register_resource_group("measurement_allocations_shared");
    // Allocations of the counters' size between the groups, so that groups registered one after another do not share
    // a cache line, which per-decoder groups would have to avoid too.
    let mut spacers = Vec::new();
    let own = (0..16)
        .map(|thread| {
            spacers.extend((0..4).map(|_| Box::new([0u8; 40])));
            registry.register_resource_group(format!("measurement_allocations_{}", thread))
        })
        .collect::<Vec<_>>();
    println!("| threads | one group (ns per allocation and free) | a group per thread | difference |");
    println!("|---|---|---|---|");
    for threads in [1, 2, 4, 8, 16] {
        let one = allocation_cost(threads, |_| shared);
        let each = allocation_cost(threads, |thread| own[thread]);
        println!("| {} | {:.1} | {:.1} | {:.1} |", threads, one, each, one - each);
    }
    drop(spacers);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_encoders_are_observed_and_their_payloads_counted() {
    // The measurement topology's wiring, quickly, on both paths: the test observes the sentinels at the real encoders'
    // input, and the forwarders that discard the encoders' payloads count the metrics those carry. The measurements
    // above depend on both.
    let pool = global_pool();
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let (mut topology, stream) = start(path, CONFIG, pool.handle()).await;
        // A phase ends once every sentinel has reached the main encoder.
        let measured = phase(&mut topology, &templates(BATCH), 0, 2, None).await;
        assert!(measured.throughput > 0.0, "the {:?} path's sentinels arrive", path);
        // An encoder sends a payload that is not full when its flush timeout expires.
        tokio::time::timeout(Duration::from_secs(30), async {
            while topology.forwarded() == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the {:?} path's payloads reach the forwarders", path));
        drop(topology.stop().await);
        drop(stream);
    }
    pool.shutdown_background();
}

#[test]
fn batches_of_any_size_cycle_through_the_contexts_of_full_batches_in_the_same_proportions() {
    // The per-buffer cost compares batch sizes, so they must carry the same work: the same contexts, each as often per
    // metric, over a cycle of their templates.
    let counts = |batch: usize| {
        let mut counts = std::collections::HashMap::new();
        for metric in templates(batch).iter().flatten() {
            *counts.entry(metric.context().clone()).or_insert(0usize) += 1;
        }
        counts
    };
    let full = counts(BATCH);
    assert_eq!(templates(BATCH).len(), 1);
    for batch in [128, 100, 2] {
        let small = counts(batch);
        assert_eq!(
            small.len(),
            full.len(),
            "batches of {} carry as many contexts as full batches",
            batch
        );
        // A cycle covers the sequence a whole number of times, so each context appears that many times as often as in
        // one full batch.
        let times = small.values().sum::<usize>() / full.values().sum::<usize>();
        for (context, count) in &small {
            assert_eq!(
                Some(*count),
                full.get(context).map(|count| count * times),
                "batches of {} carry {:?} as often as full batches",
                batch,
                context
            );
        }
    }
}

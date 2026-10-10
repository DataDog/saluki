//! Throughput, CPU, and latency of the plan-driven topology against the legacy one, in process, checked against the
//! stage gate: throughput, CPU, and p99 latency no worse than legacy.
//!
//! Each round runs both primary topologies (see [`test_topology`](crate::programmable_metrics::test_topology)) with
//! native aggregation, on the connected configuration path, in the layout production uses: the components of the
//! topology, the configuration updates worker, and the installer share one runtime, as they share ADP's primary runtime
//! (the worker pool of the topology only runs compute-heavy subtasks of components, and the internal supervisor that
//! runs the installer is ambient). The test's collectors, which stand in for the encoders, run there too, and its
//! feeding runs on the test's own thread. The workload is the 1,024-metric DogStatsD-like batch of the plan crate's
//! `region_driver` bench (mapped counters, a blocked name, distributions whose `user_id` tag the tag filterlist
//! removes, gauges, and timestamped counters for the passthrough), with one timestamped sentinel per batch, which
//! reaches the main output through every region. Its mapper profile makes the pre-aggregation chain costly enough that
//! R1's cost model pipelines it over two regions. A second configuration, [`GATEWAYS`], adds two endpoint routing
//! groups and Multi-Region Failover mirroring an allowlist, whose gateways the legacy topology runs as components of
//! their own and the plan-driven one as `split`s of its policy region, one decision of the enrichment region. Four
//! phases are measured:
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
//! CPU is the runtime's worker busy time per metric, which counts the installer's lowering and preparations as part of
//! the cost of the plan-driven topology. Allocations are every byte and object the process allocated per metric, which
//! the test binary's tracking allocator counts; bytes are checked, and objects and the bytes the process holds at the
//! end of each phase are reported.
//!
//! The tolerances are the run-to-run noise of the legacy topology measured against itself, which
//! `the_legacy_topology_against_itself_stays_within_the_tolerances` checks (medians of five rounds, four workers,
//! Apple Silicon): throughput within 7%, CPU and allocated bytes within 7%, and p99 latency within 10%. The tests are
//! ignored: they measure, and their thresholds only hold in release mode on an otherwise idle machine. Run them with
//! `cargo nextest run -p agent-data-plane --release --run-ignored only -E 'test(/measurement_tests/)' --no-capture
//! --test-threads 1`: no two may run at once.

use std::time::Duration;

use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceStatsSnapshot};
use saluki_core::data_model::{
    event::metric::{context::Context, Metric, ScalarPoints},
    tags::{Tag, TagSet},
};
use serde_json::json;
use tokio::time::{interval, Instant, MissedTickBehavior};

use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        test_support::{connected_configuration_system, update},
        test_topology::{Aggregation, MetricsPath, Options, TestTopology},
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

fn tags(tags: &[String]) -> TagSet {
    tags.iter().map(|tag| Tag::from(tag.as_str())).collect()
}

/// Returns the `region_driver` bench's batch, without its last metric, which each batch replaces with its sentinel.
fn template() -> Vec<Metric> {
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

/// Returns a batch of timestamped counters only, over 256 contexts, without its sentinel: the native passthrough
/// batcher sends each such batch on as soon as it arrives, since it fills one buffer.
fn passthrough_template() -> Vec<Metric> {
    (0..BATCH - 1)
        .map(|i| timestamped(format!("app.jobs{}", i % 256)))
        .collect()
}

fn timestamped(name: String) -> Metric {
    Metric::counter(
        Context::from_parts(name, TagSet::default()),
        ScalarPoints::from_iter([(std::num::NonZeroU64::new(1_700_000_000), 2.0)]),
    )
}

/// Returns `count` batches from `first` on: copies of the template, which share its contexts as a source's resolved
/// contexts do, each with its sentinel. They are built before a phase starts, so that building them is not measured.
fn batches(template: &[Metric], first: usize, count: usize) -> Vec<Vec<Metric>> {
    (first..first + count)
        .map(|index| {
            let mut batch = template.to_vec();
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

/// Returns the total time the current runtime's workers have spent busy.
fn busy() -> Duration {
    let metrics = tokio::runtime::Handle::current().metrics();
    (0..metrics.num_workers())
        .map(|worker| metrics.worker_total_busy_duration(worker))
        .sum()
}

/// What one phase measured.
#[derive(Clone, Copy, Debug, Default)]
struct Measured {
    /// Metrics per second.
    throughput: f64,

    /// The runtime's worker busy time per metric, in nanoseconds.
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

/// Feeds `count` batches of `template`, starting at `first`, at most one per `period` (as fast as possible without
/// one), and returns what the phase measured once every sentinel has arrived at the main output.
async fn phase(
    topology: &mut TestTopology, template: &[Metric], first: usize, count: usize, period: Option<Duration>,
) -> Measured {
    let batches = batches(template, first, count);
    let mut fed = Vec::with_capacity(count);
    let started = Instant::now();
    let busy_before = busy();
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
                .filter(|_| delivery.output == "dd_metrics_encode")
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
            .filter(|_| delivery.output == "dd_metrics_encode")
        {
            latencies.push(delivery.at - fed[sentinel_batch(metric) - first]);
            arrived += 1;
        }
    }
    let elapsed = started.elapsed();
    let (bytes_after, allocations_after, live) = allocations();
    let metrics = (count * BATCH) as f64;
    latencies.sort();
    let micros = |at: f64| latencies[((latencies.len() - 1) as f64 * at) as usize].as_secs_f64() * 1e6;
    Measured {
        throughput: metrics / elapsed.as_secs_f64(),
        busy_per_metric: (busy() - busy_before).as_nanos() as f64 / metrics,
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

/// Runs one round of `path` with the configuration `config`, pacing at `paced_rate` batches per second, or at half the
/// measured saturated rate when `None`.
async fn round(path: MetricsPath, config: &str, paced_rate: Option<f64>) -> Round {
    let (system, updates, stream) = connected_configuration_system(config).await;
    let mut options = Options::new(path, Aggregation::Native);
    options.keep = |metric| metric.context().name() == SENTINEL;
    let mut topology = TestTopology::start(&system, options, |supervisor| {
        supervisor.add_worker(ConfigUpdatesWorker::new(updates))
    })
    .await;

    // Warm up: caches, allocations, and the first aggregation contexts.
    let template = template();
    phase(&mut topology, &template, 0, 200, None).await;
    let saturated = phase(&mut topology, &template, 200, 2_000, None).await;
    let rate = paced_rate.unwrap_or(saturated.throughput / BATCH as f64 / 2.0);
    let period = Duration::from_secs_f64(1.0 / rate);
    let paced_batches = (rate * 2.0) as usize;
    let mut first = 2_200;
    let paced = phase(&mut topology, &template, first, paced_batches, Some(period)).await;
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
    let updating = phase(&mut topology, &template, first, paced_batches, Some(period)).await;
    changes.abort();
    first += paced_batches;

    let passthrough = phase(
        &mut topology,
        &passthrough_template(),
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

const MEASURES: [(&str, Measure); 8] = [
    ("throughput (metrics/s)", |m| m.throughput),
    ("busy ns/metric", |m| m.busy_per_metric),
    ("allocated bytes/metric", |m| m.bytes_per_metric),
    ("allocations/metric", |m| m.allocations_per_metric),
    ("live MiB at the end", |m| m.live_mib),
    ("p50 latency (µs)", |m| m.p50),
    ("p99 latency (µs)", |m| m.p99),
    ("worst latency (µs)", |m| m.worst),
];

/// Measures `first` against `second` with the configuration `config` over [`ROUNDS`] rounds each, alternating them, on
/// a runtime of `workers` workers, and prints the medians as a table; returns the rounds of each.
async fn compare(first: MetricsPath, second: MetricsPath, workers: usize, config: &str) -> (Vec<Round>, Vec<Round>) {
    let mut firsts = Vec::new();
    let mut seconds = Vec::new();
    // Both are paced at the same rate: half the saturated rate of the first in its first round.
    let mut rate = None;
    for _ in 0..ROUNDS {
        let round_first = round(first, config, rate).await;
        let paced = *rate.get_or_insert(round_first[0].throughput / BATCH as f64 / 2.0);
        firsts.push(round_first);
        seconds.push(round(second, config, Some(paced)).await);
    }

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

/// Checks the stage gate, within the tolerances: saturated throughput no lower than legacy's, CPU and allocated bytes
/// per metric no higher in any phase, and p99 latency no higher in any paced phase. The number of allocations is
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

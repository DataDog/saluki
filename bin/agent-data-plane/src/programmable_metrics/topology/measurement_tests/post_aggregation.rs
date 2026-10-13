//! Throughput of the sources that enter the metrics path after aggregation, OTLP and the checks source, whose batches
//! the post-aggregation slot spreads over its lanes (plan section 8.5, increment 9.2).
//!
//! Both topologies run with OTLP and the checks source beside DogStatsD, on a runtime of `W` workers, with the real
//! encoders on the parent module's global pool. The test's thread feeds the scripted OTLP and checks sources batches of
//! the parent module's workload in turn, as fast as the topology takes them, each batch with its sentinel, and a phase
//! ends once every sentinel has reached the main encoder: throughput is the metrics fed per second until then, and CPU
//! is the process's CPU time per metric, less the feeding thread's. Nothing of DogStatsD runs, so every metric crosses
//! the post-aggregation slot only, on one lane without lanes and on `M` lanes with them, each buffer spread to the
//! first lane with room.
//!
//! The measurement is ignored: run it in release mode on an otherwise idle machine, one test at a time, for example
//! `SALUKI_MEASURE_WORKERS=1,2,4,8 cargo nextest run -p agent-data-plane --release --features measurement
//! --run-ignored only -E 'test(/post_aggregation::/)' --no-capture --test-threads 1`. Knobs:
//! `SALUKI_MEASURE_WORKERS` (comma-separated workers, `1,2,4,8` by default), `SALUKI_MEASURE_ROUNDS` (alternating
//! rounds per path, 3 by default), `SALUKI_MEASURE_POST_LANES` (the post-aggregation lanes of the path on lanes,
//! `min(W, 8)` by default at each W), and `SALUKI_MEASURE_BATCHES` (batches per phase, 4,000 by default).

use std::time::Duration;

use saluki_core::data_model::event::metric::Metric;
use saluki_metric_plan::compile::LanePolicy;
use tokio::time::Instant;

use super::{
    allocations, batches, global_pool, process_cpu, sentinel_batch, templates, thread_cpu, BATCH, CONFIG, SENTINEL,
};
use crate::programmable_metrics::{
    lanes::LaneCounts,
    test_support::configuration_system,
    test_topology::{Aggregation, Delivery, Encoders, MetricsPath, Options, TestTopology},
};

/// OTLP and the checks source beside DogStatsD, with the parent module's configuration.
const OTLP_AND_CHECKS: &str = "data_plane:\n  otlp:\n    enabled: true\n  checks:\n    enabled: true\n";

/// The feeds of the sources that enter the metrics path after aggregation, which batches take in turn.
const FEEDS: [&str; 2] = ["otlp_in.metrics", "checks_ipc_in.metrics"];

/// One of the paths a round measures.
#[derive(Clone, Copy, Debug)]
enum Path {
    /// The legacy topology.
    Legacy,

    /// The plan-driven topology without lanes: one post-aggregation lane, R1's layout.
    Plan,

    /// The plan-driven topology on one producer lane, one state lane, and this many post-aggregation lanes.
    PostLanes(u16),
}

impl Path {
    fn metrics_path(self) -> MetricsPath {
        match self {
            Self::Legacy => MetricsPath::Legacy,
            Self::Plan | Self::PostLanes(_) => MetricsPath::Plan,
        }
    }

    fn lanes(self) -> Option<LaneCounts> {
        match self {
            Self::Legacy | Self::Plan => None,
            Self::PostLanes(post) => Some(LaneCounts::on(LanePolicy {
                dogstatsd: 1,
                state: 1,
                post,
            })),
        }
    }
}

/// What one phase measured.
#[derive(Clone, Copy, Debug, Default)]
struct Measured {
    /// Metrics per second, fed until the last sentinel arrived.
    throughput: f64,

    /// The process's CPU time per metric, less the feeding thread's, in nanoseconds.
    cpu_per_metric: f64,

    /// Bytes allocated per metric, by the whole process.
    bytes_per_metric: f64,
}

/// Reads a number knob from the environment.
fn knob(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse().unwrap_or_else(|_| panic!("{} is a number", name)))
}

/// Runs one saturated phase of `path` on a runtime of `workers` workers: `count` batches fed into the sources in turn.
fn measure(path: Path, workers: usize, count: usize) -> Measured {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("primary")
        .enable_all()
        .build()
        .expect("the primary runtime builds");
    let pool = global_pool();
    let config = format!("{}{}", CONFIG, OTLP_AND_CHECKS);
    let measured = runtime.block_on(async {
        let system = configuration_system(&config).await;
        let mut options = Options::new(path.metrics_path(), Aggregation::Native);
        options.keep = |metric| metric.context().name() == SENTINEL;
        options.encoders = Encoders::Real;
        options.worker_pool = Some(pool.handle().clone());
        options.lanes = path.lanes();
        let mut topology = TestTopology::start(&system, options, |_| {}).await;
        for feed in FEEDS {
            assert!(topology.has_feed(feed), "the topology runs the source of `{}`", feed);
        }
        let templates = templates(BATCH);
        // Warm up: caches, allocations, and every lane's first buffers.
        run(&mut topology, &templates, 0, 200).await;
        let (cpu_before, feeding_before) = (process_cpu(), thread_cpu());
        let (bytes_before, _, _) = allocations();
        let started = Instant::now();
        run(&mut topology, &templates, 200, count).await;
        let elapsed = started.elapsed();
        let cpu = process_cpu().saturating_sub(cpu_before) - thread_cpu().saturating_sub(feeding_before);
        let (bytes_after, _, _) = allocations();
        drop(topology.stop().await);
        let metrics = (count * BATCH) as f64;
        Measured {
            throughput: metrics / elapsed.as_secs_f64(),
            cpu_per_metric: cpu.as_nanos() as f64 / metrics,
            bytes_per_metric: bytes_after.saturating_sub(bytes_before) as f64 / metrics,
        }
    });
    pool.shutdown_background();
    runtime.shutdown_background();
    measured
}

/// Feeds `count` batches from `first` on into the sources in turn, as fast as the topology takes them, and waits until
/// every sentinel has reached the main encoder.
async fn run(topology: &mut TestTopology, templates: &[Vec<Metric>], first: usize, count: usize) {
    let mut arrived = 0;
    for (index, batch) in batches(templates, first, count).into_iter().enumerate() {
        topology.feed(FEEDS[index % FEEDS.len()], batch);
        // Take what has arrived meanwhile, so that the collectors' channel stays short.
        while let Some(Some(delivery)) = futures::FutureExt::now_or_never(topology.next_delivery()) {
            arrived += sentinels(&delivery, first);
        }
    }
    while arrived < count {
        let delivery = tokio::time::timeout(Duration::from_secs(120), topology.next_delivery())
            .await
            .expect("every sentinel arrives")
            .expect("the outputs stay open");
        arrived += sentinels(&delivery, first);
    }
}

/// Returns how many sentinels of batches from `first` on `delivery` carries to the main encoder.
fn sentinels(delivery: &Delivery, first: usize) -> usize {
    delivery
        .metrics
        .iter()
        .filter(|metric| delivery.output == "dd_metrics_encode" && metric.context().name() == SENTINEL)
        .filter(|metric| sentinel_batch(metric) >= first)
        .count()
}

/// Returns the median of `value` over `rounds`.
fn median(rounds: &[Measured], value: impl Fn(&Measured) -> f64) -> f64 {
    let mut values = rounds.iter().map(value).collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[test]
#[ignore = "a measurement: run it in release mode, as the module documentation says"]
fn otlp_and_checks_throughput_on_post_aggregation_lanes() {
    let workers = std::env::var("SALUKI_MEASURE_WORKERS")
        .ok()
        .filter(|list| !list.is_empty())
        .map(|list| {
            list.split(',')
                .map(|workers| workers.trim().parse().expect("SALUKI_MEASURE_WORKERS lists numbers"))
                .collect::<Vec<usize>>()
        })
        .unwrap_or_else(|| vec![1, 2, 4, 8]);
    let rounds = knob("SALUKI_MEASURE_ROUNDS").unwrap_or(3);
    let count = knob("SALUKI_MEASURE_BATCHES").unwrap_or(4_000);
    println!(
        "OTLP and checks, saturated, {} batches of {} metrics per phase, {} alternating rounds:",
        count, BATCH, rounds
    );
    println!(
        "| W | path | throughput (metrics/s) | ratio to legacy | process CPU ns/metric | allocated bytes/metric |"
    );
    println!("|---|---|---|---|---|---|");
    for &w in &workers {
        let post = knob("SALUKI_MEASURE_POST_LANES").map_or(w.clamp(1, 8) as u16, |post| post as u16);
        let paths = [Path::Legacy, Path::Plan, Path::PostLanes(post)];
        let mut measured = vec![Vec::new(); paths.len()];
        for _ in 0..rounds {
            for (index, path) in paths.iter().enumerate() {
                measured[index].push(measure(*path, w, count));
            }
        }
        let legacy = median(&measured[0], |m| m.throughput);
        for (path, rounds) in paths.iter().zip(&measured) {
            let throughput = median(rounds, |m| m.throughput);
            println!(
                "| {} | {:?} | {:.0} | {:.3} | {:.0} | {:.1} |",
                w,
                path,
                throughput,
                throughput / legacy,
                median(rounds, |m| m.cpu_per_metric),
                median(rounds, |m| m.bytes_per_metric)
            );
        }
    }
}

#[test]
fn the_post_aggregation_measurement_runs_every_path_end_to_end() {
    // The harness's wiring, quickly: every path's topology runs both sources, and their sentinels arrive.
    for path in [Path::Legacy, Path::Plan, Path::PostLanes(2)] {
        let measured = measure(path, 2, 4);
        assert!(measured.throughput > 0.0, "the {:?} path's sentinels arrive", path);
    }
}

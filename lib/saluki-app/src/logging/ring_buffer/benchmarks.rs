use rand::{rngs::SmallRng, Rng, SeedableRng};
use tracing::Metadata;

use super::codec::write_length_prefixed;
use super::event::CondensedEvent;
use super::processor::ProcessorState;
use super::RingBufferConfig;

const BENCH_TARGETS: &[&str] = &[
    "saluki_components::sources::dogstatsd",
    "saluki_core::topology::runner",
    "saluki_io::net::udp",
    "saluki_app::logging::ring_buffer",
    "saluki_components::transforms::aggregate",
    "saluki_metadata::origin::resolver",
    "saluki_io::compression",
    "saluki_components::destinations::datadog",
    "saluki_core::buffers::memory",
    "agent_data_plane::cli::run",
    "saluki_components::sources::otlp",
    "saluki_tls::provider",
    "saluki_core::topology::interconnect",
    "saluki_app::internal::remote_agent",
    "saluki_metrics::recorder",
];

const BENCH_SHORT_MESSAGES: &[&str] = &[
    "Listener started.",
    "Connection closed.",
    "Shutting down.",
    "Health check passed.",
    "Config reloaded.",
    "Worker started.",
    "Flushing buffers.",
    "Checkpoint saved.",
    "Reconnecting.",
    "Pipeline initialized.",
];

const BENCH_FIELD_KEYS: &[&str] = &[
    "error",
    "listen_addr",
    "count",
    "path",
    "duration_ms",
    "component",
    "peer_addr",
    "bytes",
];

const BENCH_FIELD_VALUES: &[&str] = &[
    "127.0.0.1:8125",
    "/var/run/datadog/apm.socket",
    "connection reset by peer",
    "dogstatsd",
    "topology_runner",
    "Permission denied (os error 13)",
    "https://intake.datadoghq.com/api/v2/series",
    "sess_a1b2c3d4e5f6",
];

const BENCH_FILES: &[&str] = &[
    "src/sources/dogstatsd/mod.rs",
    "src/topology/runner.rs",
    "src/net/udp.rs",
    "src/logging/ring_buffer.rs",
    "src/transforms/aggregate.rs",
    "src/destinations/datadog.rs",
    "src/cli/run.rs",
    "src/internal/remote_agent.rs",
];

const BENCH_LEVELS: &[tracing::Level] = &[
    tracing::Level::DEBUG,
    tracing::Level::INFO,
    tracing::Level::WARN,
    tracing::Level::ERROR,
];

/// Builds a leaked `&'static Metadata<'static>` for benchmark use with the given target and
/// level. File/line are set to a representative value; in production these come from the
/// callsite and are not varied per-event.
fn bench_metadata(target: &'static str, level: tracing::Level, file: &'static str) -> &'static Metadata<'static> {
    // Intentionally leak: benchmark metadata lives for the process lifetime.
    &*Box::leak(Box::new(tracing::Metadata::new(
        "bench",
        target,
        level,
        Some(file),
        Some(1),
        Some(target),
        tracing::field::FieldSet::new(
            &[],
            tracing::callsite::Identifier(
                // Each leaked Metadata gets its own unique leaked callsite so that
                // Identifier uniqueness is satisfied.
                &*Box::leak(Box::new(BenchCallsite)),
            ),
        ),
        tracing::metadata::Kind::EVENT,
    )))
}

struct BenchCallsite;
impl tracing::callsite::Callsite for BenchCallsite {
    fn set_interest(&self, _: tracing::subscriber::Interest) {}
    fn metadata(&self) -> &Metadata<'_> {
        unimplemented!("bench callsite metadata should never be called")
    }
}

struct EventGenerator {
    rng: SmallRng,
    timestamp_nanos: u128,
    metadata_table: Vec<&'static Metadata<'static>>,
}

impl EventGenerator {
    fn new(seed: u64) -> Self {
        // Pre-build a metadata entry for each (target, level) combination.
        let mut metadata_table = Vec::new();
        for &target in BENCH_TARGETS {
            let file = BENCH_FILES[metadata_table.len() % BENCH_FILES.len()];
            for &level in BENCH_LEVELS {
                metadata_table.push(bench_metadata(target, level, file));
            }
        }
        Self {
            rng: SmallRng::seed_from_u64(seed),
            timestamp_nanos: 1_700_000_000_000_000_000, // ~Nov 2023
            metadata_table,
        }
    }

    fn next_event(&mut self) -> CondensedEvent {
        // Advance timestamp by 1-50ms.
        self.timestamp_nanos += self.rng.random_range(1_000_000u128..50_000_000u128);

        // Weighted level distribution: 40% DEBUG, 30% INFO, 20% WARN, 10% ERROR.
        let level_roll: u32 = self.rng.random_range(0..100);
        let level_idx = if level_roll < 40 {
            0 // DEBUG
        } else if level_roll < 70 {
            1 // INFO
        } else if level_roll < 90 {
            2 // WARN
        } else {
            3 // ERROR
        };

        let target_idx = self.rng.random_range(0..BENCH_TARGETS.len());
        let metadata = self.metadata_table[target_idx * BENCH_LEVELS.len() + level_idx];

        // 60% short static messages, 40% formatted with numbers.
        let message = if self.rng.random_range(0..100u32) < 60 {
            BENCH_SHORT_MESSAGES[self.rng.random_range(0..BENCH_SHORT_MESSAGES.len())].to_string()
        } else {
            let variant: u32 = self.rng.random_range(0..4);
            match variant {
                0 => format!(
                    "Processed {} events in {}ms",
                    self.rng.random_range(1u64..100_000),
                    self.rng.random_range(1u32..5000)
                ),
                1 => format!(
                    "Buffer capacity at {}%, {} bytes used of {}",
                    self.rng.random_range(10u32..100),
                    self.rng.random_range(1024u64..1_048_576),
                    self.rng.random_range(1_048_576u64..10_485_760)
                ),
                2 => format!(
                    "Forwarding {} metric(s) to {}",
                    self.rng.random_range(1u32..10_000),
                    BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())]
                ),
                _ => format!(
                    "Retry attempt {} of {} for endpoint {}",
                    self.rng.random_range(1u32..5),
                    self.rng.random_range(3u32..10),
                    BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())]
                ),
            }
        };

        // 0-5 fields, weighted toward fewer: 30% 0, 30% 1, 20% 2, 10% 3, 5% 4, 5% 5.
        let field_roll: u32 = self.rng.random_range(0..100);
        let num_fields = if field_roll < 30 {
            0
        } else if field_roll < 60 {
            1
        } else if field_roll < 80 {
            2
        } else if field_roll < 90 {
            3
        } else if field_roll < 95 {
            4
        } else {
            5
        };

        let mut fields = Vec::new();
        for _ in 0..num_fields {
            let key = BENCH_FIELD_KEYS[self.rng.random_range(0..BENCH_FIELD_KEYS.len())];
            // 50% use a static value, 50% use a formatted number.
            let value: String = if self.rng.random_range(0..2u32) == 0 {
                BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())].to_string()
            } else {
                format!("{}", self.rng.random_range(0u64..1_000_000))
            };
            write_length_prefixed(&mut fields, key.as_bytes());
            write_length_prefixed(&mut fields, value.as_bytes());
        }

        CondensedEvent {
            timestamp_nanos: self.timestamp_nanos,
            metadata: Some(metadata),
            message,
            fields,
        }
    }
}

struct SegmentStats {
    event_count: usize,
    compressed_bytes: usize,
    uncompressed_bytes: usize,
}

struct BenchmarkResult {
    config_desc: String,
    seed: u64,
    events_fed: usize,
    events_retained: usize,
    segments_live: usize,
    segments_dropped: u64,
    total_compressed_bytes: usize,
    event_buffer_bytes: usize,
    total_size_bytes: usize,
    per_segment: Vec<SegmentStats>,
}

fn run_benchmark(config: RingBufferConfig, config_desc: &str, seed: u64, num_events: usize) -> BenchmarkResult {
    let mut state = ProcessorState::new(config);
    let mut gen = EventGenerator::new(seed);

    for _ in 0..num_events {
        let event = gen.next_event();
        state.add_event(&event).unwrap();
    }

    let per_segment: Vec<SegmentStats> = state
        .compressed_segments
        .iter_segments()
        .map(|seg| SegmentStats {
            event_count: seg.event_count(),
            compressed_bytes: seg.size_bytes(),
            uncompressed_bytes: seg.uncompressed_size_bytes(),
        })
        .collect();

    let events_retained = state.compressed_segments.event_count() + state.event_buffer.event_count();

    BenchmarkResult {
        config_desc: config_desc.to_string(),
        seed,
        events_fed: num_events,
        events_retained,
        segments_live: state.compressed_segments.segment_count(),
        segments_dropped: state.compressed_segments.segments_dropped_total(),
        total_compressed_bytes: state.compressed_segments.size_bytes(),
        event_buffer_bytes: state.event_buffer.size_bytes(),
        total_size_bytes: state.total_size_bytes(),
        per_segment,
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.2} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn print_report(result: &BenchmarkResult) {
    let total_uncompressed: usize = result.per_segment.iter().map(|s| s.uncompressed_bytes).sum();
    let total_compressed: usize = result.per_segment.iter().map(|s| s.compressed_bytes).sum();
    let overall_ratio = if total_compressed > 0 {
        total_uncompressed as f64 / total_compressed as f64
    } else {
        0.0
    };
    let avg_bytes_per_event = if result.events_retained > 0 {
        result.total_compressed_bytes as f64 / result.events_retained as f64
    } else {
        0.0
    };
    let retention_pct = if result.events_fed > 0 {
        100.0 * result.events_retained as f64 / result.events_fed as f64
    } else {
        0.0
    };

    println!();
    println!("=== Ring Buffer Capacity Benchmark ===");
    println!("Config: {}", result.config_desc);
    println!("Seed: 0x{:X} | Events fed: {}", result.seed, result.events_fed);
    println!();
    println!("--- Summary ---");
    println!("Events retained:          {}", result.events_retained);
    println!(
        "Events evicted:           {}",
        result.events_fed - result.events_retained
    );
    println!("Retention rate:           {:.1}%", retention_pct);
    println!("Segments live:            {}", result.segments_live);
    println!("Segments dropped:         {}", result.segments_dropped);
    println!(
        "Compressed bytes:         {} ({})",
        result.total_compressed_bytes,
        format_bytes(result.total_compressed_bytes)
    );
    println!(
        "Event buffer bytes:       {} ({})",
        result.event_buffer_bytes,
        format_bytes(result.event_buffer_bytes)
    );
    println!(
        "Total size:               {} ({})",
        result.total_size_bytes,
        format_bytes(result.total_size_bytes)
    );
    println!("Avg compressed bytes/evt: {:.1}", avg_bytes_per_event);
    println!("Overall compression ratio:{:.2}x", overall_ratio);

    if !result.per_segment.is_empty() {
        println!();
        println!("--- Per-Segment Stats ---");
        println!(
            "  {:>3} | {:>6} | {:>10} | {:>10} | {:>5}",
            "#", "Events", "Uncompr.", "Compr.", "Ratio"
        );
        for (i, seg) in result.per_segment.iter().enumerate() {
            let ratio = if seg.compressed_bytes > 0 {
                seg.uncompressed_bytes as f64 / seg.compressed_bytes as f64
            } else {
                0.0
            };
            println!(
                "  {:>3} | {:>6} | {:>10} | {:>10} | {:>4.2}x",
                i + 1,
                seg.event_count,
                seg.uncompressed_bytes,
                seg.compressed_bytes,
                ratio
            );
        }
    }
    println!();
}

// --- Stability benchmark ---
//
// Measures how stable the retained event count is over time. The baseline capacity benchmark only
// reports the final retained count; this benchmark samples retained count after every event (once
// past a warmup phase) so we can characterize the min / avg / stddev / percentile distribution.
//
// Why this matters: ring buffer retention oscillates. Every time a segment is evicted, retained
// count drops sharply by that segment's event count (typically thousands of events). Useful
// coverage depends on the *minimum* retained count, not peak. Two configs could have the same
// peak retention but very different min -- the one with smaller drop amplitudes is more useful.

struct PhaseStats {
    warmup_events: usize,
    samples_count: usize,
    drops_observed: u64,

    // Retained event count statistics.
    min_retained: usize,
    max_retained: usize,
    avg_retained: f64,
    stddev_retained: f64,
    p1_retained: usize,
    p10_retained: usize,
    p50_retained: usize,
    p90_retained: usize,
    p99_retained: usize,

    // Coverage duration statistics (simulated nanoseconds).
    min_coverage_ns: u128,
    avg_coverage_ns: f64,
    p1_coverage_ns: u128,
    p50_coverage_ns: u128,

    // Drop amplitude characterization.
    max_drop_amplitude: usize,
    avg_drop_amplitude: f64,
    p99_drop_amplitude: usize,
}

struct StabilityResult {
    config_desc: String,
    seed: u64,
    events_fed: usize,
    early: PhaseStats,
    steady: PhaseStats,
}

/// "Early" phase: after first segment drop (buffer has been filled once) but within the first
/// cycle. This captures the transient "cleanup" period where we evict any monster segments
/// created during startup.
const STABILITY_EARLY_WARMUP_DROPS: u64 = 1;
/// "Steady state" phase: after many drops, the startup-era segments have been fully evicted and
/// behavior should be fully stable.
const STABILITY_STEADY_WARMUP_DROPS: u64 = 50;

fn run_stability_benchmark(
    config: RingBufferConfig, config_desc: &str, seed: u64, num_events: usize,
) -> StabilityResult {
    let mut state = ProcessorState::new(config);
    let mut gen = EventGenerator::new(seed);

    // Collect samples at all phases.
    let mut early_retained: Vec<u32> = Vec::with_capacity(num_events);
    let mut early_coverage: Vec<u64> = Vec::with_capacity(num_events);
    let mut early_drops: Vec<u32> = Vec::new();
    let mut early_warmup_events: usize = 0;
    let mut early_drops_at_start: u64 = 0;
    let mut early_warmed_up = false;
    let mut early_prev_retained: usize = 0;

    let mut steady_retained: Vec<u32> = Vec::with_capacity(num_events);
    let mut steady_coverage: Vec<u64> = Vec::with_capacity(num_events);
    let mut steady_drops: Vec<u32> = Vec::new();
    let mut steady_warmup_events: usize = 0;
    let mut steady_drops_at_start: u64 = 0;
    let mut steady_warmed_up = false;
    let mut steady_prev_retained: usize = 0;

    for i in 0..num_events {
        let event = gen.next_event();
        state.add_event(&event).unwrap();

        let drops_total = state.compressed_segments.segments_dropped_total();
        if !early_warmed_up && drops_total >= STABILITY_EARLY_WARMUP_DROPS {
            early_warmed_up = true;
            early_warmup_events = i + 1;
            early_drops_at_start = drops_total;
            early_prev_retained = state.compressed_segments.event_count() + state.event_buffer.event_count();
        }
        if !steady_warmed_up && drops_total >= STABILITY_STEADY_WARMUP_DROPS {
            steady_warmed_up = true;
            steady_warmup_events = i + 1;
            steady_drops_at_start = drops_total;
            steady_prev_retained = state.compressed_segments.event_count() + state.event_buffer.event_count();
        }

        let retained = state.compressed_segments.event_count() + state.event_buffer.event_count();
        let oldest = if state.compressed_segments.segment_count() > 0 {
            state.compressed_segments.oldest_timestamp_nanos()
        } else {
            state.event_buffer.oldest_timestamp_nanos()
        };
        let coverage = event.timestamp_nanos.saturating_sub(oldest) as u64;

        if early_warmed_up {
            early_retained.push(retained as u32);
            early_coverage.push(coverage);
            if retained < early_prev_retained {
                early_drops.push((early_prev_retained - retained) as u32);
            }
            early_prev_retained = retained;
        }
        if steady_warmed_up {
            steady_retained.push(retained as u32);
            steady_coverage.push(coverage);
            if retained < steady_prev_retained {
                steady_drops.push((steady_prev_retained - retained) as u32);
            }
            steady_prev_retained = retained;
        }
    }

    let early_drops_observed = state.compressed_segments.segments_dropped_total() - early_drops_at_start;
    let steady_drops_observed = state.compressed_segments.segments_dropped_total() - steady_drops_at_start;

    let build_phase_stats = |retained: &[u32],
                             coverage: &[u64],
                             drops: &[u32],
                             warmup_events: usize,
                             drops_observed: u64|
     -> PhaseStats {
        let (min_r, max_r, avg_r, stddev_r, p1_r, p10_r, p50_r, p90_r, p99_r) = stats_u32(retained);
        let (min_c, avg_c, p1_c, p50_c) = {
            if coverage.is_empty() {
                (0u128, 0.0, 0u128, 0u128)
            } else {
                let min = *coverage.iter().min().unwrap() as u128;
                let sum: u128 = coverage.iter().map(|&x| x as u128).sum();
                let avg = sum as f64 / coverage.len() as f64;
                let mut sorted = coverage.to_vec();
                sorted.sort_unstable();
                let p = |pct: usize| -> u128 { sorted.get(sorted.len() * pct / 100).copied().unwrap_or(0) as u128 };
                (min, avg, p(1), p(50))
            }
        };
        let (max_d, avg_d, p99_d) = {
            if drops.is_empty() {
                (0u32, 0.0, 0u32)
            } else {
                let max = *drops.iter().max().unwrap();
                let sum: u64 = drops.iter().map(|&x| x as u64).sum();
                let avg = sum as f64 / drops.len() as f64;
                let mut sorted = drops.to_vec();
                sorted.sort_unstable();
                let p99 = sorted[(sorted.len() * 99) / 100];
                (max, avg, p99)
            }
        };
        PhaseStats {
            warmup_events,
            samples_count: retained.len(),
            drops_observed,
            min_retained: min_r as usize,
            max_retained: max_r as usize,
            avg_retained: avg_r,
            stddev_retained: stddev_r,
            p1_retained: p1_r as usize,
            p10_retained: p10_r as usize,
            p50_retained: p50_r as usize,
            p90_retained: p90_r as usize,
            p99_retained: p99_r as usize,
            min_coverage_ns: min_c,
            avg_coverage_ns: avg_c,
            p1_coverage_ns: p1_c,
            p50_coverage_ns: p50_c,
            max_drop_amplitude: max_d as usize,
            avg_drop_amplitude: avg_d,
            p99_drop_amplitude: p99_d as usize,
        }
    };
    let early = build_phase_stats(
        &early_retained,
        &early_coverage,
        &early_drops,
        early_warmup_events,
        early_drops_observed,
    );
    let steady = build_phase_stats(
        &steady_retained,
        &steady_coverage,
        &steady_drops,
        steady_warmup_events,
        steady_drops_observed,
    );

    StabilityResult {
        config_desc: config_desc.to_string(),
        seed,
        events_fed: num_events,
        early,
        steady,
    }
}

fn stats_u32(values: &[u32]) -> (u32, u32, f64, f64, u32, u32, u32, u32, u32) {
    let min = *values.iter().min().unwrap_or(&0);
    let max = *values.iter().max().unwrap_or(&0);
    let sum: u64 = values.iter().map(|&x| x as u64).sum();
    let avg = if values.is_empty() {
        0.0
    } else {
        sum as f64 / values.len() as f64
    };
    let variance: f64 = values
        .iter()
        .map(|&x| {
            let d = x as f64 - avg;
            d * d
        })
        .sum::<f64>()
        / values.len().max(1) as f64;
    let stddev = variance.sqrt();
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p = |pct: usize| -> u32 { sorted.get(sorted.len() * pct / 100).copied().unwrap_or(0) };
    (min, max, avg, stddev, p(1), p(10), p(50), p(90), p(99))
}

fn format_ns_as_secs(ns: u128) -> String {
    let secs = ns as f64 / 1_000_000_000.0;
    if secs >= 60.0 {
        format!("{:.1}m", secs / 60.0)
    } else {
        format!("{:.1}s", secs)
    }
}

fn print_phase(label: &str, p: &PhaseStats) {
    let delta = p.max_retained.saturating_sub(p.min_retained);
    let delta_pct = if p.avg_retained > 0.0 {
        100.0 * delta as f64 / p.avg_retained
    } else {
        0.0
    };
    let min_pct = if p.avg_retained > 0.0 {
        100.0 * p.min_retained as f64 / p.avg_retained
    } else {
        0.0
    };
    println!(
        "--- {} (from event {}, drops {}, samples {}) ---",
        label, p.warmup_events, p.drops_observed, p.samples_count
    );
    println!(
        "  retained: min={} p1={} p10={} p50={} avg={:.0} p90={} p99={} max={}",
        p.min_retained,
        p.p1_retained,
        p.p10_retained,
        p.p50_retained,
        p.avg_retained,
        p.p90_retained,
        p.p99_retained,
        p.max_retained
    );
    println!(
        "  stddev={:.0}  max-min={} ({:.1}% of avg)  min/avg={:.1}%",
        p.stddev_retained, delta, delta_pct, min_pct
    );
    println!(
        "  drop amplitude: max={} p99={} avg={:.0}",
        p.max_drop_amplitude, p.p99_drop_amplitude, p.avg_drop_amplitude
    );
    println!(
        "  coverage: min={} p1={} p50={} avg={}",
        format_ns_as_secs(p.min_coverage_ns),
        format_ns_as_secs(p.p1_coverage_ns),
        format_ns_as_secs(p.p50_coverage_ns),
        format_ns_as_secs(p.avg_coverage_ns as u128)
    );
}

fn print_stability_report(result: &StabilityResult) {
    println!();
    println!("=== Ring Buffer Stability Benchmark ===");
    println!("Config: {}", result.config_desc);
    println!("Seed: 0x{:X} | Events fed: {}", result.seed, result.events_fed);
    println!();
    print_phase("Early (after 1st drop)", &result.early);
    println!();
    print_phase("Steady (after 50th drop)", &result.steady);
    println!();
}

#[test]
#[ignore]
fn ring_buffer_stability_bench() {
    let seed = 0xDEAD_BEEF_CAFE;
    // 1.5M events -- at ~25ms per event, that's ~10 hours simulated. Produces ~300 drops at
    // steady state, which is plenty for percentile stability.
    let num_events = 1_500_000;

    let config = RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

    let result = run_stability_benchmark(config, "default: max=2MiB, min_segment=128KiB, zstd=19", seed, num_events);
    print_stability_report(&result);
}

#[test]
#[ignore]
fn ring_buffer_stability_sweep() {
    let seed = 0xDEAD_BEEF_CAFE;
    let num_events = 750_000;

    let configs: Vec<(&str, RingBufferConfig)> = vec![
        (
            "default: 128k, zstd=19",
            RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024),
        ),
        (
            "64k segments",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_min_uncompressed_segment_size_bytes(64 * 1024),
        ),
        (
            "32k segments",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_min_uncompressed_segment_size_bytes(32 * 1024),
        ),
        (
            "256k segments",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_min_uncompressed_segment_size_bytes(256 * 1024),
        ),
        (
            "128k, zstd=11",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_compression_level(11),
        ),
        (
            "128k, zstd=22",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_compression_level(22),
        ),
    ];

    let mut results = Vec::new();
    for (name, config) in configs {
        let result = run_stability_benchmark(config, name, seed, num_events);
        print_stability_report(&result);
        results.push(result);
    }

    println!("=== Early-phase Comparison (from first drop) ===");
    println!(
        "  {:<45} | {:>8} | {:>8} | {:>8} | {:>8} | {:>9} | {:>8}",
        "Config", "min", "p1", "p50", "avg", "max-drop", "min/avg"
    );
    for r in &results {
        let min_pct = if r.early.avg_retained > 0.0 {
            100.0 * r.early.min_retained as f64 / r.early.avg_retained
        } else {
            0.0
        };
        println!(
            "  {:<45} | {:>8} | {:>8} | {:>8} | {:>8.0} | {:>9} | {:>7.1}%",
            r.config_desc,
            r.early.min_retained,
            r.early.p1_retained,
            r.early.p50_retained,
            r.early.avg_retained,
            r.early.max_drop_amplitude,
            min_pct,
        );
    }
    println!();
    println!("=== Steady-state Comparison (after 50 drops) ===");
    println!(
        "  {:<45} | {:>8} | {:>8} | {:>8} | {:>8} | {:>9} | {:>8}",
        "Config", "min", "p1", "p50", "avg", "max-drop", "min/avg"
    );
    for r in &results {
        let min_pct = if r.steady.avg_retained > 0.0 {
            100.0 * r.steady.min_retained as f64 / r.steady.avg_retained
        } else {
            0.0
        };
        println!(
            "  {:<45} | {:>8} | {:>8} | {:>8} | {:>8.0} | {:>9} | {:>7.1}%",
            r.config_desc,
            r.steady.min_retained,
            r.steady.p1_retained,
            r.steady.p50_retained,
            r.steady.avg_retained,
            r.steady.max_drop_amplitude,
            min_pct,
        );
    }
    println!();
}

#[test]
#[ignore]
fn ring_buffer_capacity_bench() {
    let seed = 0xDEAD_BEEF_CAFE;
    let num_events = 500_000;

    let config = RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

    let result = run_benchmark(config, "max=2MiB, min_segment=128KiB, zstd_level=19", seed, num_events);
    print_report(&result);
}

#[test]
#[ignore]
fn ring_buffer_capacity_sweep() {
    let seed = 0xDEAD_BEEF_CAFE;
    let num_events = 500_000;

    let configs: Vec<(&str, RingBufferConfig)> = vec![
        (
            "default: max=2MiB, min_segment=128KiB, zstd=19",
            RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024),
        ),
        (
            "zstd=3: max=2MiB, min_segment=128KiB, zstd=3",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_compression_level(3),
        ),
        (
            "zstd=11: max=2MiB, min_segment=128KiB, zstd=11",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_compression_level(11),
        ),
        (
            "256k seg: max=2MiB, min_segment=256KiB, zstd=19",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_min_uncompressed_segment_size_bytes(256 * 1024),
        ),
        (
            "64k seg: max=2MiB, min_segment=64KiB, zstd=19",
            RingBufferConfig::default()
                .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                .with_min_uncompressed_segment_size_bytes(64 * 1024),
        ),
    ];

    let mut results = Vec::new();
    for (name, config) in configs {
        let result = run_benchmark(config, name, seed, num_events);
        print_report(&result);
        results.push(result);
    }

    // Print compact comparison table.
    println!("=== Comparison ===");
    println!(
        "  {:<50} | {:>8} | {:>6} | {:>7} | {:>6}",
        "Config", "Retained", "Rate", "Ratio", "B/evt"
    );
    for r in &results {
        let total_uncompr: usize = r.per_segment.iter().map(|s| s.uncompressed_bytes).sum();
        let total_compr: usize = r.per_segment.iter().map(|s| s.compressed_bytes).sum();
        let ratio = if total_compr > 0 {
            total_uncompr as f64 / total_compr as f64
        } else {
            0.0
        };
        let bytes_per_evt = if r.events_retained > 0 {
            r.total_compressed_bytes as f64 / r.events_retained as f64
        } else {
            0.0
        };
        let rate = 100.0 * r.events_retained as f64 / r.events_fed as f64;
        println!(
            "  {:<50} | {:>8} | {:>5.1}% | {:>5.2}x | {:>6.1}",
            r.config_desc, r.events_retained, rate, ratio, bytes_per_evt
        );
    }
    println!();
}

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

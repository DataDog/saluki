use rand::RngExt as _;
use rand::{rngs::SmallRng, SeedableRng};
use tracing::Metadata;

use super::codec::write_length_prefixed;
use super::event::CondensedEvent;
use super::event_buffer::EventBuffer;
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

/// Log-generation locality mode.
///
/// - `Iid`: every event picks its target, level, file, and message independently -- there is no
///   temporal locality. This is the original generator and remains the reference for all previously
///   recorded numbers.
/// - `Bursty`: models how real components actually log. A fixed set of callsites is drawn up front
///   (each with a stable target, level, file, message template, and field shape); events are then
///   emitted in contiguous *bursts* from a single callsite -- with sub-millisecond inter-event
///   spacing and a stable level -- before switching to another callsite. This exercises the RLE
///   columns (callsite/template/field-count indices), the millisecond timestamp quantization, and
///   verbatim field-value repetition the way production traffic does. See the "benchmark fidelity"
///   trial in `optimization_field_notes.md` for why the i.i.d. generator understates these wins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GenMode {
    Iid,
    Bursty,
}

/// Number of distinct callsites modeled in `Bursty` mode.
const BENCH_BURSTY_CALLSITES: usize = 48;
/// Burst length range: consecutive events emitted from one callsite before switching. The upper
/// bound is exclusive, so the mean burst is ~21 events.
const BENCH_BURST_MIN: usize = 4;
const BENCH_BURST_MAX: usize = 40;
/// Within-burst inter-event spacing (nanoseconds). Sub-millisecond (mean ~0.15 ms), so a burst's
/// events span only a handful of adjacent millisecond buckets and their quantized timestamp deltas
/// are mostly 0 (with the occasional 1) -- i.e. long RLE-friendly runs, matching a real tight
/// logging burst.
const BENCH_BURST_DELTA_MIN_NS: u128 = 5_000;
const BENCH_BURST_DELTA_MAX_NS: u128 = 300_000;
/// Between-burst gap (nanoseconds): the quiet interval before another callsite logs.
const BENCH_BURST_GAP_MIN_NS: u128 = 1_000_000;
const BENCH_BURST_GAP_MAX_NS: u128 = 40_000_000;

/// The message shape emitted by one bursty callsite. Numeric slots vary per event; any embedded
/// endpoint is fixed for the callsite, because a given log statement always names the same endpoint
/// (the Drain clusterer therefore keeps it as static template text, not a wildcard).
#[derive(Clone, Copy)]
enum MessageArchetype {
    Static(&'static str),
    ProcessedEvents,
    BufferCapacity,
    Forwarding(&'static str),
    Retry(&'static str),
}

/// One fixed callsite used by `Bursty` mode: a leaked `&'static Metadata` (so pointer-identity
/// interning behaves exactly as in production) plus the message template and field shape this
/// callsite always emits. Each `field_specs` entry pairs a fixed field key with either a fixed
/// value (stable for the life of the callsite) or `None` (a per-event random number).
struct BurstyCallsite {
    metadata: &'static Metadata<'static>,
    message: MessageArchetype,
    field_specs: Vec<(&'static str, Option<&'static str>)>,
}

/// Draws the fixed callsite table used by `Bursty` mode. Deterministic given the RNG state.
fn build_bursty_callsites(rng: &mut SmallRng) -> Vec<BurstyCallsite> {
    let mut callsites = Vec::with_capacity(BENCH_BURSTY_CALLSITES);
    for _ in 0..BENCH_BURSTY_CALLSITES {
        let target = BENCH_TARGETS[rng.random_range(0..BENCH_TARGETS.len())];
        let file = BENCH_FILES[rng.random_range(0..BENCH_FILES.len())];
        let level = weighted_level(rng);
        let metadata = bench_metadata(target, level, file);

        // 60% short static message, 40% a formatted template -- matching the Iid message mix.
        let message = if rng.random_range(0..100u32) < 60 {
            MessageArchetype::Static(BENCH_SHORT_MESSAGES[rng.random_range(0..BENCH_SHORT_MESSAGES.len())])
        } else {
            match rng.random_range(0..4u32) {
                0 => MessageArchetype::ProcessedEvents,
                1 => MessageArchetype::BufferCapacity,
                2 => MessageArchetype::Forwarding(BENCH_FIELD_VALUES[rng.random_range(0..BENCH_FIELD_VALUES.len())]),
                _ => MessageArchetype::Retry(BENCH_FIELD_VALUES[rng.random_range(0..BENCH_FIELD_VALUES.len())]),
            }
        };

        // Fixed field shape for the callsite: same key set on every event, same count weighting as
        // the Iid generator. Half the values are fixed strings (stable per callsite), half are
        // per-event random numbers.
        let num_fields = weighted_field_count(rng);
        let mut field_specs = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            let key = BENCH_FIELD_KEYS[rng.random_range(0..BENCH_FIELD_KEYS.len())];
            let fixed = if rng.random_range(0..2u32) == 0 {
                Some(BENCH_FIELD_VALUES[rng.random_range(0..BENCH_FIELD_VALUES.len())])
            } else {
                None
            };
            field_specs.push((key, fixed));
        }

        callsites.push(BurstyCallsite {
            metadata,
            message,
            field_specs,
        });
    }
    callsites
}

/// Picks a level using the same weighting as the Iid generator (40% DEBUG, 30% INFO, 20% WARN,
/// 10% ERROR). Used only by the bursty callsite builder.
fn weighted_level(rng: &mut SmallRng) -> tracing::Level {
    let roll: u32 = rng.random_range(0..100);
    if roll < 40 {
        tracing::Level::DEBUG
    } else if roll < 70 {
        tracing::Level::INFO
    } else if roll < 90 {
        tracing::Level::WARN
    } else {
        tracing::Level::ERROR
    }
}

/// Picks a field count using the same weighting as the Iid generator (30% 0, 30% 1, 20% 2, 10% 3,
/// 5% 4, 5% 5). Used only by the bursty callsite builder.
fn weighted_field_count(rng: &mut SmallRng) -> usize {
    let roll: u32 = rng.random_range(0..100);
    if roll < 30 {
        0
    } else if roll < 60 {
        1
    } else if roll < 80 {
        2
    } else if roll < 90 {
        3
    } else if roll < 95 {
        4
    } else {
        5
    }
}

/// Renders a callsite's message, drawing fresh numeric variables while keeping fixed endpoints
/// stable.
fn render_bursty_message(rng: &mut SmallRng, archetype: MessageArchetype) -> String {
    match archetype {
        MessageArchetype::Static(s) => s.to_string(),
        MessageArchetype::ProcessedEvents => format!(
            "Processed {} events in {}ms",
            rng.random_range(1u64..100_000),
            rng.random_range(1u32..5000)
        ),
        MessageArchetype::BufferCapacity => format!(
            "Buffer capacity at {}%, {} bytes used of {}",
            rng.random_range(10u32..100),
            rng.random_range(1024u64..1_048_576),
            rng.random_range(1_048_576u64..10_485_760)
        ),
        MessageArchetype::Forwarding(ep) => {
            format!("Forwarding {} metric(s) to {}", rng.random_range(1u32..10_000), ep)
        }
        MessageArchetype::Retry(ep) => format!(
            "Retry attempt {} of {} for endpoint {}",
            rng.random_range(1u32..5),
            rng.random_range(3u32..10),
            ep
        ),
    }
}

struct EventGenerator {
    rng: SmallRng,
    timestamp_nanos: u128,
    mode: GenMode,
    // Iid mode: one metadata entry per (target, level) combination, picked independently per event.
    metadata_table: Vec<&'static Metadata<'static>>,
    // Bursty mode: the fixed callsite table plus the currently-active burst.
    callsites: Vec<BurstyCallsite>,
    current_callsite: usize,
    burst_remaining: usize,
}

impl EventGenerator {
    fn with_mode(seed: u64, mode: GenMode) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);

        // Note: the Iid branch consumes no RNG during construction, so the Iid draw sequence (and
        // every previously recorded Iid number) is unchanged from the original generator.
        let mut metadata_table = Vec::new();
        let mut callsites = Vec::new();
        match mode {
            GenMode::Iid => {
                // Pre-build a metadata entry for each (target, level) combination.
                for &target in BENCH_TARGETS {
                    let file = BENCH_FILES[metadata_table.len() % BENCH_FILES.len()];
                    for &level in BENCH_LEVELS {
                        metadata_table.push(bench_metadata(target, level, file));
                    }
                }
            }
            GenMode::Bursty => {
                callsites = build_bursty_callsites(&mut rng);
            }
        }

        Self {
            rng,
            timestamp_nanos: 1_700_000_000_000_000_000, // ~Nov 2023
            mode,
            metadata_table,
            callsites,
            current_callsite: 0,
            burst_remaining: 0,
        }
    }

    fn next_event(&mut self) -> CondensedEvent {
        match self.mode {
            GenMode::Iid => self.next_event_iid(),
            GenMode::Bursty => self.next_event_bursty(),
        }
    }

    /// Emits the next event in `Bursty` mode: continue the current burst (sub-ms spacing) or, when
    /// it is exhausted, switch to a new callsite after a larger inter-burst gap.
    fn next_event_bursty(&mut self) -> CondensedEvent {
        if self.burst_remaining == 0 {
            self.current_callsite = self.rng.random_range(0..self.callsites.len());
            self.burst_remaining = self.rng.random_range(BENCH_BURST_MIN..BENCH_BURST_MAX);
            self.timestamp_nanos += self.rng.random_range(BENCH_BURST_GAP_MIN_NS..BENCH_BURST_GAP_MAX_NS);
        } else {
            self.timestamp_nanos += self
                .rng
                .random_range(BENCH_BURST_DELTA_MIN_NS..BENCH_BURST_DELTA_MAX_NS);
        }
        self.burst_remaining -= 1;

        // Copy the small Copy fields out so the callsite borrow does not conflict with `self.rng`.
        let cc = self.current_callsite;
        let metadata = self.callsites[cc].metadata;
        let message_arch = self.callsites[cc].message;
        let message = render_bursty_message(&mut self.rng, message_arch);

        let num_fields = self.callsites[cc].field_specs.len();
        let mut fields = Vec::new();
        for fi in 0..num_fields {
            let (key, fixed) = self.callsites[cc].field_specs[fi];
            let value: String = match fixed {
                Some(v) => v.to_string(),
                None => format!("{}", self.rng.random_range(0u64..1_000_000)),
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

    fn next_event_iid(&mut self) -> CondensedEvent {
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

fn run_benchmark(
    config: RingBufferConfig, config_desc: &str, seed: u64, num_events: usize, mode: GenMode,
) -> BenchmarkResult {
    let mut state = ProcessorState::new(config);
    let mut gen = EventGenerator::with_mode(seed, mode);

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
    config: RingBufferConfig, config_desc: &str, seed: u64, num_events: usize, mode: GenMode,
) -> StabilityResult {
    let mut state = ProcessorState::new(config);
    let mut gen = EventGenerator::with_mode(seed, mode);

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

    let build_phase_stats =
        |retained: &[u32], coverage: &[u64], drops: &[u32], warmup_events: usize, drops_observed: u64| -> PhaseStats {
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

    let result = run_stability_benchmark(
        config,
        "default: max=2MiB, min_segment=128KiB, zstd=19",
        seed,
        num_events,
        GenMode::Iid,
    );
    print_stability_report(&result);
}

#[test]
#[ignore]
fn ring_buffer_stability_bench_bursty() {
    let seed = 0xDEAD_BEEF_CAFE;
    let num_events = 1_500_000;

    let config = RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

    let result = run_stability_benchmark(
        config,
        "BURSTY: max=2MiB, min_segment=128KiB, zstd=19",
        seed,
        num_events,
        GenMode::Bursty,
    );
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
        let result = run_stability_benchmark(config, name, seed, num_events, GenMode::Iid);
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

/// Diagnostic: fill one ~128KiB segment's worth of events and report where the compressed bytes go,
/// column by column. This is the ground-truth map for deciding which columns are worth optimizing.
#[test]
#[ignore]
fn ring_buffer_column_breakdown() {
    run_column_breakdown(GenMode::Iid);
}

#[test]
#[ignore]
fn ring_buffer_column_breakdown_bursty() {
    run_column_breakdown(GenMode::Bursty);
}

fn run_column_breakdown(mode: GenMode) {
    let seed = 0xDEAD_BEEF_CAFE;
    let min_segment = 128 * 1024;

    let mut buffer = EventBuffer::from_compression_level(19);
    let mut gen = EventGenerator::with_mode(seed, mode);

    // Feed events until the buffer reaches the flush threshold.
    let mut fed = 0usize;
    while buffer.size_bytes() < min_segment {
        let event = gen.next_event();
        buffer.encode_event(&event).unwrap();
        fed += 1;
    }

    let bd = buffer.column_breakdown();
    let total_compressed = bd.meta_compressed + bd.content_compressed;

    println!();
    let mode_label = match mode {
        GenMode::Iid => "i.i.d.",
        GenMode::Bursty => "bursty",
    };
    println!("=== Column Breakdown (single segment, {} generator) ===", mode_label);
    println!(
        "Events: {} | string table entries: {}",
        bd.event_count, bd.string_table_entries
    );
    println!(
        "Meta frame:    {} -> {} ({:.2}x)",
        bd.meta_uncompressed,
        bd.meta_compressed,
        bd.meta_uncompressed as f64 / bd.meta_compressed.max(1) as f64
    );
    println!(
        "Content frame: {} -> {} ({:.2}x)",
        bd.content_uncompressed,
        bd.content_compressed,
        bd.content_uncompressed as f64 / bd.content_compressed.max(1) as f64
    );
    println!(
        "Total compressed: {} | bytes/event: {:.2}",
        total_compressed,
        total_compressed as f64 / bd.event_count.max(1) as f64
    );
    println!();
    println!(
        "  {:<22} | {:>10} | {:>10} | {:>10} | {:>7} | {:>7}",
        "column", "uncompr", "compr*", "b/evt(u)", "b/evt(c)", "%oftot"
    );
    // Note: compr* is each column compressed in ISOLATION, so the sum exceeds the real
    // meta+content totals (which share a zstd context). Useful for relative comparison only.
    let sum_alone: usize = bd.entries.iter().map(|e| e.compressed_alone).sum();
    for e in &bd.entries {
        println!(
            "  {:<22} | {:>10} | {:>10} | {:>10.2} | {:>7.2} | {:>6.1}%",
            e.name,
            e.uncompressed,
            e.compressed_alone,
            e.uncompressed as f64 / bd.event_count.max(1) as f64,
            e.compressed_alone as f64 / bd.event_count.max(1) as f64,
            100.0 * e.compressed_alone as f64 / sum_alone.max(1) as f64,
        );
    }
    println!("  (fed {} events to reach {} KiB estimate)", fed, min_segment / 1024);
    println!();
}

/// Diagnostic: measures an **overfit-proof upper bound** on how much a zstd dictionary (or any
/// cross-segment shared-context scheme) could save.
///
/// Each segment is currently compressed as an independent zstd frame, so every segment re-learns the
/// common content vocabulary (repeated field values, message fragments) from scratch. A trained
/// dictionary front-loads that shared context -- but training one on this synthetic generator would
/// overfit and post a fake number. Instead we bound the opportunity with **zero training**: compress
/// N segments' frames independently (current behavior) versus concatenated into a single frame.
/// Concatenation lets each segment reference *all* prior segments (strictly more context than any
/// fixed-size dictionary), so `(independent_sum - concatenated) / independent_sum` is an upper bound
/// on any dictionary's benefit. If that ceiling is small, a dictionary is not worth the complexity.
#[test]
#[ignore]
fn ring_buffer_dictionary_ceiling() {
    let seed = 0xDEAD_BEEF_CAFE;
    let level = 19;
    let min_segment = 128 * 1024;
    let num_segments = 16;

    let zc = |data: &[u8]| -> usize {
        use std::io::Write as _;
        let mut enc = zstd::Encoder::new(Vec::new(), level).unwrap();
        enc.write_all(data).unwrap();
        enc.finish().unwrap().len()
    };

    for (mode_label, mode) in [("i.i.d.", GenMode::Iid), ("bursty", GenMode::Bursty)] {
        let mut gen = EventGenerator::with_mode(seed, mode);
        let mut meta_frames: Vec<Vec<u8>> = Vec::new();
        let mut content_frames: Vec<Vec<u8>> = Vec::new();
        let mut total_events = 0usize;

        for _ in 0..num_segments {
            let mut buffer = EventBuffer::from_compression_level(level);
            while buffer.size_bytes() < min_segment {
                buffer.encode_event(&gen.next_event()).unwrap();
                total_events += 1;
            }
            let (meta, content) = buffer.uncompressed_frames();
            meta_frames.push(meta);
            content_frames.push(content);
        }

        // Independent (current behavior): each frame compressed alone.
        let meta_indep: usize = meta_frames.iter().map(|f| zc(f)).sum();
        let content_indep: usize = content_frames.iter().map(|f| zc(f)).sum();

        // Concatenated (upper bound on shared-context / dictionary schemes).
        let meta_all: Vec<u8> = meta_frames.concat();
        let content_all: Vec<u8> = content_frames.concat();
        let meta_concat = zc(&meta_all);
        let content_concat = zc(&content_all);

        let indep = meta_indep + content_indep;
        let concat = meta_concat + content_concat;
        let ceiling = if indep > 0 {
            100.0 * (indep as i64 - concat as i64) as f64 / indep as f64
        } else {
            0.0
        };

        println!();
        println!(
            "=== Dictionary Ceiling ({} generator, {} segments) ===",
            mode_label, num_segments
        );
        println!("Events: {}", total_events);
        println!(
            "  meta:    independent {} -> concatenated {} ({:+.1}%)",
            meta_indep,
            meta_concat,
            100.0 * (meta_indep as i64 - meta_concat as i64) as f64 / meta_indep.max(1) as f64
        );
        println!(
            "  content: independent {} -> concatenated {} ({:+.1}%)",
            content_indep,
            content_concat,
            100.0 * (content_indep as i64 - content_concat as i64) as f64 / content_indep.max(1) as f64
        );
        println!(
            "  TOTAL:   independent {} -> concatenated {}  =>  dictionary ceiling {:.1}%",
            indep, concat, ceiling
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

    let result = run_benchmark(
        config,
        "max=2MiB, min_segment=128KiB, zstd_level=19",
        seed,
        num_events,
        GenMode::Iid,
    );
    print_report(&result);
}

#[test]
#[ignore]
fn ring_buffer_capacity_bench_bursty() {
    let seed = 0xDEAD_BEEF_CAFE;
    let num_events = 500_000;

    let config = RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

    let result = run_benchmark(
        config,
        "BURSTY: max=2MiB, min_segment=128KiB, zstd_level=19",
        seed,
        num_events,
        GenMode::Bursty,
    );
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
        let result = run_benchmark(config, name, seed, num_events, GenMode::Iid);
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

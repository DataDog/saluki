//! This is a place for re-usable Saluki-only defaults.
//!
//! Important: this is *not* a place where we should restate Datadog Agent schema-derived defaults.
//! Defaults that flow from the Datadog schema are defined by codegen in the appropriate crate. We
//! only want the definitive codegen defaults rather than restating values that can drift (see
//! #1802).

use std::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

/// Default internal telemetry verbosity.
pub const DEFAULT_METRICS_LEVEL: &str = "info";

/// Default memory-accounting slop factor.
pub const DEFAULT_MEMORY_SLOP_FACTOR: f64 = 0.25;

/// Default global memory limiter state.
pub const DEFAULT_ENABLE_GLOBAL_LIMITER: bool = true;

/// Default Checks IPC endpoint.
pub const DEFAULT_CHECKS_IPC_ENDPOINT: &str = "tcp://0.0.0.0:5105";

/// Default length of an aggregation window.
pub const DEFAULT_AGGREGATE_WINDOW_DURATION_SECONDS: NonZeroU64 = NonZeroU64::new(10).unwrap();

/// Default maximum number of contexts held per aggregation window.
pub const DEFAULT_AGGREGATE_CONTEXT_LIMIT: usize = 1_000_000;

/// Default period between aggregation flushes.
pub const DEFAULT_AGGREGATE_FLUSH_INTERVAL: Duration = Duration::from_secs(15);

/// Default delay before an idle passthrough buffer is flushed.
pub const DEFAULT_AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Default byte capacity of the DogStatsD mapper's string interner.
pub const DEFAULT_DOGSTATSD_MAPPER_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();

/// Default DogStatsD TCP listen port.
///
/// A value of `0` disables the TCP listener.
pub const DEFAULT_DOGSTATSD_TCP_PORT: u16 = 0;

/// Whether DogStatsD UDP listener autoscaling is enabled by default.
pub const DEFAULT_DOGSTATSD_AUTOSCALE_UDP_LISTENERS: bool = false;

/// Default number of DogStatsD packet receive buffers allocated at startup.
pub const DEFAULT_DOGSTATSD_BUFFER_COUNT: usize = 128;

/// Default ceiling on DogStatsD packet receive buffers.
///
/// 32768 buffers at the default 8 KiB buffer size provide 256 MiB of payload capacity.
pub const DEFAULT_DOGSTATSD_BUFFER_COUNT_MAX: usize = 32_768;

/// Whether the DogStatsD decoder relaxes its strictness by default.
///
/// Matches the Datadog Agent, which accepts payloads that violate the DogStatsD spec.
pub const DEFAULT_DOGSTATSD_PERMISSIVE_DECODING: bool = true;

/// Default maximum number of DogStatsD metric contexts held in the cache.
pub const DEFAULT_DOGSTATSD_CACHED_CONTEXTS_LIMIT: usize = 500_000;

/// Default maximum number of DogStatsD tagsets held in the cache.
pub const DEFAULT_DOGSTATSD_CACHED_TAGSETS_LIMIT: usize = 500_000;

/// Whether DogStatsD contexts may be heap-allocated when the string interner is full by default.
pub const DEFAULT_DOGSTATSD_ALLOW_CONTEXT_HEAP_ALLOCS: bool = true;

/// Default floor applied to DogStatsD metric sample rates, which is roughly 260M samples.
///
/// Sample rates below this floor are clamped, bounding how many equivalent samples a single metric
/// can contribute.
pub const DEFAULT_DOGSTATSD_MINIMUM_SAMPLE_RATE: f64 = 0.000000003845;

/// Default timeout before a partially filled encoder payload is flushed.
pub const DEFAULT_ENCODER_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Default zstd compression level for payloads ADP sends.
///
/// Higher than the Agent's default of `1` because ADP compresses more efficiently and can afford
/// better compression: level 3 yields ~6% smaller payloads without a net CPU increase.
pub const DEFAULT_ZSTD_COMPRESSOR_LEVEL: i32 = 3;

/// Default maximum number of metrics packed into a single encoder payload.
pub const DEFAULT_MAX_METRICS_PER_PAYLOAD: usize = 10_000;

/// Default environment for traces that do not provide one.
pub const DEFAULT_TRACE_ENV: &str = "none";

/// Whether error traces are sampled independently of the base sampler by default.
pub const DEFAULT_ERROR_SAMPLING_ENABLED: bool = true;

/// Default rare-sampler traces-per-second budget.
pub const DEFAULT_RARE_SAMPLER_TPS: f64 = 5.0;

/// Default rare-sampler cooldown, in seconds.
pub const DEFAULT_RARE_SAMPLER_COOLDOWN_SECS: f64 = 300.0;

/// Default rare-sampler signature cardinality.
pub const DEFAULT_RARE_SAMPLER_CARDINALITY: usize = 200;

/// Default OTLP trace string interner capacity: 512 KiB.
pub const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap();

/// Maximum string interner capacity: 1 GiB. Arbitrary to stop @blt from blowing us up.
pub const MAX_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024 * 1024).unwrap();

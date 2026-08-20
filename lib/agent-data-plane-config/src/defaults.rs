//! This is a place for re-usable Saluki-only defaults.
//!
//! Important: this is *not* a place where we should restate Datadog Agent schema-derived defaults.
//! Defaults that flow from the Datadog schema are defined by codegen in the appropriate crate. We
//! only want the definitive codegen defaults rather than restating values that can drift (see
//! #1802).

use std::{num::NonZeroUsize, time::Duration};

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

/// Maximum OTLP trace string interner capacity: 1 GiB. Arbitrary to stop @blt from blowing us up.
pub const MAX_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024 * 1024).unwrap();

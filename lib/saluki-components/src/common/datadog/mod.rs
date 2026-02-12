pub mod apm;
pub mod config;
pub mod endpoints;
pub mod io;
pub mod middleware;
pub mod obfuscation;
pub mod protocol;
mod proxy;
pub mod request_builder;
mod retry;
pub mod telemetry;
pub mod transaction;

use saluki_core::data_model::event::trace::Trace;
use stringtheory::MetaString;

/// Metric key used to store Datadog sampling priority (`_sampling_priority_v1`).
pub const SAMPLING_PRIORITY_METRIC_KEY: &str = "_sampling_priority_v1";

/// Default compressed size limit for intake requests.
pub const DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MiB

/// Default uncompressed size limit for intake requests.
pub const DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MiB

/// Metadata tag used to store the sampling decision maker (`_dd.p.dm`).
pub const TAG_DECISION_MAKER: &str = "_dd.p.dm";

/// Decision maker value for probabilistic sampling (matches Datadog Agent).
pub const DECISION_MAKER_PROBABILISTIC: &str = "-9";

/// Metadata key used to store the OTEL trace id.
pub const OTEL_TRACE_ID_META_KEY: &str = "otel.trace_id";

/// Maximum trace id used for deterministic sampling.
pub const MAX_TRACE_ID: u64 = u64::MAX;

/// Precomputed float form of `MAX_TRACE_ID`.
pub const MAX_TRACE_ID_FLOAT: f64 = MAX_TRACE_ID as f64;

/// Hasher used for deterministic sampling.
pub const SAMPLER_HASHER: u64 = 1111111111111111111;

/// Returns whether to keep a trace, based on its ID and a sampling rate.
///
/// This assumes trace IDs are nearly uniformly distributed.
pub fn sample_by_rate(trace_id: u64, rate: f64) -> bool {
    if rate < 1.0 {
        trace_id.wrapping_mul(SAMPLER_HASHER) < (rate * MAX_TRACE_ID_FLOAT) as u64
    } else {
        true
    }
}

pub fn get_trace_env(trace: &Trace, root_span_idx: usize) -> Option<&MetaString> {
    // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/traceutil/trace.go#L19-L20
    let env = trace.spans().get(root_span_idx).and_then(|span| span.meta().get("env"));
    match env {
        Some(env) => Some(env),
        None => {
            for span in trace.spans().iter() {
                if let Some(env) = span.meta().get("env") {
                    return Some(env);
                }
            }
            None
        }
    }
}

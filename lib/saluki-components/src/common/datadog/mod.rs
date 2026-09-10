pub(crate) mod api_key;
pub mod config;
pub mod endpoints;
pub mod io;
pub mod middleware;
pub mod obfuscation;
pub mod protocol;
mod proxy;
pub mod request_builder;
mod retry;
mod retry_capacity;
pub mod telemetry;
#[cfg(test)]
pub(crate) mod test_util;
pub mod transaction;
pub mod validation;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Span, Trace};
use stringtheory::MetaString;
use tracing::debug;

/// Metric key used to store Datadog sampling priority (`_sampling_priority_v1`).
pub const SAMPLING_PRIORITY_METRIC_KEY: &str = "_sampling_priority_v1";

/// Default compressed size limit for intake requests.
pub const DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MiB

/// Default uncompressed size limit for intake requests.
pub const DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MiB

/// Datadog Agent default compressed size limit for generic payloads.
pub const DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT: usize = 2_621_440; // 2.5 MiB

/// Datadog Agent default uncompressed size limit for generic payloads.
pub const DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT: usize = 4_194_304; // 4 MiB

/// Returns payload limits capped to the provided upper bounds.
pub fn clamp_payload_limits(
    uncompressed_len_limit: usize, compressed_len_limit: usize, max_uncompressed_len_limit: usize,
    max_compressed_len_limit: usize,
) -> (usize, usize) {
    (
        uncompressed_len_limit.min(max_uncompressed_len_limit),
        compressed_len_limit.min(max_compressed_len_limit),
    )
}

/// V1 metric series intake path.
pub(crate) const METRICS_SERIES_V1_PATH: &str = "/api/v1/series";

/// V2 metric series intake path.
pub(crate) const METRICS_SERIES_V2_PATH: &str = "/api/v2/series";

/// V3 metric series intake path.
pub(crate) const METRICS_SERIES_V3_PATH: &str = "/api/intake/metrics/v3/series";

/// V3 beta metric series intake path.
pub(crate) const METRICS_SERIES_V3_BETA_PATH: &str = "/api/intake/metrics/v3beta/series";

/// Metric sketches intake path.
pub(crate) const METRICS_SKETCHES_PATH: &str = "/api/beta/sketches";

/// V3 metric sketches intake path.
pub(crate) const METRICS_SKETCHES_V3_PATH: &str = "/api/intake/metrics/v3/sketches";

/// Metric intake paths emitted by the encoder and matched by OPW routing.
///
/// Keep these paths in one place so metric encoding and OPW routing don't drift.
pub(crate) const METRIC_INTAKE_PATHS: [&str; 6] = [
    METRICS_SERIES_V1_PATH,
    METRICS_SERIES_V2_PATH,
    METRICS_SERIES_V3_PATH,
    METRICS_SERIES_V3_BETA_PATH,
    METRICS_SKETCHES_PATH,
    METRICS_SKETCHES_V3_PATH,
];

/// Metadata tag used to store the sampling decision maker (`_dd.p.dm`).
pub const TAG_DECISION_MAKER: &str = "_dd.p.dm";

/// Metadata tag used to store the trace origin (`_dd.origin`).
pub const TAG_ORIGIN: &str = "_dd.origin";

/// Span attribute key used to mark top-level spans (`_top_level`).
pub const TOP_LEVEL_KEY: &str = "_top_level";

/// Decision maker value for probabilistic sampling (matches Datadog Agent).
pub const DECISION_MAKER_PROBABILISTIC: &str = "-9";

/// Decision maker value for manual/user-set sampling (matches Datadog Agent).
pub const DECISION_MAKER_MANUAL: &str = "-4";

/// Metadata key used to store the OTel trace id.
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
    let env = trace
        .spans()
        .get(root_span_idx)
        .and_then(|span| span.attributes.get("env").and_then(AttributeValue::as_string));
    if let Some(env) = env {
        return Some(env);
    }
    for span in trace.spans().iter() {
        if let Some(env) = span.attributes.get("env").and_then(AttributeValue::as_string) {
            return Some(env);
        }
    }
    // Fall back to the payload-level env (set from tracer payload headers or OTLP resource attributes).
    if !trace.payload.env.is_empty() {
        return Some(&trace.payload.env);
    }
    None
}

/// Finds the index of the root span within the given spans.
///
/// Scans backwards for the first span with a zero parent ID (clients often report the root last),
/// falling back to whichever span claims a parent that is absent from the trace. Returns `None`
/// only when `spans` is empty. The spans are never modified; the parent-claim map is scratch
/// state local to this function.
pub fn get_root_span_index(spans: &[Span]) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }

    let length = spans.len();
    let mut parent_id_to_child: FastHashMap<u64, usize> = FastHashMap::default();

    for i in 0..length {
        let j = length - 1 - i;
        if spans[j].parent_id() == 0 {
            return Some(j);
        }
        parent_id_to_child.insert(spans[j].parent_id(), j);
    }

    for span in spans.iter() {
        parent_id_to_child.remove(&span.span_id());
    }

    if parent_id_to_child.len() != 1 {
        debug!("Didn't reliably find the root span for a trace");
    }

    if let Some((_, child_idx)) = parent_id_to_child.iter().next() {
        return Some(*child_idx);
    }

    Some(length - 1)
}

/// Computes top-level marks from the span relationships themselves, marking spans in-place.
///
/// A span is top-level when it is a root span, its parent is missing from the trace (the entry
/// point of a distributed fragment), or its parent is in a different service (the entry point of
/// this service's subtree). Marks are only ever set, never cleared.
pub fn compute_top_level(spans: &mut [Span]) {
    let mut span_id_to_index: FastHashMap<u64, usize> = FastHashMap::default();
    for (i, span) in spans.iter().enumerate() {
        span_id_to_index.insert(span.span_id(), i);
    }

    // Classify while immutably borrowed, then mark in a second pass.
    let mut top_level_indices = Vec::with_capacity(spans.len());
    for (i, span) in spans.iter().enumerate() {
        let parent_id = span.parent_id();
        if parent_id == 0 {
            top_level_indices.push(i);
        } else if let Some(&parent_idx) = span_id_to_index.get(&parent_id) {
            if spans[parent_idx].service() != span.service() {
                top_level_indices.push(i);
            }
        } else {
            top_level_indices.push(i);
        }
    }

    for i in top_level_indices {
        spans[i]
            .attributes
            .insert(MetaString::from_static(TOP_LEVEL_KEY), AttributeValue::Float(1.0));
    }
}

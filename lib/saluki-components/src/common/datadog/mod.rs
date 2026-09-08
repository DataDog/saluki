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
/// This mirrors `traceutil.GetRoot` (datadog-agent/pkg/trace/traceutil/trace.go):
/// - Fast path: scanning backwards, return the last span with a parent ID of zero, since some
///   clients report the root span last.
/// - Otherwise: build a map of parent ID to child span index, remove every parent ID that an
///   actual span satisfies, and return whichever orphaned claim remains. A well-formed trace
///   leaves exactly one.
/// - Graceful failure: if the trace is malformed and no claim survives, return the last span.
///
/// Returns `None` only when `spans` is empty. The spans are never modified; the temporary
/// parent-claim map is scratch state local to this function.
pub fn get_root_span_index(spans: &[Span]) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }

    let length = spans.len();
    let mut parent_id_to_child: FastHashMap<u64, usize> = FastHashMap::default();

    for i in 0..length {
        // Common case optimization: check for a span with a zero parent ID, starting from the
        // end, since some clients report the root span last.
        let j = length - 1 - i;
        if spans[j].parent_id() == 0 {
            return Some(j);
        }
        parent_id_to_child.insert(spans[j].parent_id(), j);
    }

    // Cross out every claim whose parent actually exists in the trace.
    for span in spans.iter() {
        parent_id_to_child.remove(&span.span_id());
    }

    // Here, if the trace is valid, exactly one claim should remain: the root, whose "parent" (0)
    // is a sentinel that no span can ever satisfy.
    if parent_id_to_child.len() != 1 {
        debug!("Didn't reliably find the root span for a trace");
    }

    // Have a safe behavior if that's not the case. Pick a span without its parent present.
    if let Some((_, child_idx)) = parent_id_to_child.iter().next() {
        return Some(*child_idx);
    }

    // Gracefully fail with the last span of the trace.
    Some(length - 1)
}

/// Marks top-level spans in-place, as a fallback for traces whose top-level marks were not already
/// computed (for example, from OTLP span kinds).
///
/// This mirrors `traceutil.ComputeTopLevel` (datadog-agent/pkg/trace/traceutil/trace.go): a span is
/// top-level when it is a root span, its parent is missing from the trace (the parent lives in
/// another chunk or service), or its parent belongs to a different service (the span is the local
/// entry point of this service's subtree). Like the reference implementation, marking only ever
/// sets `_top_level` to 1 and never clears an existing value.
///
/// This must run before samplers or stats read span attributes, matching the ordering in the
/// agent's `Process`, which computes top-level spans before sampling.
pub fn compute_top_level(spans: &mut [Span]) {
    let mut span_id_to_index: FastHashMap<u64, usize> = FastHashMap::default();
    for (i, span) in spans.iter().enumerate() {
        span_id_to_index.insert(span.span_id(), i);
    }

    // First pass: decide which spans are top-level while the spans are immutably borrowed.
    let mut top_level_indices = Vec::with_capacity(spans.len());
    for (i, span) in spans.iter().enumerate() {
        let parent_id = span.parent_id();
        if parent_id == 0 {
            // Root span.
            top_level_indices.push(i);
        } else if let Some(&parent_idx) = span_id_to_index.get(&parent_id) {
            if spans[parent_idx].service() != span.service() {
                // The parent is in the trace but in a different service: local root at a service
                // boundary.
                top_level_indices.push(i);
            }
        } else {
            // The parent is missing from the trace: orphan fragment of a distributed trace.
            top_level_indices.push(i);
        }
    }

    // Second pass: apply the marks.
    for i in top_level_indices {
        spans[i]
            .attributes
            .insert(MetaString::from_static(TOP_LEVEL_KEY), AttributeValue::Float(1.0));
    }
}

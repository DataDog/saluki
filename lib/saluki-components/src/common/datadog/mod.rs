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
mod retry_capacity;
pub mod telemetry;
pub mod transaction;
pub mod validation;

use saluki_core::data_model::event::trace::{AttributeValue, Trace};
use stringtheory::MetaString;

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

/// Datadog Agent default serializer compressor.
pub(crate) const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";

/// Returns the Datadog Agent default serializer compressor.
pub(crate) fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

/// The Datadog Agent's default zstd compression level. Because the Agent forwards its full config
/// (including this default) over the config stream, this is indistinguishable from a user who
/// explicitly set the level to 1, so we treat it as "unset" when resolving the level for ADP.
pub(crate) const AGENT_DEFAULT_ZSTD_COMPRESSOR_LEVEL: i32 = 1;

/// ADP's default zstd compression level. Higher than the Agent's default of 1 because ADP is more
/// efficient and can afford better compression: level 3 yields ~6% smaller payloads without a net
/// CPU increase.
pub(crate) const DEFAULT_ADP_ZSTD_COMPRESSOR_LEVEL: i32 = 3;

/// Resolves the effective zstd compression level for an ADP encoder.
///
/// Precedence:
/// 1. `data_plane.serializer_zstd_compressor_level` (ADP-specific) when set.
/// 2. `serializer_zstd_compressor_level` (Core Agent) when set to a non-default value, so a user who
///    raised the Agent's level still gets it applied to ADP.
/// 3. ADP's default of 3.
pub(crate) fn resolve_zstd_compressor_level(data_plane_level: Option<i32>, agent_level: Option<i32>) -> i32 {
    data_plane_level
        .or_else(|| agent_level.filter(|&level| level != AGENT_DEFAULT_ZSTD_COMPRESSOR_LEVEL))
        .unwrap_or(DEFAULT_ADP_ZSTD_COMPRESSOR_LEVEL)
}

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

#[cfg(test)]
mod tests {
    use super::{resolve_zstd_compressor_level, DEFAULT_ADP_ZSTD_COMPRESSOR_LEVEL};

    #[test]
    fn zstd_level_defaults_to_adp_default_when_nothing_set() {
        assert_eq!(
            resolve_zstd_compressor_level(None, None),
            DEFAULT_ADP_ZSTD_COMPRESSOR_LEVEL
        );
    }

    #[test]
    fn zstd_level_ignores_agent_default_of_one() {
        // The Agent forwards its default of 1, which we can't distinguish from an explicit 1, so it
        // does not override ADP's own default.
        assert_eq!(
            resolve_zstd_compressor_level(None, Some(1)),
            DEFAULT_ADP_ZSTD_COMPRESSOR_LEVEL
        );
    }

    #[test]
    fn zstd_level_uses_agent_value_when_changed_from_default() {
        assert_eq!(resolve_zstd_compressor_level(None, Some(5)), 5);
    }

    #[test]
    fn zstd_level_data_plane_takes_precedence() {
        // The ADP-specific key wins over the Agent value, even a non-default one.
        assert_eq!(resolve_zstd_compressor_level(Some(4), Some(5)), 4);
        // And even when it matches the Agent default of 1.
        assert_eq!(resolve_zstd_compressor_level(Some(1), Some(5)), 1);
    }
}

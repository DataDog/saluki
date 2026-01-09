//! Aggregation keys and helper functions for APM stats.

#![allow(dead_code)]

use std::hash::{Hash, Hasher};

use fnv::FnvHasher;
use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

pub const BUCKET_DURATION_NS: u64 = 10_000_000_000;
pub const TAG_SYNTHETICS: &str = "synthetics";
pub const TAG_SPAN_KIND: &str = "span.kind";
pub const TAG_BASE_SERVICE: &str = "_dd.base_service";
const TAG_STATUS_CODE: &str = "http.status_code";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Aggregation {
    pub bucket_key: BucketsAggregationKey,
    pub payload_key: PayloadAggregationKey,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BucketsAggregationKey {
    pub service: MetaString,
    pub name: MetaString,
    pub resource: MetaString,
    pub span_type: MetaString,
    pub span_kind: MetaString,
    pub status_code: u32,
    pub synthetics: bool,
    pub peer_tags_hash: u64,
    pub is_trace_root: Option<bool>,
    pub grpc_status_code: MetaString,
    pub http_method: MetaString,
    pub http_endpoint: MetaString,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PayloadAggregationKey {
    pub env: MetaString,
    pub hostname: MetaString,
    pub version: MetaString,
    pub container_id: MetaString,
    pub git_commit_sha: MetaString,
    pub image_tag: MetaString,
    pub lang: MetaString,
    pub process_tags_hash: u64,
}

pub fn tags_fnv_hash(tags: &[MetaString]) -> u64 {
    if tags.is_empty() {
        return 0;
    }

    let mut sorted_tags: Vec<&MetaString> = tags.iter().collect();
    sorted_tags.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

    let mut hasher = FnvHasher::default();
    for (i, tag) in sorted_tags.iter().enumerate() {
        if i > 0 {
            hasher.write_u8(0);
        }
        hasher.write(tag.as_bytes());
    }
    hasher.finish()
}

pub fn process_tags_hash(process_tags: &str) -> u64 {
    if process_tags.is_empty() {
        return 0;
    }
    let tags: Vec<MetaString> = process_tags.split(',').map(MetaString::from).collect();
    tags_fnv_hash(&tags)
}

pub fn get_status_code(meta: &FastHashMap<MetaString, MetaString>, metrics: &FastHashMap<MetaString, f64>) -> u32 {
    if let Some(&code) = metrics.get(TAG_STATUS_CODE) {
        return code as u32;
    }

    if let Some(code_str) = meta.get(TAG_STATUS_CODE) {
        if let Ok(code) = code_str.as_ref().parse::<u32>() {
            return code;
        }
    }

    0
}

pub fn get_grpc_status_code(
    meta: &FastHashMap<MetaString, MetaString>, metrics: &FastHashMap<MetaString, f64>,
) -> MetaString {
    const STATUS_CODE_FIELDS: &[&str] = &[
        "rpc.grpc.status_code",
        "grpc.code",
        "rpc.grpc.status.code",
        "grpc.status.code",
    ];

    for key in STATUS_CODE_FIELDS {
        if let Some(value) = meta.get(*key) {
            let str_value = value.as_ref();
            if str_value.is_empty() {
                continue;
            }

            if let Ok(code) = str_value.parse::<u64>() {
                return MetaString::from(code.to_string());
            }

            let normalized = str_value.strip_prefix("StatusCode.").unwrap_or(str_value);
            let upper = normalized.to_uppercase();

            if let Some(code) = grpc_status_name_to_code(&upper) {
                return MetaString::from(code);
            }

            return MetaString::default();
        }
    }

    for key in STATUS_CODE_FIELDS {
        if let Some(&code) = metrics.get(*key) {
            return MetaString::from((code as u64).to_string());
        }
    }

    MetaString::default()
}

fn grpc_status_name_to_code(name: &str) -> Option<&'static str> {
    match name {
        "CANCELLED" | "CANCELED" => Some("1"),
        "INVALIDARGUMENT" => Some("3"),
        "DEADLINEEXCEEDED" => Some("4"),
        "NOTFOUND" => Some("5"),
        "ALREADYEXISTS" => Some("6"),
        "PERMISSIONDENIED" => Some("7"),
        "RESOURCEEXHAUSTED" => Some("8"),
        "FAILEDPRECONDITION" => Some("9"),
        "OUTOFRANGE" => Some("11"),
        "DATALOSS" => Some("15"),
        "OK" => Some("0"),
        "UNKNOWN" => Some("2"),
        "INVALID_ARGUMENT" => Some("3"),
        "DEADLINE_EXCEEDED" => Some("4"),
        "NOT_FOUND" => Some("5"),
        "ALREADY_EXISTS" => Some("6"),
        "PERMISSION_DENIED" => Some("7"),
        "RESOURCE_EXHAUSTED" => Some("8"),
        "FAILED_PRECONDITION" => Some("9"),
        "ABORTED" => Some("10"),
        "OUT_OF_RANGE" => Some("11"),
        "UNIMPLEMENTED" => Some("12"),
        "INTERNAL" => Some("13"),
        "UNAVAILABLE" => Some("14"),
        "DATA_LOSS" => Some("15"),
        "UNAUTHENTICATED" => Some("16"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::Span;

    use super::*;
    use crate::transforms::apm_stats::span_concentrator::SpanConcentrator;
    use crate::transforms::apm_stats::statsraw::new_aggregation_from_span;

    #[test]
    fn test_get_status_code() {
        // Empty span
        let meta = FastHashMap::default();
        let metrics = FastHashMap::default();
        assert_eq!(get_status_code(&meta, &metrics), 0);

        // Meta only
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("http.status_code"), MetaString::from("200"));
        let metrics = FastHashMap::default();
        assert_eq!(get_status_code(&meta, &metrics), 200);

        // Metrics only
        let meta = FastHashMap::default();
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("http.status_code"), 302.0);
        assert_eq!(get_status_code(&meta, &metrics), 302);

        // Both meta and metrics - metrics takes precedence
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("http.status_code"), MetaString::from("200"));
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("http.status_code"), 302.0);
        assert_eq!(get_status_code(&meta, &metrics), 302);

        // Invalid meta value
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("http.status_code"), MetaString::from("x"));
        let metrics = FastHashMap::default();
        assert_eq!(get_status_code(&meta, &metrics), 0);
    }

    #[test]
    fn test_get_grpc_status_code() {
        // Empty span
        let meta = FastHashMap::default();
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "");

        // Meta with lowercase name "aborted"
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status_code"), MetaString::from("aborted"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "10");

        // Metrics with numeric code
        let meta = FastHashMap::default();
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("grpc.code"), 1.0);
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "1");

        // Both meta and metrics - meta takes precedence
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("grpc.status.code"), MetaString::from("0"));
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("grpc.status.code"), 1.0);
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "0");

        // Numeric string in meta
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("15"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "15");

        // "Canceled" (mixed case)
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("Canceled"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "1");

        // "CANCELLED" (uppercase)
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("CANCELLED"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "1");

        // With "StatusCode." prefix
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("grpc.status.code"),
            MetaString::from("StatusCode.ABORTED"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "10");

        // Invalid prefix (typo)
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("grpc.status.code"),
            MetaString::from("StatusCodee.ABORTED"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "");

        // "InvalidArgument" (PascalCase)
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("rpc.grpc.status_code"),
            MetaString::from("InvalidArgument"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics).as_ref(), "3");
    }

    #[test]
    fn test_new_aggregation() {
        // Helper to create a span with given service, meta, and metrics
        let make_span =
            |service: &str, meta: FastHashMap<MetaString, MetaString>, metrics: FastHashMap<MetaString, f64>| {
                Span::default()
                    .with_service(service)
                    .with_meta(meta)
                    .with_metrics(metrics)
            };

        // Helper to add _dd.measured metric
        let with_measured = |mut metrics: FastHashMap<MetaString, f64>| {
            metrics.insert(MetaString::from("_dd.measured"), 1.0);
            metrics
        };

        // nil case, peer tag aggregation disabled
        {
            let concentrator = SpanConcentrator::new(true, false, &[], 0);
            let metrics = with_measured(FastHashMap::default());
            let span = make_span("", FastHashMap::default(), metrics);
            let stat_span = concentrator.new_stat_span_from_span(&span);
            assert!(
                stat_span.is_none() || {
                    let s = stat_span.unwrap();
                    let agg = new_aggregation_from_span(&s, "", PayloadAggregationKey::default());
                    agg.bucket_key.service.is_empty() && agg.bucket_key.span_kind.is_empty()
                }
            );
        }

        // peer tag aggregation disabled even though peer.service is present
        {
            let concentrator = SpanConcentrator::new(true, false, &[], 0);
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            meta.insert(MetaString::from("peer.service"), MetaString::from("remote-service"));
            let metrics = with_measured(FastHashMap::default());
            let span = make_span("a", meta, metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.service.as_ref(), "a");
                assert_eq!(agg.bucket_key.span_kind.as_ref(), "client");
                assert_eq!(agg.bucket_key.peer_tags_hash, 0); // disabled
            }
        }

        // peer tags aggregation enabled, span.kind == client
        {
            let peer_tags = vec![
                MetaString::from("db.instance"),
                MetaString::from("db.system"),
                MetaString::from("peer.service"),
            ];
            let concentrator = SpanConcentrator::new(true, true, &peer_tags, 0);
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            meta.insert(MetaString::from("peer.service"), MetaString::from("remote-service"));
            let metrics = with_measured(FastHashMap::default());
            let span = make_span("a", meta, metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.service.as_ref(), "a");
                assert_eq!(agg.bucket_key.span_kind.as_ref(), "client");
                assert_ne!(agg.bucket_key.peer_tags_hash, 0); // enabled and has peer tag
            }
        }

        // peer tags aggregation enabled, span.kind == producer
        {
            let peer_tags = vec![
                MetaString::from("db.instance"),
                MetaString::from("db.system"),
                MetaString::from("peer.service"),
            ];
            let concentrator = SpanConcentrator::new(true, true, &peer_tags, 0);
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from("span.kind"), MetaString::from("producer"));
            meta.insert(MetaString::from("peer.service"), MetaString::from("remote-service"));
            let metrics = with_measured(FastHashMap::default());
            let span = make_span("a", meta, metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.service.as_ref(), "a");
                assert_eq!(agg.bucket_key.span_kind.as_ref(), "producer");
                assert_ne!(agg.bucket_key.peer_tags_hash, 0);
            }
        }

        // peer tags aggregation enabled but all peer tags are empty
        {
            let peer_tags = vec![
                MetaString::from("db.instance"),
                MetaString::from("db.system"),
                MetaString::from("peer.service"),
            ];
            let concentrator = SpanConcentrator::new(true, true, &peer_tags, 0);
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            meta.insert(MetaString::from("peer.service"), MetaString::from(""));
            meta.insert(MetaString::from("db.instance"), MetaString::from(""));
            meta.insert(MetaString::from("db.system"), MetaString::from(""));
            let metrics = with_measured(FastHashMap::default());
            let span = make_span("a", meta, metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.service.as_ref(), "a");
                assert_eq!(agg.bucket_key.span_kind.as_ref(), "client");
                assert_eq!(agg.bucket_key.peer_tags_hash, 0); // empty tags = 0 hash
            }
        }
    }

    #[test]
    fn test_peer_tags_to_aggregate_for_span() {
        // Tests that peer tags are only returned for client/producer/consumer span kinds
        let peer_tags = vec![MetaString::from("server.address"), MetaString::from("_dd.base_service")];

        let make_span_with_kind = |kind: &str| {
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from("span.kind"), MetaString::from(kind));
            meta.insert(MetaString::from("server.address"), MetaString::from("test-server"));
            let mut metrics = FastHashMap::default();
            metrics.insert(MetaString::from("_dd.measured"), 1.0);
            Span::default().with_meta(meta).with_metrics(metrics)
        };

        let concentrator = SpanConcentrator::new(true, true, &peer_tags, 0);

        // client - should have peer tags
        let span = make_span_with_kind("client");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(!stat_span.matching_peer_tags.is_empty(), "client should have peer tags");
        }

        // CLIENT (uppercase) - should have peer tags
        let span = make_span_with_kind("CLIENT");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(!stat_span.matching_peer_tags.is_empty(), "CLIENT should have peer tags");
        }

        // producer - should have peer tags
        let span = make_span_with_kind("producer");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(
                !stat_span.matching_peer_tags.is_empty(),
                "producer should have peer tags"
            );
        }

        // consumer - should have peer tags
        let span = make_span_with_kind("consumer");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(
                !stat_span.matching_peer_tags.is_empty(),
                "consumer should have peer tags"
            );
        }

        // server - should NOT have peer tags
        let span = make_span_with_kind("server");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(
                stat_span.matching_peer_tags.is_empty(),
                "server should NOT have peer tags"
            );
        }

        // internal - should NOT have peer tags (no base_service)
        let span = make_span_with_kind("internal");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(
                stat_span.matching_peer_tags.is_empty(),
                "internal should NOT have peer tags"
            );
        }

        // empty - should NOT have peer tags
        let span = make_span_with_kind("");
        if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
            assert!(
                stat_span.matching_peer_tags.is_empty(),
                "empty span.kind should NOT have peer tags"
            );
        }
    }

    #[test]
    fn test_is_root_span() {
        let concentrator = SpanConcentrator::new(true, true, &[], 0);

        // Span with parent_id = 0 -> is_trace_root = true
        {
            let mut metrics = FastHashMap::default();
            metrics.insert(MetaString::from("_dd.measured"), 1.0);
            let span = Span::default().with_parent_id(0).with_metrics(metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.is_trace_root, Some(true));
            }
        }

        // Span with parent_id != 0 -> is_trace_root = false
        {
            let mut metrics = FastHashMap::default();
            metrics.insert(MetaString::from("_dd.measured"), 1.0);
            let span = Span::default().with_parent_id(123).with_metrics(metrics);
            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
                assert_eq!(agg.bucket_key.is_trace_root, Some(false));
            }
        }
    }
}

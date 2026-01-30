//! Aggregation keys and helper functions for APM stats.

use std::hash::{Hash, Hasher};

use fnv::FnvHasher;
use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

pub const BUCKET_DURATION_NS: u64 = 10_000_000_000;
pub const TAG_SYNTHETICS: &str = "synthetics";
pub const TAG_SPAN_KIND: &str = "span.kind";
pub const TAG_BASE_SERVICE: &str = "_dd.base_service";
const TAG_STATUS_CODE: &str = "http.status_code";

/// Span kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SpanKind {
    /// Unspecified or empty span kind.
    #[default]
    Unspecified = 0,

    /// Internal span (not a network boundary).
    Internal = 1,

    /// Server-side span (handling incoming request).
    Server = 2,

    /// Client-side span (making outgoing request).
    Client = 3,

    /// Producer span (sending message to queue/topic).
    Producer = 4,

    /// Consumer span (receiving message from queue/topic).
    Consumer = 5,
}

impl SpanKind {
    /// Parses a span kind from a string in a case-insensitive fashion.
    ///
    /// If the span kind is not recognized, `SpanKind::Unspecified` is returned.
    pub const fn from_str(s: &str) -> Self {
        if s.eq_ignore_ascii_case("client") {
            Self::Client
        } else if s.eq_ignore_ascii_case("server") {
            Self::Server
        } else if s.eq_ignore_ascii_case("producer") {
            Self::Producer
        } else if s.eq_ignore_ascii_case("consumer") {
            Self::Consumer
        } else if s.eq_ignore_ascii_case("internal") {
            Self::Internal
        } else {
            Self::Unspecified
        }
    }

    /// Converts `self` into a string representation.
    pub const fn to_metastring(self) -> MetaString {
        match self {
            Self::Unspecified => MetaString::empty(),
            Self::Internal => MetaString::from_static("internal"),
            Self::Server => MetaString::from_static("server"),
            Self::Client => MetaString::from_static("client"),
            Self::Producer => MetaString::from_static("producer"),
            Self::Consumer => MetaString::from_static("consumer"),
        }
    }
}

/// gRPC status code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GrpcStatusCode {
    /// No gRPC status code present.
    #[default]
    Unset = 255,

    /// Success.
    Ok = 0,

    /// Cancelled.
    Cancelled = 1,

    /// Unknown error.
    Unknown = 2,

    /// Invalid argument.
    InvalidArgument = 3,

    /// Deadline exceeded.
    DeadlineExceeded = 4,

    /// Not found.
    NotFound = 5,

    /// Already exists.
    AlreadyExists = 6,

    /// Permission denied.
    PermissionDenied = 7,

    /// Resource exhausted.
    ResourceExhausted = 8,

    /// Failed precondition.
    FailedPrecondition = 9,

    /// Aborted.
    Aborted = 10,

    /// Out of range.
    OutOfRange = 11,

    /// Unimplemented.
    Unimplemented = 12,

    /// Internal error.
    Internal = 13,

    /// Unavailable.
    Unavailable = 14,

    /// Data loss.
    DataLoss = 15,

    /// Unauthenticated.
    Unauthenticated = 16,
}

impl GrpcStatusCode {
    /// Parses a numeric code into a `GrpcStatusCode`.
    ///
    /// If the code is out of range, `GrpcStatusCode::Unset` is returned.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Ok,
            1 => Self::Cancelled,
            2 => Self::Unknown,
            3 => Self::InvalidArgument,
            4 => Self::DeadlineExceeded,
            5 => Self::NotFound,
            6 => Self::AlreadyExists,
            7 => Self::PermissionDenied,
            8 => Self::ResourceExhausted,
            9 => Self::FailedPrecondition,
            10 => Self::Aborted,
            11 => Self::OutOfRange,
            12 => Self::Unimplemented,
            13 => Self::Internal,
            14 => Self::Unavailable,
            15 => Self::DataLoss,
            16 => Self::Unauthenticated,
            _ => Self::Unset,
        }
    }

    /// Parses a gRPC status code from a string, either in numeric or the canonical form.
    ///
    /// If the status code is not recognized, or is out of range, `GrpcStatusCode::Unset` is returned.
    pub fn from_str(s: &str) -> Self {
        if let Ok(code) = s.parse::<u8>() {
            return Self::from_code(code);
        }

        // Strip StatusCode. prefix if present
        let normalized = s.strip_prefix("StatusCode.").unwrap_or(s);
        let upper = normalized.to_uppercase();

        // TODO: do this with case-insensitive matching so we don't bother with allocating
        // just to uppercase the string
        match upper.as_str() {
            "OK" => Self::Ok,
            "CANCELLED" | "CANCELED" => Self::Cancelled,
            "UNKNOWN" => Self::Unknown,
            "INVALIDARGUMENT" | "INVALID_ARGUMENT" => Self::InvalidArgument,
            "DEADLINEEXCEEDED" | "DEADLINE_EXCEEDED" => Self::DeadlineExceeded,
            "NOTFOUND" | "NOT_FOUND" => Self::NotFound,
            "ALREADYEXISTS" | "ALREADY_EXISTS" => Self::AlreadyExists,
            "PERMISSIONDENIED" | "PERMISSION_DENIED" => Self::PermissionDenied,
            "RESOURCEEXHAUSTED" | "RESOURCE_EXHAUSTED" => Self::ResourceExhausted,
            "FAILEDPRECONDITION" | "FAILED_PRECONDITION" => Self::FailedPrecondition,
            "ABORTED" => Self::Aborted,
            "OUTOFRANGE" | "OUT_OF_RANGE" => Self::OutOfRange,
            "UNIMPLEMENTED" => Self::Unimplemented,
            "INTERNAL" => Self::Internal,
            "UNAVAILABLE" => Self::Unavailable,
            "DATALOSS" | "DATA_LOSS" => Self::DataLoss,
            "UNAUTHENTICATED" => Self::Unauthenticated,
            _ => Self::Unset,
        }
    }

    /// Converts `self` to a string representation.
    pub const fn to_metastring(self) -> MetaString {
        match self {
            Self::Unset => MetaString::empty(),
            Self::Ok => MetaString::from_static("0"),
            Self::Cancelled => MetaString::from_static("1"),
            Self::Unknown => MetaString::from_static("2"),
            Self::InvalidArgument => MetaString::from_static("3"),
            Self::DeadlineExceeded => MetaString::from_static("4"),
            Self::NotFound => MetaString::from_static("5"),
            Self::AlreadyExists => MetaString::from_static("6"),
            Self::PermissionDenied => MetaString::from_static("7"),
            Self::ResourceExhausted => MetaString::from_static("8"),
            Self::FailedPrecondition => MetaString::from_static("9"),
            Self::Aborted => MetaString::from_static("10"),
            Self::OutOfRange => MetaString::from_static("11"),
            Self::Unimplemented => MetaString::from_static("12"),
            Self::Internal => MetaString::from_static("13"),
            Self::Unavailable => MetaString::from_static("14"),
            Self::DataLoss => MetaString::from_static("15"),
            Self::Unauthenticated => MetaString::from_static("16"),
        }
    }
}

/// HTTP method.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HttpMethod {
    /// No HTTP method or unknown method.
    #[default]
    Unspecified = 0,

    /// GET request.
    Get = 1,

    /// POST request.
    Post = 2,

    /// PUT request.
    Put = 3,

    /// DELETE request.
    Delete = 4,

    /// PATCH request.
    Patch = 5,

    /// HEAD request.
    Head = 6,

    /// OPTIONS request.
    Options = 7,

    /// CONNECT request.
    Connect = 8,

    /// TRACE request.
    Trace = 9,
}

impl HttpMethod {
    /// Parses an HTTP method from a string in a case-insensitive fashion.
    ///
    /// If the HTTP method is not recognized, `HttpMethod::Unspecified` is returned.
    pub const fn from_str(s: &str) -> Self {
        if s.eq_ignore_ascii_case("GET") {
            Self::Get
        } else if s.eq_ignore_ascii_case("POST") {
            Self::Post
        } else if s.eq_ignore_ascii_case("PUT") {
            Self::Put
        } else if s.eq_ignore_ascii_case("DELETE") {
            Self::Delete
        } else if s.eq_ignore_ascii_case("PATCH") {
            Self::Patch
        } else if s.eq_ignore_ascii_case("HEAD") {
            Self::Head
        } else if s.eq_ignore_ascii_case("OPTIONS") {
            Self::Options
        } else if s.eq_ignore_ascii_case("CONNECT") {
            Self::Connect
        } else if s.eq_ignore_ascii_case("TRACE") {
            Self::Trace
        } else {
            Self::Unspecified
        }
    }

    /// Converts `self` to a string representation.
    pub const fn to_metastring(self) -> MetaString {
        match self {
            Self::Unspecified => MetaString::empty(),
            Self::Get => MetaString::from_static("GET"),
            Self::Post => MetaString::from_static("POST"),
            Self::Put => MetaString::from_static("PUT"),
            Self::Delete => MetaString::from_static("DELETE"),
            Self::Patch => MetaString::from_static("PATCH"),
            Self::Head => MetaString::from_static("HEAD"),
            Self::Options => MetaString::from_static("OPTIONS"),
            Self::Connect => MetaString::from_static("CONNECT"),
            Self::Trace => MetaString::from_static("TRACE"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Aggregation {
    pub bucket_key: BucketsAggregationKey,
    pub payload_key: PayloadAggregationKey,
}

impl Aggregation {
    pub fn into_parts(self) -> (BucketsAggregationKey, PayloadAggregationKey) {
        (self.bucket_key, self.payload_key)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BucketsAggregationKey {
    pub service: MetaString,
    pub name: MetaString,
    pub resource: MetaString,
    pub span_type: MetaString,
    pub span_kind: SpanKind,
    pub status_code: u32,
    pub synthetics: bool,
    pub peer_tags_hash: u64,
    pub is_trace_root: Option<bool>,
    pub grpc_status_code: GrpcStatusCode,
    pub http_method: HttpMethod,
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

pub fn tags_fnv_hash<I, T>(tags: I) -> u64
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut sorted_tags: Vec<T> = tags.into_iter().collect();
    sorted_tags.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

    let mut count = 0;
    let mut hasher = FnvHasher::default();
    for (i, tag) in sorted_tags.iter().enumerate() {
        if i > 0 {
            hasher.write_u8(0);
        }
        hasher.write(tag.as_ref().as_bytes());
        count += 1;
    }

    if count > 0 {
        hasher.finish()
    } else {
        0
    }
}

pub fn process_tags_hash(process_tags: &str) -> u64 {
    if process_tags.is_empty() {
        return 0;
    }

    tags_fnv_hash(process_tags.split(','))
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
) -> GrpcStatusCode {
    const STATUS_CODE_FIELDS: &[&str] = &[
        "rpc.grpc.status_code",
        "grpc.code",
        "rpc.grpc.status.code",
        "grpc.status.code",
    ];

    for key in STATUS_CODE_FIELDS {
        if let Some(value) = meta.get(*key) {
            if value.is_empty() {
                continue;
            }

            return GrpcStatusCode::from_str(value.as_ref());
        }
    }

    for key in STATUS_CODE_FIELDS {
        if let Some(&code) = metrics.get(*key) {
            return GrpcStatusCode::from_code(code as u8);
        }
    }

    GrpcStatusCode::Unset
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
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Unset);

        // Meta with lowercase name "aborted"
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status_code"), MetaString::from("aborted"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Aborted);

        // Metrics with numeric code
        let meta = FastHashMap::default();
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("grpc.code"), 1.0);
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Cancelled);

        // Both meta and metrics - meta takes precedence
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("grpc.status.code"), MetaString::from("0"));
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("grpc.status.code"), 1.0);
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Ok);

        // Numeric string in meta
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("15"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::DataLoss);

        // "Canceled" (mixed case)
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("Canceled"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Cancelled);

        // "CANCELLED" (uppercase)
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("rpc.grpc.status.code"), MetaString::from("CANCELLED"));
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Cancelled);

        // With "StatusCode." prefix
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("grpc.status.code"),
            MetaString::from("StatusCode.ABORTED"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Aborted);

        // Invalid prefix (typo)
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("grpc.status.code"),
            MetaString::from("StatusCodee.ABORTED"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::Unset);

        // "InvalidArgument" (PascalCase)
        let mut meta = FastHashMap::default();
        meta.insert(
            MetaString::from("rpc.grpc.status_code"),
            MetaString::from("InvalidArgument"),
        );
        let metrics = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&meta, &metrics), GrpcStatusCode::InvalidArgument);
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
                    agg.bucket_key.service.is_empty() && agg.bucket_key.span_kind == SpanKind::Unspecified
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
                assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
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
                assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
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
                assert_eq!(agg.bucket_key.span_kind, SpanKind::Producer);
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
                assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
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

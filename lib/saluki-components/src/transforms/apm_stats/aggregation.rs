//! Aggregation keys and helper functions for APM stats.

use std::{
    collections::hash_map::Entry,
    fmt,
    hash::{Hash, Hasher},
};

use fnv::FnvHasher;
use saluki_common::collections::{FastHashMap, PrehashedHashMap};
use saluki_core::data_model::event::trace::AttributeValue;
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
    /// If the span kind isn't recognized, `SpanKind::Unspecified` is returned.
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
            Self::Unspecified => MetaString::from_static("unspecified"),
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
    /// If the status code isn't recognized, or is out of range, `GrpcStatusCode::Unset` is returned.
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
    /// If the HTTP method isn't recognized, `HttpMethod::Unspecified` is returned.
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

/// A registry that stores aggregation keys with reference counting.
///
/// Keys are stored once and referenced by hash. When a bucket is flushed,
/// it decrements the reference count for its keys, and keys with zero
/// references are automatically removed.
pub struct AggregationRegistry {
    entries: PrehashedHashMap<AggregationKeyHash, RegistryEntry>,
}

struct RegistryEntry {
    key: Aggregation,
    ref_count: u32,
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct AggregationKeyHash(u64);

impl fmt::Display for AggregationKeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

impl AggregationRegistry {
    /// Creates a new empty registry.
    pub fn new() -> Self {
        Self {
            entries: PrehashedHashMap::default(),
        }
    }

    /// Computes a stable hash for an aggregation key.
    #[inline]
    pub fn hash_key(key: &Aggregation) -> AggregationKeyHash {
        let mut hasher = FnvHasher::default();
        key.hash(&mut hasher);
        AggregationKeyHash(hasher.finish())
    }

    /// Inserts a new key or increments the reference count if it already exists.
    ///
    /// Called when a bucket first uses this key. The caller should have already
    /// computed the hash via `hash_key()`.
    pub fn insert_or_increment(&mut self, key_hash: AggregationKeyHash, key: Aggregation) {
        self.entries
            .entry(key_hash)
            .and_modify(|e| e.ref_count += 1)
            .or_insert_with(|| RegistryEntry { key, ref_count: 1 });
    }

    /// Decrements the reference count for a key, removing it if the count reaches zero.
    pub fn decrement(&mut self, key_hash: AggregationKeyHash) {
        if let Entry::Occupied(mut entry) = self.entries.entry(key_hash) {
            let new_ref_count = entry.get().ref_count.saturating_sub(1);
            if new_ref_count == 0 {
                entry.remove();
            } else {
                entry.get_mut().ref_count = new_ref_count;
            }
        }
    }

    /// Looks up the full aggregation key by its hash.
    ///
    /// Used during export to reconstruct the full key from the hash.
    #[inline]
    pub fn get(&self, key_hash: AggregationKeyHash) -> Option<&Aggregation> {
        self.entries.get(&key_hash).map(|e| &e.key)
    }

    /// Returns the number of unique keys currently in the registry.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the registry contains no keys.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for AggregationRegistry {
    fn default() -> Self {
        Self::new()
    }
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

pub fn get_status_code(attributes: &FastHashMap<MetaString, AttributeValue>) -> u32 {
    if let Some(code) = attributes.get(TAG_STATUS_CODE).and_then(AttributeValue::as_num) {
        return code as u32;
    }

    if let Some(code_str) = attributes.get(TAG_STATUS_CODE).and_then(AttributeValue::as_string) {
        if let Ok(code) = code_str.as_ref().parse::<u32>() {
            return code;
        }
    }

    0
}

pub fn get_grpc_status_code(attributes: &FastHashMap<MetaString, AttributeValue>) -> GrpcStatusCode {
    const STATUS_CODE_FIELDS: &[&str] = &[
        "rpc.grpc.status_code",
        "grpc.code",
        "rpc.grpc.status.code",
        "grpc.status.code",
    ];

    for key in STATUS_CODE_FIELDS {
        if let Some(value) = attributes.get(*key).and_then(AttributeValue::as_string) {
            if value.is_empty() {
                continue;
            }

            return GrpcStatusCode::from_str(value.as_ref());
        }
    }

    for key in STATUS_CODE_FIELDS {
        if let Some(code) = attributes.get(*key).and_then(AttributeValue::as_num) {
            return GrpcStatusCode::from_code(code as u8);
        }
    }

    GrpcStatusCode::Unset
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::{AttributeValue as AV, Span};

    use super::*;
    use crate::transforms::apm_stats::span_concentrator::SpanConcentrator;
    use crate::transforms::apm_stats::statsraw::new_aggregation_from_span;

    #[test]
    fn get_status_code_from_attributes() {
        // Empty attrs
        let attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        assert_eq!(get_status_code(&attrs), 0);

        // String value only
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("http.status_code"),
            AV::String(MetaString::from("200")),
        );
        assert_eq!(get_status_code(&attrs), 200);

        // Float value only
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(MetaString::from("http.status_code"), AV::Float(302.0));
        assert_eq!(get_status_code(&attrs), 302);

        // Both string and float in same map — float takes precedence (checked first)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(MetaString::from("http.status_code"), AV::Float(302.0));
        assert_eq!(get_status_code(&attrs), 302);

        // Invalid string value
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(MetaString::from("http.status_code"), AV::String(MetaString::from("x")));
        assert_eq!(get_status_code(&attrs), 0);
    }

    #[test]
    fn get_grpc_status_code_from_attributes() {
        // Empty attrs
        let attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Unset);

        // String with lowercase name "aborted"
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("rpc.grpc.status_code"),
            AV::String(MetaString::from("aborted")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Aborted);

        // Float with numeric code
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(MetaString::from("grpc.code"), AV::Float(1.0));
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Cancelled);

        // String takes precedence over float (string path is checked first)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(MetaString::from("grpc.status.code"), AV::String(MetaString::from("0")));
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Ok);

        // Numeric string in attrs
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("rpc.grpc.status.code"),
            AV::String(MetaString::from("15")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::DataLoss);

        // "Canceled" (mixed case)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("rpc.grpc.status.code"),
            AV::String(MetaString::from("Canceled")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Cancelled);

        // "CANCELLED" (uppercase)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("rpc.grpc.status.code"),
            AV::String(MetaString::from("CANCELLED")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Cancelled);

        // With "StatusCode." prefix
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("grpc.status.code"),
            AV::String(MetaString::from("StatusCode.ABORTED")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Aborted);

        // Invalid prefix (typo)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("grpc.status.code"),
            AV::String(MetaString::from("StatusCodee.ABORTED")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::Unset);

        // "InvalidArgument" (PascalCase)
        let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
        attrs.insert(
            MetaString::from("rpc.grpc.status_code"),
            AV::String(MetaString::from("InvalidArgument")),
        );
        assert_eq!(get_grpc_status_code(&attrs), GrpcStatusCode::InvalidArgument);
    }

    #[test]
    fn new_aggregation() {
        // Helper to create a span with given service, meta, and metrics
        let make_span = |service: &str,
                         meta: FastHashMap<MetaString, MetaString>,
                         metrics: FastHashMap<MetaString, f64>| {
            let mut attrs: FastHashMap<MetaString, AV> = meta.into_iter().map(|(k, v)| (k, AV::String(v))).collect();
            attrs.extend(metrics.into_iter().map(|(k, v)| (k, AV::Float(v))));
            Span::default().with_service(service).with_attributes(attrs)
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
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.service.as_ref(), "a");
            assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
            assert_eq!(agg.bucket_key.peer_tags_hash, 0); // disabled
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
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.service.as_ref(), "a");
            assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
            assert_ne!(agg.bucket_key.peer_tags_hash, 0); // enabled and has peer tag
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
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.service.as_ref(), "a");
            assert_eq!(agg.bucket_key.span_kind, SpanKind::Producer);
            assert_ne!(agg.bucket_key.peer_tags_hash, 0);
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
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.service.as_ref(), "a");
            assert_eq!(agg.bucket_key.span_kind, SpanKind::Client);
            assert_eq!(agg.bucket_key.peer_tags_hash, 0); // empty tags = 0 hash
        }
    }

    #[test]
    fn peer_tags_to_aggregate_for_span() {
        // Tests that peer tags are only returned for client/producer/consumer span kinds
        let peer_tags = vec![MetaString::from("server.address"), MetaString::from("_dd.base_service")];

        let make_span_with_kind = |kind: &str| {
            let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
            attrs.insert(MetaString::from("span.kind"), AV::String(MetaString::from(kind)));
            attrs.insert(
                MetaString::from("server.address"),
                AV::String(MetaString::from("test-server")),
            );
            attrs.insert(MetaString::from("_dd.measured"), AV::Float(1.0));
            Span::default().with_attributes(attrs)
        };

        let concentrator = SpanConcentrator::new(true, true, &peer_tags, 0);

        // Peer tags are only aggregated for client/producer/consumer span kinds (case-insensitive); server, internal,
        // and unspecified kinds must not carry them. Each case unwraps with `.expect(...)` so a regression that makes
        // `new_stat_span_from_span` return `None` fails loudly instead of silently skipping the assertion.
        let cases = [
            ("client", true),
            ("CLIENT", true),
            ("producer", true),
            ("consumer", true),
            ("server", false),
            ("internal", false),
            ("", false),
        ];

        for (span_kind, should_have_peer_tags) in cases {
            let span = make_span_with_kind(span_kind);
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            assert_eq!(
                !stat_span.matching_peer_tags.is_empty(),
                should_have_peer_tags,
                "span.kind={:?} peer-tag expectation mismatch (matching_peer_tags={:?})",
                span_kind,
                stat_span.matching_peer_tags,
            );
        }
    }

    #[test]
    fn is_root_span() {
        let concentrator = SpanConcentrator::new(true, true, &[], 0);

        // Span with parent_id = 0 -> is_trace_root = true
        {
            let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
            attrs.insert(MetaString::from("_dd.measured"), AV::Float(1.0));
            let span = Span::default().with_parent_id(0).with_attributes(attrs);
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.is_trace_root, Some(true));
        }

        // Span with parent_id != 0 -> is_trace_root = false
        {
            let mut attrs: FastHashMap<MetaString, AV> = FastHashMap::default();
            attrs.insert(MetaString::from("_dd.measured"), AV::Float(1.0));
            let span = Span::default().with_parent_id(123).with_attributes(attrs);
            let stat_span = concentrator
                .new_stat_span_from_span(&span)
                .expect("measured span should always produce a stat span");
            let agg = new_aggregation_from_span(&stat_span, "", PayloadAggregationKey::default());
            assert_eq!(agg.bucket_key.is_trace_root, Some(false));
        }
    }
}

use std::sync::LazyLock;

use ddsketch::canonical::mapping::FixedLogarithmicMapping;
use ddsketch::canonical::store::CollapsingLowestDenseStore;
use ddsketch::canonical::PositiveOnlyDDSketch;
use protobuf::Message;
use rand::Rng as _;
use saluki_common::collections::{FastHashMap, PrehashedHashMap};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace_stats::{ClientGroupedStats, ClientStatsBucket};
use stringtheory::MetaString;
use tracing::error;

use super::aggregation::{
    tags_fnv_hash, Aggregation, AggregationKeyHash, AggregationRegistry, BucketsAggregationKey, GrpcStatusCode,
    HttpMethod, PayloadAggregationKey, SpanKind, TAG_SYNTHETICS,
};
use super::span_concentrator::StatSpan;

static EMPTY_DDSKETCH_ENCODED: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let sketch = PositiveOnlyDDSketch::new(FixedLogarithmicMapping, CollapsingLowestDenseStore::new(2048));
    let sketch_proto = sketch.to_proto();
    sketch_proto.write_to_bytes().unwrap()
});

/// Type alias for DDSketch optimized for latencies (positive-only values).
///
/// Uses `PositiveOnlyDDSketch` which eliminates the negative store, saving 48 bytes per sketch.
/// Combined with the zero-sized `FixedLogarithmicMapping`, this saves 64 bytes per sketch
/// compared to the standard `DDSketch<LogarithmicMapping, CollapsingLowestDenseStore>`.
type ApmDDSketch = PositiveOnlyDDSketch<FixedLogarithmicMapping, CollapsingLowestDenseStore>;

struct GroupedStats {
    hits: f64,
    top_level_hits: f64,
    errors: f64,
    duration: f64,
    ok_distribution: ApmDDSketch,
    /// Error distribution is lazily initialized on first error to save memory.
    /// Many services have few or no errors, so this avoids allocating a full sketch in those cases.
    err_distribution: Option<Box<ApmDDSketch>>,
    peer_tags: Vec<MetaString>,
}

impl GroupedStats {
    // Note: The code in the agent uses `ddsketch.LogCollapsingLowestDenseDDSketch(0.01, 2048)`
    fn new() -> Self {
        Self {
            hits: 0.0,
            top_level_hits: 0.0,
            errors: 0.0,
            duration: 0.0,
            ok_distribution: ApmDDSketch::default(),
            err_distribution: None,
            peer_tags: Vec::new(),
        }
    }

    fn export(self, bucket_agg_key: BucketsAggregationKey) -> ClientGroupedStats {
        let ok_summary = convert_to_ddsketch_proto_bytes(&self.ok_distribution);
        let error_summary = self
            .err_distribution
            .as_ref()
            .map(|d| convert_to_ddsketch_proto_bytes(d))
            .unwrap_or_else(|| EMPTY_DDSKETCH_ENCODED.clone());

        ClientGroupedStats::new(bucket_agg_key.service, bucket_agg_key.name, bucket_agg_key.resource)
            .with_http_status_code(bucket_agg_key.status_code)
            .with_span_type(bucket_agg_key.span_type)
            .with_hits(round(self.hits))
            .with_errors(round(self.errors))
            .with_duration(round(self.duration))
            .with_top_level_hits(round(self.top_level_hits))
            .with_ok_summary(ok_summary)
            .with_error_summary(error_summary)
            .with_synthetics(bucket_agg_key.synthetics)
            .with_span_kind(bucket_agg_key.span_kind.to_metastring())
            .with_peer_tags(self.peer_tags)
            .with_is_trace_root(bucket_agg_key.is_trace_root)
            .with_grpc_status_code(bucket_agg_key.grpc_status_code.to_metastring())
            .with_http_method(bucket_agg_key.http_method.to_metastring())
            .with_http_endpoint(bucket_agg_key.http_endpoint)
    }
}

impl Default for GroupedStats {
    fn default() -> Self {
        Self::new()
    }
}
pub struct RawBucket {
    /// Timestamp of the bucket start.
    start: u64,
    /// Bucket duration in nanoseconds.
    duration: u64,
    /// Stats grouped by aggregation key hash.
    /// The full aggregation keys are stored in the shared `AggregationRegistry`.
    data: FastHashMap<AggregationKeyHash, GroupedStats>,
    /// Map of container ID to container tags.
    pub(super) container_tags_by_id: FastHashMap<MetaString, SharedTagSet>,
    /// Map of process tags hash to process tags.
    pub(super) process_tags_by_hash: PrehashedHashMap<u64, MetaString>,
}

impl RawBucket {
    pub fn new(start: u64, duration: u64) -> Self {
        Self {
            start,
            duration,
            data: FastHashMap::default(),
            container_tags_by_id: FastHashMap::default(),
            process_tags_by_hash: PrehashedHashMap::default(),
        }
    }

    /// Store process tags for later export.
    pub fn set_process_tags(&mut self, hash: u64, tags: MetaString) {
        self.process_tags_by_hash.insert(hash, tags);
    }

    /// Store container tags for later export.
    pub fn set_container_tags(&mut self, container_id: MetaString, tags: impl Into<SharedTagSet>) {
        self.container_tags_by_id.insert(container_id, tags.into());
    }

    /// Get container tags by container ID.
    #[allow(unused)]
    pub fn get_container_tags(&self, container_id: &MetaString) -> Option<&SharedTagSet> {
        self.container_tags_by_id.get(container_id)
    }

    /// Get process tags by hash.
    #[allow(unused)]
    pub fn get_process_tags(&self, hash: u64) -> Option<&MetaString> {
        self.process_tags_by_hash.get(&hash)
    }

    /// Handles a span by aggregating its stats into this bucket.
    ///
    /// The `registry` is used to store/lookup the full aggregation key, while this bucket
    /// only stores the hash as the key. This deduplicates keys across buckets.
    pub fn handle_span(
        &mut self, span: &StatSpan, weight: f64, origin: &str, payload_key: PayloadAggregationKey,
        registry: &mut AggregationRegistry,
    ) {
        if payload_key.env.is_empty() {
            error!("PayloadAggregationKey env should never be empty");
            return;
        }
        let aggr = new_aggregation_from_span(span, origin, payload_key);
        let key_hash = AggregationRegistry::hash_key(&aggr);

        // Use entry API - registry increment only happens on actual insert
        let gs = self.data.entry(key_hash).or_insert_with(|| {
            registry.insert_or_increment(key_hash, aggr);
            let mut gs = GroupedStats::new();
            gs.peer_tags = span.matching_peer_tags.clone();
            gs
        });

        if span.is_top_level {
            gs.top_level_hits += weight;
        }
        gs.hits += weight;

        if span.error != 0 {
            gs.errors += weight;
        }

        gs.duration += (span.duration as f64) * weight;

        let trunc_dur = ns_timestamp_to_float(span.duration);
        if span.error != 0 {
            gs.err_distribution
                .get_or_insert_with(|| Box::new(ApmDDSketch::default()))
                .add(trunc_dur);
        } else {
            gs.ok_distribution.add(trunc_dur);
        }
    }

    /// Exports this bucket's data, looking up full keys from the registry.
    ///
    /// The `registry` is used to retrieve the full aggregation keys from their hashes. After looking up each key, the
    /// reference count is decremented inline, removing keys that are no longer referenced by any bucket.
    pub fn export(self, registry: &mut AggregationRegistry) -> ExportedBucket {
        let mut data = FastHashMap::default();

        for (key_hash, gs) in self.data {
            // Look up the full aggregation key from the registry
            let Some(agg) = registry.get(key_hash) else {
                error!("Missing aggregation key in registry for hash {}", key_hash);
                continue;
            };

            let (bucket_agg_key, payload_agg_key) = agg.clone().into_parts();

            // Decrement reference count now that we've cloned the key.
            // Keys with zero references are automatically removed.
            registry.decrement(key_hash);

            let grouped_stats = gs.export(bucket_agg_key);

            let bucket = data
                .entry(payload_agg_key)
                .or_insert_with(|| ClientStatsBucket::new(self.start, self.duration, Vec::new()));
            bucket.stats_mut().push(grouped_stats);
        }

        ExportedBucket {
            data,
            container_tags_by_id: self.container_tags_by_id,
            process_tags_by_hash: self.process_tags_by_hash,
        }
    }
}

pub struct ExportedBucket {
    pub data: FastHashMap<PayloadAggregationKey, ClientStatsBucket>,
    pub container_tags_by_id: FastHashMap<MetaString, SharedTagSet>,
    pub process_tags_by_hash: PrehashedHashMap<u64, MetaString>,
}

pub(super) fn new_aggregation_from_span(
    span: &StatSpan, origin: &str, payload_key: PayloadAggregationKey,
) -> Aggregation {
    let synthetics = origin.starts_with(TAG_SYNTHETICS);
    let is_trace_root = if span.parent_id == 0 { Some(true) } else { Some(false) };

    Aggregation {
        payload_key,
        bucket_key: BucketsAggregationKey {
            service: span.service.clone(),
            name: span.name.clone(),
            resource: span.resource.clone(),
            span_type: span.typ.clone(),
            span_kind: SpanKind::from_str(span.span_kind.as_ref()),
            status_code: span.status_code,
            synthetics,
            is_trace_root,
            grpc_status_code: GrpcStatusCode::from_str(span.grpc_status_code.as_ref()),
            peer_tags_hash: tags_fnv_hash(&span.matching_peer_tags),
            http_method: HttpMethod::from_str(span.http_method.as_ref()),
            http_endpoint: span.http_endpoint.clone(),
        },
    }
}
// Convert a nanosec timestamp into a float nanosecond timestamp truncated to a fixed precision.
fn ns_timestamp_to_float(ns: u64) -> f64 {
    let f = ns as f64;
    let bits = f.to_bits();
    // IEEE-754
    // the mask include 1 bit sign 11 bits exponent (0xfff)
    // then we filter the mantissa to 10bits (0xff8) (9 bits as it has implicit value of 1)
    // 10 bits precision (any value will be +/- 1/1024)
    // https://en.wikipedia.org/wiki/Double-precision_floating-point_format
    let truncated_bits = bits & 0xffff_f800_0000_0000;
    f64::from_bits(truncated_bits)
}

fn round(f: f64) -> u64 {
    let i = f as u64;
    let frac = f - (i as f64);
    if rand::rng().random::<f64>() < frac {
        i + 1
    } else {
        i
    }
}

fn convert_to_ddsketch_proto_bytes(sketch: &ApmDDSketch) -> Vec<u8> {
    let proto = sketch.to_proto();
    match proto.write_to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = ?e, "Failed to serialize DDSketch to canonical Protocol Buffers representation.");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ns_timestamp_to_float() {
        let ns: u64 = 1_000_000_000;
        let truncated = ns_timestamp_to_float(ns);
        let relative_error = ((truncated - ns as f64) / (ns as f64)).abs();
        assert!(relative_error < 0.001, "relative error too large: {}", relative_error);

        assert_eq!(ns_timestamp_to_float(0), 0.0);

        let small: u64 = 1000;
        let truncated_small = ns_timestamp_to_float(small);
        assert!(truncated_small > 0.0);
    }

    #[test]
    fn test_round_deterministic_cases() {
        assert_eq!(round(0.0), 0);
        assert_eq!(round(1.0), 1);
        assert_eq!(round(100.0), 100);
    }

    #[test]
    fn test_grouped_stats_new() {
        let gs = GroupedStats::new();
        assert_eq!(gs.hits, 0.0);
        assert_eq!(gs.top_level_hits, 0.0);
        assert_eq!(gs.errors, 0.0);
        assert_eq!(gs.duration, 0.0);
        assert!(gs.ok_distribution.is_empty());
        assert!(gs.err_distribution.is_none()); // Lazily initialized
        assert!(gs.peer_tags.is_empty());
    }

    #[test]
    fn test_raw_bucket_new() {
        let bucket = RawBucket::new(1000, 10_000_000_000);
        assert_eq!(bucket.start, 1000);
        assert_eq!(bucket.duration, 10_000_000_000);
        assert!(bucket.data.is_empty());
    }

    #[test]
    fn test_grain() {
        let s = StatSpan {
            service: MetaString::from("thing"),
            name: MetaString::from("other"),
            resource: MetaString::from("yo"),
            typ: MetaString::default(),
            span_kind: MetaString::default(),
            status_code: 0,
            error: 0,
            parent_id: 0,
            start: 0,
            duration: 0,
            is_top_level: false,
            matching_peer_tags: Vec::new(),
            grpc_status_code: MetaString::default(),
            http_method: MetaString::default(),
            http_endpoint: MetaString::default(),
        };
        let aggr = new_aggregation_from_span(
            &s,
            "",
            PayloadAggregationKey {
                env: MetaString::from("default"),
                hostname: MetaString::from("default"),
                container_id: MetaString::from("cid"),
                ..PayloadAggregationKey::default()
            },
        );

        assert_eq!(aggr.payload_key.env.as_ref(), "default");
        assert_eq!(aggr.payload_key.hostname.as_ref(), "default");
        assert_eq!(aggr.payload_key.container_id.as_ref(), "cid");
        assert_eq!(aggr.bucket_key.service.as_ref(), "thing");
        assert_eq!(aggr.bucket_key.name.as_ref(), "other");
        assert_eq!(aggr.bucket_key.resource.as_ref(), "yo");
        assert_eq!(aggr.bucket_key.is_trace_root, Some(true)); // parent_id == 0
    }

    #[test]
    fn test_grain_with_peer_tags() {
        // none present - span.kind = client but no peer tags in meta
        {
            let s = StatSpan {
                service: MetaString::from("thing"),
                name: MetaString::from("other"),
                resource: MetaString::from("yo"),
                typ: MetaString::default(),
                span_kind: MetaString::from("client"),
                status_code: 0,
                error: 0,
                parent_id: 0,
                start: 0,
                duration: 0,
                is_top_level: false,
                matching_peer_tags: Vec::new(), // no peer tags
                grpc_status_code: MetaString::default(),
                http_method: MetaString::default(),
                http_endpoint: MetaString::default(),
            };
            let aggr = new_aggregation_from_span(
                &s,
                "",
                PayloadAggregationKey {
                    env: MetaString::from("default"),
                    hostname: MetaString::from("default"),
                    container_id: MetaString::from("cid"),
                    ..PayloadAggregationKey::default()
                },
            );
            assert_eq!(aggr.bucket_key.service.as_ref(), "thing");
            assert_eq!(aggr.bucket_key.span_kind, SpanKind::Client);
            assert_eq!(aggr.bucket_key.peer_tags_hash, 0); // no peer tags
        }

        // partially present - some peer tags
        {
            let s = StatSpan {
                service: MetaString::from("thing"),
                name: MetaString::from("other"),
                resource: MetaString::from("yo"),
                typ: MetaString::default(),
                span_kind: MetaString::from("client"),
                status_code: 0,
                error: 0,
                parent_id: 0,
                start: 0,
                duration: 0,
                is_top_level: false,
                matching_peer_tags: vec![
                    MetaString::from("peer.service:aws-s3"),
                    MetaString::from("aws.s3.bucket:bucket-a"),
                ],
                grpc_status_code: MetaString::default(),
                http_method: MetaString::default(),
                http_endpoint: MetaString::default(),
            };
            let aggr = new_aggregation_from_span(
                &s,
                "",
                PayloadAggregationKey {
                    env: MetaString::from("default"),
                    hostname: MetaString::from("default"),
                    container_id: MetaString::from("cid"),
                    ..PayloadAggregationKey::default()
                },
            );
            assert_eq!(aggr.bucket_key.service.as_ref(), "thing");
            assert_eq!(aggr.bucket_key.span_kind, SpanKind::Client);
            assert_ne!(aggr.bucket_key.peer_tags_hash, 0); // has peer tags
        }

        // all present - multiple peer tags
        {
            let s = StatSpan {
                service: MetaString::from("thing"),
                name: MetaString::from("other"),
                resource: MetaString::from("yo"),
                typ: MetaString::default(),
                span_kind: MetaString::from("client"),
                status_code: 0,
                error: 0,
                parent_id: 0,
                start: 0,
                duration: 0,
                is_top_level: false,
                matching_peer_tags: vec![
                    MetaString::from("peer.service:aws-dynamodb"),
                    MetaString::from("db.instance:dynamo.test.us1"),
                    MetaString::from("db.system:dynamodb"),
                ],
                grpc_status_code: MetaString::default(),
                http_method: MetaString::default(),
                http_endpoint: MetaString::default(),
            };
            let aggr = new_aggregation_from_span(
                &s,
                "",
                PayloadAggregationKey {
                    env: MetaString::from("default"),
                    hostname: MetaString::from("default"),
                    container_id: MetaString::from("cid"),
                    ..PayloadAggregationKey::default()
                },
            );
            assert_eq!(aggr.bucket_key.service.as_ref(), "thing");
            assert_eq!(aggr.bucket_key.span_kind, SpanKind::Client);
            assert_ne!(aggr.bucket_key.peer_tags_hash, 0);
        }
    }

    #[test]
    fn test_grain_with_synthetics() {
        let s = StatSpan {
            service: MetaString::from("thing"),
            name: MetaString::from("other"),
            resource: MetaString::from("yo"),
            typ: MetaString::default(),
            span_kind: MetaString::default(),
            status_code: 418,
            error: 0,
            parent_id: 0,
            start: 0,
            duration: 0,
            is_top_level: false,
            matching_peer_tags: Vec::new(),
            grpc_status_code: MetaString::default(),
            http_method: MetaString::default(),
            http_endpoint: MetaString::default(),
        };

        // With synthetics origin
        let aggr = new_aggregation_from_span(
            &s,
            "synthetics-browser",
            PayloadAggregationKey {
                hostname: MetaString::from("host-id"),
                version: MetaString::from("v0"),
                env: MetaString::from("default"),
                container_id: MetaString::from("cid"),
                ..PayloadAggregationKey::default()
            },
        );

        assert_eq!(aggr.payload_key.hostname.as_ref(), "host-id");
        assert_eq!(aggr.payload_key.version.as_ref(), "v0");
        assert_eq!(aggr.payload_key.env.as_ref(), "default");
        assert_eq!(aggr.payload_key.container_id.as_ref(), "cid");
        assert_eq!(aggr.bucket_key.service.as_ref(), "thing");
        assert_eq!(aggr.bucket_key.resource.as_ref(), "yo");
        assert_eq!(aggr.bucket_key.name.as_ref(), "other");
        assert_eq!(aggr.bucket_key.status_code, 418);
        assert!(aggr.bucket_key.synthetics); // synthetics flag should be true
        assert_eq!(aggr.bucket_key.is_trace_root, Some(true));

        // Without synthetics origin
        let aggr2 = new_aggregation_from_span(
            &s,
            "real-traffic",
            PayloadAggregationKey {
                hostname: MetaString::from("host-id"),
                env: MetaString::from("default"),
                ..PayloadAggregationKey::default()
            },
        );
        assert!(!aggr2.bucket_key.synthetics); // synthetics flag should be false
    }
}

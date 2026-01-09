use ddsketch_agent::DDSketch;
use protobuf::Message;
use rand::Rng as _;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace_stats::{ClientGroupedStats, ClientStatsBucket};
use stringtheory::MetaString;
use tracing::error;

use super::aggregation::{tags_fnv_hash, Aggregation, BucketsAggregationKey, PayloadAggregationKey, TAG_SYNTHETICS};
use super::span_concentrator::StatSpan;

struct GroupedStats {
    hits: f64,
    top_level_hits: f64,
    errors: f64,
    duration: f64,
    ok_distribution: DDSketch,
    err_distribution: DDSketch,
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
            ok_distribution: DDSketch::default(),
            err_distribution: DDSketch::default(),
            peer_tags: Vec::new(),
        }
    }

    fn export(self, agg: &Aggregation) -> ClientGroupedStats {
        let ok_summary = convert_to_ddsketch_proto_bytes(&self.ok_distribution);
        let error_summary = convert_to_ddsketch_proto_bytes(&self.err_distribution);

        ClientGroupedStats::new(
            agg.bucket_key.service.clone(),
            agg.bucket_key.name.clone(),
            agg.bucket_key.resource.clone(),
        )
        .with_http_status_code(agg.bucket_key.status_code)
        .with_span_type(agg.bucket_key.span_type.clone())
        .with_hits(round(self.hits))
        .with_errors(round(self.errors))
        .with_duration(round(self.duration))
        .with_top_level_hits(round(self.top_level_hits))
        .with_ok_summary(ok_summary)
        .with_error_summary(error_summary)
        .with_synthetics(agg.bucket_key.synthetics)
        .with_span_kind(agg.bucket_key.span_kind.clone())
        .with_peer_tags(self.peer_tags)
        .with_is_trace_root(agg.bucket_key.is_trace_root)
        .with_grpc_status_code(agg.bucket_key.grpc_status_code.clone())
        .with_http_method(agg.bucket_key.http_method.clone())
        .with_http_endpoint(agg.bucket_key.http_endpoint.clone())
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
    data: FastHashMap<Aggregation, GroupedStats>,
    /// Map of container ID to container tags.
    pub(super) container_tags_by_id: FastHashMap<MetaString, Vec<MetaString>>,
    /// Map of process tags hash to process tags.
    pub(super) process_tags_by_hash: FastHashMap<u64, MetaString>,
}

impl RawBucket {
    pub fn new(start: u64, duration: u64) -> Self {
        Self {
            start,
            duration,
            data: FastHashMap::default(),
            container_tags_by_id: FastHashMap::default(),
            process_tags_by_hash: FastHashMap::default(),
        }
    }

    /// Store process tags for later export.
    pub fn set_process_tags(&mut self, hash: u64, tags: MetaString) {
        self.process_tags_by_hash.insert(hash, tags);
    }

    /// Store container tags for later export.
    pub fn set_container_tags(&mut self, container_id: MetaString, tags: Vec<MetaString>) {
        self.container_tags_by_id.insert(container_id, tags);
    }

    /// Get container tags by container ID.
    #[allow(unused)]
    pub fn get_container_tags(&self, container_id: &MetaString) -> Option<&Vec<MetaString>> {
        self.container_tags_by_id.get(container_id)
    }

    /// Get process tags by hash.
    #[allow(unused)]
    pub fn get_process_tags(&self, hash: u64) -> Option<&MetaString> {
        self.process_tags_by_hash.get(&hash)
    }

    pub fn handle_span(&mut self, span: &StatSpan, weight: f64, origin: &str, payload_key: PayloadAggregationKey) {
        if payload_key.env.is_empty() {
            error!("PayloadAggregationKey env should never be empty");
            return;
        }
        let aggr = new_aggregation_from_span(span, origin, payload_key);
        self.add(span, weight, aggr);
    }

    fn add(&mut self, span: &StatSpan, weight: f64, aggr: Aggregation) {
        let gs = self.data.entry(aggr).or_insert_with(|| {
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
            gs.err_distribution.insert(trunc_dur);
        } else {
            gs.ok_distribution.insert(trunc_dur);
        }
    }

    pub fn export(self) -> FastHashMap<PayloadAggregationKey, ClientStatsBucket> {
        let mut result = FastHashMap::default();

        for (agg, gs) in self.data {
            let grouped_stats = gs.export(&agg);

            let bucket = result
                .entry(agg.payload_key)
                .or_insert_with(|| ClientStatsBucket::new(self.start, self.duration, Vec::new()));
            bucket.stats_mut().push(grouped_stats);
        }

        result
    }
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
            span_kind: span.span_kind.clone(),
            status_code: span.status_code,
            synthetics,
            is_trace_root,
            grpc_status_code: span.grpc_status_code.clone(),
            peer_tags_hash: tags_fnv_hash(&span.matching_peer_tags),
            http_method: span.http_method.clone(),
            http_endpoint: span.http_endpoint.clone(),
        },
    }
}

fn ns_timestamp_to_float(ns: u64) -> f64 {
    let f = ns as f64;
    let bits = f.to_bits();
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

fn convert_to_ddsketch_proto_bytes(sketch: &DDSketch) -> Vec<u8> {
    let proto = sketch.to_proto();
    match proto.write_to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(error = ?e, "Failed to serialize DDSketch to protobuf bytes.");
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
            assert_eq!(aggr.bucket_key.span_kind.as_ref(), "client");
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
            assert_eq!(aggr.bucket_key.span_kind.as_ref(), "client");
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
            assert_eq!(aggr.bucket_key.span_kind.as_ref(), "client");
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

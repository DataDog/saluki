//! Span concentrator for APM stats computation.

use saluki_common::collections::FastHashMap;
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;
use saluki_core::data_model::event::trace_stats::{ClientStatsBucket, ClientStatsPayload};
use stringtheory::MetaString;

use super::aggregation::{
    get_grpc_status_code, get_status_code, process_tags_hash, PayloadAggregationKey, BUCKET_DURATION_NS,
    TAG_BASE_SERVICE, TAG_SPAN_KIND,
};
use super::statsraw::RawBucket;

const DEFAULT_BUFFER_LEN: u64 = 2;
const METRIC_TOP_LEVEL: &str = "_top_level";
const METRIC_MEASURED: &str = "_dd.measured";
pub const METRIC_PARTIAL_VERSION: &str = "_dd.partial_version";

/// Base peer tags copied from `pkg/trace/config/peer_tags.ini`.
const BASE_PEER_TAGS: &[&str] = &[
    "_dd.base_service",
    "active_record.db.vendor",
    "amqp.destination",
    "amqp.exchange",
    "amqp.queue",
    "aws.queue.name",
    "aws.s3.bucket",
    "bucketname",
    "cassandra.keyspace",
    "db.cassandra.contact.points",
    "db.couchbase.seed.nodes",
    "db.hostname",
    "db.instance",
    "db.name",
    "db.namespace",
    "db.system",
    "db.type",
    "dns.hostname",
    "grpc.host",
    "hostname",
    "http.host",
    "http.server_name",
    "messaging.destination",
    "messaging.destination.name",
    "messaging.kafka.bootstrap.servers",
    "messaging.rabbitmq.exchange",
    "messaging.system",
    "mongodb.db",
    "msmq.queue.path",
    "net.peer.name",
    "network.destination.ip",
    "network.destination.name",
    "out.host",
    "peer.hostname",
    "peer.service",
    "queuename",
    "rpc.service",
    "rpc.system",
    "sequel.db.vendor",
    "server.address",
    "streamname",
    "tablename",
    "topicname",
];

#[derive(Clone, Default)]
pub struct InfraTags {
    /// Container ID from the tracer payload.
    pub container_id: MetaString,
    /// Container tags resolved from the container runtime.
    pub container_tags: SharedTagSet,
    /// Hash of the process tags string.
    pub process_tags_hash: u64,
    /// Process tags string from the tracer payload.
    pub process_tags: MetaString,
}

impl InfraTags {
    pub fn new(
        container_id: impl Into<MetaString>, container_tags: SharedTagSet, process_tags: impl Into<MetaString>,
    ) -> Self {
        let process_tags: MetaString = process_tags.into();
        let process_tags_hash = process_tags_hash(process_tags.as_ref());
        Self {
            container_id: container_id.into(),
            container_tags,
            process_tags_hash,
            process_tags,
        }
    }
}

pub(super) struct StatSpan {
    pub(super) service: MetaString,
    pub(super) resource: MetaString,
    pub(super) name: MetaString,
    pub(super) typ: MetaString,
    pub(super) span_kind: MetaString,
    pub(super) status_code: u32,
    pub(super) error: i32,
    pub(super) parent_id: u64,
    pub(super) start: u64,
    pub(super) duration: u64,
    pub(super) is_top_level: bool,
    pub(super) matching_peer_tags: Vec<MetaString>,
    pub(super) grpc_status_code: MetaString,
    pub(super) http_method: MetaString,
    pub(super) http_endpoint: MetaString,
}

pub struct SpanConcentrator {
    /// Whether to compute stats based on span.kind
    compute_stats_by_span_kind: bool,

    /// Whether peer tags aggregation is enabled
    peer_tags_aggregation: bool,

    /// Configured peer tag keys for aggregation (base + custom)
    peer_tag_keys: Vec<MetaString>,

    /// Bucket duration in nanoseconds (10s)
    bsize: u64,

    /// Timestamp of oldest allowed bucket (prevents stats for already-flushed buckets)
    oldest_ts: u64,

    /// Number of buckets to buffer before flushing
    buffer_len: u64,

    /// Time-bucketed raw stats: bucket_timestamp -> RawBucket
    buckets: FastHashMap<u64, RawBucket>,
}

impl SpanConcentrator {
    pub fn new(
        compute_stats_by_span_kind: bool, peer_tags_aggregation: bool, custom_peer_tags: &[MetaString], now: u64,
    ) -> Self {
        let mut peer_tag_keys: Vec<MetaString> = BASE_PEER_TAGS.iter().map(|s| MetaString::from_static(s)).collect();
        for tag in custom_peer_tags {
            if !peer_tag_keys.iter().any(|t| t == tag) {
                peer_tag_keys.push(tag.clone());
            }
        }

        Self {
            compute_stats_by_span_kind,
            peer_tags_aggregation,
            peer_tag_keys,
            bsize: BUCKET_DURATION_NS,
            oldest_ts: align_ts(now, BUCKET_DURATION_NS),
            buffer_len: DEFAULT_BUFFER_LEN,
            buckets: FastHashMap::default(),
        }
    }

    pub fn new_stat_span_from_span(&self, span: &Span) -> Option<StatSpan> {
        self.new_stat_span(span)
    }

    pub fn add_span(
        &mut self, stat_span: &StatSpan, weight: f64, payload_key: &PayloadAggregationKey, infra_tags: &InfraTags,
        origin: &str,
    ) {
        self.add_span_internal(stat_span, weight, payload_key, infra_tags, origin);
    }

    pub fn flush(&mut self, now: u64, force: bool) -> Vec<ClientStatsPayload> {
        let mut m = FastHashMap::<PayloadAggregationKey, Vec<ClientStatsBucket>>::default();
        let mut container_tags_by_id = FastHashMap::<MetaString, SharedTagSet>::default();
        let mut process_tags_by_hash = FastHashMap::<u64, MetaString>::default();

        let timestamps: Vec<u64> = self.buckets.keys().copied().collect();

        for ts in timestamps {
            if !force && ts + self.buffer_len * self.bsize > now {
                continue;
            }

            if let Some(srb) = self.buckets.remove(&ts) {
                let exported = srb.export();

                for (k, b) in exported.data {
                    if let Some(ctags) = exported.container_tags_by_id.get(&k.container_id) {
                        container_tags_by_id.insert(k.container_id.clone(), ctags.clone());
                    }
                    if let Some(ptags) = exported.process_tags_by_hash.get(&k.process_tags_hash) {
                        process_tags_by_hash.insert(k.process_tags_hash, ptags.clone());
                    }
                    m.entry(k).or_default().push(b);
                }
            }
        }

        let new_oldest_ts = align_ts(now, self.bsize).saturating_sub((self.buffer_len - 1) * self.bsize);
        if new_oldest_ts > self.oldest_ts {
            self.oldest_ts = new_oldest_ts;
        }

        let mut sb: Vec<ClientStatsPayload> = Vec::with_capacity(m.len());
        for (k, s) in m {
            let p = ClientStatsPayload::new(k.hostname, k.env, k.version)
                .with_stats(s)
                .with_tags(container_tags_by_id.get(&k.container_id).cloned().unwrap_or_default())
                .with_container_id(k.container_id)
                .with_git_commit_sha(k.git_commit_sha)
                .with_image_tag(k.image_tag)
                .with_lang(k.lang)
                .with_process_tags(
                    process_tags_by_hash
                        .get(&k.process_tags_hash)
                        .cloned()
                        .unwrap_or_default(),
                )
                .with_process_tags_hash(k.process_tags_hash);
            sb.push(p);
        }
        sb
    }

    fn is_span_eligible(&self, span: &Span) -> bool {
        if let Some(&val) = span.metrics().get(METRIC_TOP_LEVEL) {
            if val == 1.0 {
                return true;
            }
        }
        if let Some(&val) = span.metrics().get(METRIC_MEASURED) {
            if val == 1.0 {
                return true;
            }
        }
        if self.compute_stats_by_span_kind {
            if let Some(kind) = span.meta().get(TAG_SPAN_KIND) {
                return compute_stats_for_span_kind(kind);
            }
        }
        false
    }

    fn new_stat_span(&self, span: &Span) -> Option<StatSpan> {
        if !self.is_span_eligible(span) {
            return None;
        }

        if is_partial_snapshot(span) {
            return None;
        }

        let span_kind = span.meta().get(TAG_SPAN_KIND).cloned().unwrap_or_default();
        let status_code = get_status_code(span.meta(), span.metrics());
        let grpc_status_code = get_grpc_status_code(span.meta(), span.metrics());
        let is_top_level = span.metrics().get(METRIC_TOP_LEVEL).map(|&v| v == 1.0).unwrap_or(false);
        let matching_peer_tags = self.matching_peer_tags(span, &span_kind);

        Some(StatSpan {
            service: MetaString::from(span.service()),
            resource: MetaString::from(span.resource()),
            name: MetaString::from(span.name()),
            typ: MetaString::from(span.span_type()),
            span_kind,
            status_code,
            error: span.error(),
            parent_id: span.parent_id(),
            start: span.start(),
            duration: span.duration(),
            is_top_level,
            matching_peer_tags,
            grpc_status_code,
            http_method: MetaString::default(),
            http_endpoint: MetaString::default(),
        })
    }

    fn matching_peer_tags(&self, span: &Span, span_kind: &str) -> Vec<MetaString> {
        let mut peer_tags = Vec::new();

        let keys_to_check = self.peer_tag_keys_to_aggregate_for_span(span_kind, span.meta().get(TAG_BASE_SERVICE));
        for key in keys_to_check {
            if let Some(value) = span.meta().get(key) {
                if !value.is_empty() {
                    peer_tags.push(MetaString::from(format!("{}:{}", key, value)));
                }
            }
        }

        peer_tags
    }

    fn peer_tag_keys_to_aggregate_for_span(&self, span_kind: &str, base_service: Option<&MetaString>) -> &[MetaString] {
        static EMPTY_PEER_TAGS: &[MetaString] = &[];
        static BASE_SERVICE_PEER_TAGS: &[MetaString] = &[MetaString::from_static(TAG_BASE_SERVICE)];

        if !self.peer_tags_aggregation || self.peer_tag_keys.is_empty() {
            return EMPTY_PEER_TAGS;
        }

        if (span_kind.is_empty() || span_kind.eq_ignore_ascii_case("internal"))
            && base_service.map(|s| !s.is_empty()).unwrap_or(false)
        {
            return BASE_SERVICE_PEER_TAGS;
        }

        if span_kind.eq_ignore_ascii_case("client")
            || span_kind.eq_ignore_ascii_case("producer")
            || span_kind.eq_ignore_ascii_case("consumer")
        {
            return &self.peer_tag_keys;
        }

        EMPTY_PEER_TAGS
    }

    fn add_span_internal(
        &mut self, s: &StatSpan, weight: f64, agg_key: &PayloadAggregationKey, tags: &InfraTags, origin: &str,
    ) {
        let end = s.start + s.duration;
        let btime = (end - end % self.bsize).max(self.oldest_ts);

        let b = self
            .buckets
            .entry(btime)
            .or_insert_with(|| RawBucket::new(btime, self.bsize));

        if tags.process_tags_hash != 0 && !tags.process_tags.is_empty() {
            b.set_process_tags(tags.process_tags_hash, tags.process_tags.clone());
        }
        if !tags.container_id.is_empty() && !tags.container_tags.is_empty() {
            b.set_container_tags(tags.container_id.clone(), tags.container_tags.clone());
        }

        b.handle_span(s, weight, origin, agg_key.clone());
    }
}

/// Align timestamp to bucket boundary.
#[inline]
fn align_ts(ts: u64, bsize: u64) -> u64 {
    ts - ts % bsize
}

pub const fn compute_stats_for_span_kind(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("server")
        || kind.eq_ignore_ascii_case("client")
        || kind.eq_ignore_ascii_case("producer")
        || kind.eq_ignore_ascii_case("consumer")
}

fn is_partial_snapshot(span: &Span) -> bool {
    match span.metrics().get(METRIC_PARTIAL_VERSION) {
        Some(&v) => v >= 0.0,
        None => false,
    }
}

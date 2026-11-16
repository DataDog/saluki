//! Span concentrator for APM stats computation.

use datadog_agent_apm::semantics::SemanticRegistryProvider;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::{
    event::{
        trace::{AttributeValue, Span, TraceIngestInfo},
        trace_stats::{ClientStatsBucket, ClientStatsPayload},
    },
    tags::TagSet,
};
use stringtheory::MetaString;

use super::aggregation::AggregationRegistry;
use super::aggregation::{
    get_grpc_status_code, get_status_code, process_tags_hash, PayloadAggregationKey, BUCKET_DURATION_NS,
    TAG_BASE_SERVICE, TAG_SPAN_KIND,
};
use super::peer_ip_quantize::quantize_peer_ip_addresses;
use super::peer_tags::PeerTagKeys;
use super::statsraw::RawBucket;

const DEFAULT_BUFFER_LEN: u64 = 2;
const METRIC_TOP_LEVEL: &str = "_top_level";
const METRIC_MEASURED: &str = "_dd.measured";
pub const METRIC_PARTIAL_VERSION: &str = "_dd.partial_version";

#[derive(Clone, Default)]
pub struct InfraTags {
    /// Container ID from the tracer payload.
    pub container_id: MetaString,
    /// Container tags resolved from the container runtime.
    pub container_tags: TagSet,
    /// Hash of the process tags string.
    pub process_tags_hash: u64,
    /// Process tags string from the tracer payload.
    pub process_tags: MetaString,
}

impl InfraTags {
    pub fn new(
        container_id: impl Into<MetaString>, container_tags: TagSet, process_tags: impl Into<MetaString>,
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

pub(crate) struct StatSpan {
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

    /// Peer tag key set for aggregation, derived from the semantic registry (base + custom)
    peer_tag_keys: PeerTagKeys,

    /// Operator-configured peer tags, kept so the key set can be rebuilt when the registry changes
    custom_peer_tags: Vec<MetaString>,

    /// Supplies embedded or remotely updated mappings from peer concepts to attribute names.
    registry: SemanticRegistryProvider,

    /// Bucket duration in nanoseconds (10 s)
    bsize: u64,

    /// Timestamp of oldest allowed bucket (prevents stats for already-flushed buckets)
    oldest_ts: u64,

    /// Number of buckets to buffer before flushing
    buffer_len: u64,

    /// Time-bucketed raw stats: `bucket_timestamp` -> RawBucket
    buckets: FastHashMap<u64, RawBucket>,

    /// Shared registry for aggregation keys.
    /// Keys are stored here and referenced by hash in individual buckets.
    key_registry: AggregationRegistry,
}

impl SpanConcentrator {
    /// Creates a concentrator that groups spans into time-bucketed APM stats.
    ///
    /// Peer tag names come from the provider's current semantic registry, combined with `custom_peer_tags`. Their
    /// values distinguish stats for different remote services or resources when `peer_tags_aggregation` is enabled.
    /// Call [`Self::refresh_registry`] before each input event buffer to pick up mapping changes.
    ///
    /// `now` is the current Unix timestamp in nanoseconds, used to establish the oldest accepted bucket.
    pub fn new(
        compute_stats_by_span_kind: bool, peer_tags_aggregation: bool, custom_peer_tags: &[MetaString],
        registry: SemanticRegistryProvider, now: u64,
    ) -> Self {
        let peer_tag_keys = PeerTagKeys::build(&registry.snapshot(), custom_peer_tags);

        Self {
            compute_stats_by_span_kind,
            peer_tags_aggregation,
            peer_tag_keys,
            custom_peer_tags: custom_peer_tags.to_vec(),
            registry,
            bsize: BUCKET_DURATION_NS,
            oldest_ts: align_ts(now, BUCKET_DURATION_NS),
            buffer_len: DEFAULT_BUFFER_LEN,
            buckets: FastHashMap::default(),
            key_registry: AggregationRegistry::new(),
        }
    }

    /// Builds the stat span of `span`, of a trace ingested through `ingest_info`, if it is eligible for stats.
    pub fn new_stat_span_from_span(&self, span: &Span, ingest_info: TraceIngestInfo) -> Option<StatSpan> {
        self.new_stat_span(span, span_kind(span, ingest_info))
    }

    /// Aggregates a span into the bucket its end time falls in, at `now` (Unix nanoseconds).
    pub fn add_span(
        &mut self, stat_span: &StatSpan, weight: f64, payload_key: &PayloadAggregationKey, infra_tags: &InfraTags,
        origin: &str, now: u64,
    ) {
        self.add_span_internal(stat_span, weight, payload_key, infra_tags, origin, now);
    }

    /// Updates the peer tag names used to group subsequent spans when the provider's registry changes.
    ///
    /// Compares registry fingerprints and rebuilds the key set only when they differ, preserving custom peer tags.
    /// Call before converting spans from each input event buffer, so all spans in the buffer use one key set.
    /// Already aggregated stats are unchanged.
    pub fn refresh_registry(&mut self) {
        self.peer_tag_keys
            .refresh(&self.registry.snapshot(), &self.custom_peer_tags);
    }

    pub fn flush(&mut self, now: u64, force: bool) -> Vec<ClientStatsPayload> {
        let mut m = FastHashMap::<PayloadAggregationKey, Vec<ClientStatsBucket>>::default();
        let mut container_tags_by_id = FastHashMap::<MetaString, TagSet>::default();
        let mut process_tags_by_hash = FastHashMap::<u64, MetaString>::default();

        let timestamps: Vec<u64> = self.buckets.keys().copied().collect();

        for ts in timestamps {
            if !force && ts + self.buffer_len * self.bsize > now {
                continue;
            }

            if let Some(bucket) = self.buckets.remove(&ts) {
                // Export bucket data, which also decrements reference counts for keys inline
                let exported = bucket.export(&mut self.key_registry);

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

    fn is_span_eligible(&self, span: &Span, span_kind: Option<&str>) -> bool {
        if span
            .attributes
            .get(METRIC_TOP_LEVEL)
            .and_then(AttributeValue::as_num)
            .is_some_and(|v| v == 1.0)
        {
            return true;
        }
        if span
            .attributes
            .get(METRIC_MEASURED)
            .and_then(AttributeValue::as_num)
            .is_some_and(|v| v == 1.0)
        {
            return true;
        }
        if self.compute_stats_by_span_kind {
            if let Some(kind) = span_kind {
                return compute_stats_for_span_kind(kind);
            }
        }
        false
    }

    fn new_stat_span(&self, span: &Span, span_kind: Option<MetaString>) -> Option<StatSpan> {
        if !self.is_span_eligible(span, span_kind.as_deref()) {
            return None;
        }

        if is_partial_snapshot(span) {
            return None;
        }

        let span_kind = span_kind.unwrap_or_default();
        let status_code = get_status_code(&span.attributes);
        let grpc_status_code = get_grpc_status_code(&span.attributes).to_metastring();
        let is_top_level = span
            .attributes
            .get(METRIC_TOP_LEVEL)
            .and_then(AttributeValue::as_num)
            .is_some_and(|v| v == 1.0);
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

        let base_service = span
            .attributes
            .get(TAG_BASE_SERVICE)
            .and_then(AttributeValue::as_string);
        let keys_to_check = self.peer_tag_keys_to_aggregate_for_span(span_kind, base_service);
        for key in keys_to_check {
            if let Some(value) = span.attributes.get(key.as_ref()).and_then(AttributeValue::as_string) {
                if !value.is_empty() {
                    // Quantize IP addresses before the value is hashed into the aggregation key, so
                    // that per-host cardinality collapses to a single dimension.
                    let value = quantize_peer_ip_addresses(value.as_ref());
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
            return self.peer_tag_keys.keys();
        }

        EMPTY_PEER_TAGS
    }

    fn add_span_internal(
        &mut self, s: &StatSpan, weight: f64, agg_key: &PayloadAggregationKey, tags: &InfraTags, origin: &str, now: u64,
    ) {
        let btime = bucket_time(s.start, s.duration, self.bsize, self.oldest_ts, now);

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

        b.handle_span(s, weight, origin, agg_key.clone(), &mut self.key_registry);
    }
}

/// Align timestamp to bucket boundary.
#[inline]
fn align_ts(ts: u64, bsize: u64) -> u64 {
    ts - ts % bsize
}

/// Returns the start of the bucket a span counts in, as the trace-agent's `addSpan` computes it.
///
/// The trace-agent's stat spans hold `int64` times, so a start past `i64::MAX` is negative there, and the end wraps
/// rather than overflows; `%` truncates toward zero in both languages. An end before the oldest bucket counts in the
/// oldest bucket, and an end in the future in the current one: a bucket ahead of the clock would only be flushed once
/// the clock reaches it. The result is never below the oldest bucket, which is never negative.
fn bucket_time(start: u64, duration: u64, bsize: u64, oldest_ts: u64, now: u64) -> u64 {
    let (bsize, oldest_ts, now) = (bsize as i64, oldest_ts as i64, now as i64);
    let end = (start as i64).wrapping_add(duration as i64);
    let mut btime = (end - end % bsize).max(oldest_ts);
    if btime > now {
        btime = (now - now % bsize).max(oldest_ts);
    }
    btime as u64
}

/// Returns the kind of a span, of a trace ingested through `ingest_info`.
///
/// Datadog traces use the span's kind field only, as the reference agent's `NewStatSpanFromV1` does through
/// `InternalSpan.SpanKind`: an unspecified or unrecognized kind is `unknown`, which no kind-based rule selects. OTLP
/// spans carry their kind as the `span.kind` attribute.
fn span_kind(span: &Span, ingest_info: TraceIngestInfo) -> Option<MetaString> {
    if let TraceIngestInfo::Otlp { .. } = ingest_info {
        return span
            .attributes
            .get(TAG_SPAN_KIND)
            .and_then(AttributeValue::as_string)
            .cloned();
    }
    let name = match span.kind {
        1 => "internal",
        2 => "server",
        3 => "client",
        4 => "producer",
        5 => "consumer",
        _ => "unknown",
    };
    Some(MetaString::from_static(name))
}

pub const fn compute_stats_for_span_kind(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("server")
        || kind.eq_ignore_ascii_case("client")
        || kind.eq_ignore_ascii_case("producer")
        || kind.eq_ignore_ascii_case("consumer")
}

fn is_partial_snapshot(span: &Span) -> bool {
    match span
        .attributes
        .get(METRIC_PARTIAL_VERSION)
        .and_then(AttributeValue::as_num)
    {
        Some(v) => v >= 0.0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    const OTLP: TraceIngestInfo = TraceIngestInfo::Otlp {
        receiver_sampling_rate: 1.0,
    };

    use datadog_agent_apm::semantics::test_util::TestRegistryPublisher;
    use datadog_agent_apm::semantics::{test_util::registry_json, Registry};

    use super::*;

    #[test]
    fn datadog_spans_use_the_kind_field_and_otlp_spans_the_attribute() {
        let mut attrs = FastHashMap::default();
        attrs.insert(
            MetaString::from(TAG_SPAN_KIND),
            AttributeValue::String(MetaString::from("client")),
        );
        let span = Span::default().with_attributes(attrs);
        let datadog = TraceIngestInfo::Datadog;

        // The attribute is ignored on a Datadog span: an unspecified kind is `unknown`.
        assert_eq!(span_kind(&span, datadog).as_deref(), Some("unknown"));
        assert_eq!(
            span_kind(&span.clone().with_kind(2), datadog).as_deref(),
            Some("server")
        );
        assert_eq!(
            span_kind(&Span::default().with_kind(9), datadog).as_deref(),
            Some("unknown")
        );

        // OTLP spans are unchanged: the attribute, whatever the field.
        assert_eq!(span_kind(&span.clone().with_kind(2), OTLP).as_deref(), Some("client"));
        assert_eq!(span_kind(&Span::default(), OTLP), None);
    }

    #[test]
    fn a_datadog_span_of_unspecified_kind_is_not_selected_by_kind() {
        let concentrator = SpanConcentrator::new(true, true, &[], Default::default(), 0);
        let mut attrs = FastHashMap::default();
        attrs.insert(
            MetaString::from(TAG_SPAN_KIND),
            AttributeValue::String(MetaString::from("client")),
        );
        let span = Span::default().with_attributes(attrs);

        assert!(concentrator
            .new_stat_span_from_span(&span, TraceIngestInfo::Datadog)
            .is_none());
        let otlp = concentrator
            .new_stat_span_from_span(&span, OTLP)
            .expect("eligible by kind");
        assert_eq!(otlp.span_kind, "client");

        let top_level = {
            let mut span = span.clone();
            span.attributes
                .insert(MetaString::from(METRIC_TOP_LEVEL), AttributeValue::Float(1.0));
            span
        };
        let datadog = concentrator
            .new_stat_span_from_span(&top_level, TraceIngestInfo::Datadog)
            .expect("eligible as top-level");
        assert_eq!(datadog.span_kind, "unknown");
        assert_eq!(
            super::super::aggregation::SpanKind::from_str(&datadog.span_kind).to_metastring(),
            "unknown"
        );
    }

    #[test]
    fn refresh_registry_rebuilds_peer_tag_keys_after_registry_change() {
        let (publisher, provider) = TestRegistryPublisher::new();
        let mut concentrator = SpanConcentrator::new(true, true, &[], provider, 0);
        assert!(!concentrator
            .peer_tag_keys
            .keys()
            .iter()
            .any(|k| k.as_ref() == "custom.remote.service"));

        let modified = registry_json(
            r#"{
                "peer.service": {
                    "fallbacks": [{"name": "peer.service", "provider": "otel", "type": "string"},
                                  {"name": "custom.remote.service", "provider": "otel", "type": "string"}]
                }
            }"#,
        );
        publisher.accept_registry(
            Registry::from_json(&modified).expect("modified registry should parse"),
            "modified",
        );
        concentrator.refresh_registry();

        let names: Vec<&str> = concentrator.peer_tag_keys.keys().iter().map(|k| k.as_ref()).collect();
        assert_eq!(names, ["custom.remote.service", "peer.service"]);
    }

    #[test]
    fn spans_whose_end_overflows_count_as_the_trace_agent_counts_them() {
        const B: u64 = BUCKET_DURATION_NS;
        let now = 1_000 * B + 3;
        let oldest = 998 * B;
        // `-1` to the trace-agent: the end is `9`, before the oldest bucket.
        assert_eq!(bucket_time(u64::MAX, 10, B, oldest, now), oldest);
        // The end wraps past `i64::MAX` to a negative time.
        assert_eq!(bucket_time(i64::MAX as u64 - 5, 10, B, oldest, now), oldest);
        // An end past `i64::MAX` without wrapping is negative too.
        assert_eq!(bucket_time(i64::MAX as u64 + 1, 0, B, oldest, now), oldest);
        // Near `i64::MAX` without wrapping, the end is in the future.
        assert_eq!(bucket_time(i64::MAX as u64 - 20, 10, B, oldest, now), 1_000 * B);
        assert_eq!(bucket_time(999 * B + 1, 10, B, oldest, now), 999 * B);

        // A `/v0.4` span with `start: -1` decodes to `u64::MAX`, and is aggregated rather than panicking.
        let mut concentrator = SpanConcentrator::new(true, true, &[], Default::default(), now);
        let measured = FastHashMap::from_iter([(MetaString::from("_dd.measured"), AttributeValue::Float(1.0))]);
        let span = Span::new("svc", "op", "res", "web", 1, 0, u64::MAX, 10, 0).with_attributes(measured);
        let stat_span = concentrator.new_stat_span_from_span(&span, OTLP).unwrap();
        concentrator.add_span(
            &stat_span,
            1.0,
            &PayloadAggregationKey::default(),
            &InfraTags::default(),
            "",
            now,
        );
        let buckets: Vec<_> = concentrator.buckets.keys().copied().collect();
        assert_eq!(buckets, [align_ts(now, B)]);
    }

    // `TestConcentratorFutureClamp` in `concentrator_test.go`.
    #[test]
    fn a_span_that_ends_in_the_future_counts_in_the_current_bucket() {
        let now = 1_000 * BUCKET_DURATION_NS + 3;
        let mut concentrator = SpanConcentrator::new(true, true, &[], Default::default(), now);
        let measured = FastHashMap::from_iter([(MetaString::from("_dd.measured"), AttributeValue::Float(1.0))]);
        let span = Span::new("svc", "op", "res", "web", 1, 0, now + 1_000 * BUCKET_DURATION_NS, 75, 0)
            .with_attributes(measured);
        let stat_span = concentrator.new_stat_span_from_span(&span, OTLP).unwrap();

        concentrator.add_span(
            &stat_span,
            1.0,
            &PayloadAggregationKey::default(),
            &InfraTags::default(),
            "",
            now,
        );

        let buckets: Vec<_> = concentrator.buckets.keys().copied().collect();
        assert_eq!(buckets, [1_000 * BUCKET_DURATION_NS]);
    }

    #[test]
    fn peer_tag_ips_quantize_into_one_aggregation_group() {
        let now = 1_000_000_000u64;
        let mut concentrator = SpanConcentrator::new(true, true, &[], Default::default(), now);

        let payload_key = PayloadAggregationKey {
            env: MetaString::from("test"),
            ..Default::default()
        };
        let infra_tags = InfraTags::default();

        // Two otherwise-identical client spans, differing only in the peer tag's IP address.
        // With quantization, both collapse to `blocked-ip-address` and aggregate into a single
        // group; if the quantize call in `matching_peer_tags` were removed, the raw IPs would
        // hash differently and split the stats into two groups of one hit each.
        for peer_ip in ["10.0.0.1:5432", "10.0.0.2:5432"] {
            let mut attrs = FastHashMap::default();
            attrs.insert(
                MetaString::from("span.kind"),
                AttributeValue::String(MetaString::from("client")),
            );
            attrs.insert(
                MetaString::from("db.instance"),
                AttributeValue::String(MetaString::from(peer_ip)),
            );
            attrs.insert(MetaString::from("_dd.measured"), AttributeValue::Float(1.0));
            let span =
                Span::new("myservice", "postgres.query", "SELECT ...", "db", 1, 0, now, 75, 0).with_attributes(attrs);

            let stat_span = concentrator
                .new_stat_span_from_span(&span, OTLP)
                .expect("client span with peer tags should produce stats");
            concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "", now);
        }

        let payloads = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);

        let mut matching_groups = Vec::new();
        for payload in &payloads {
            for bucket in payload.stats() {
                for grouped in bucket.stats() {
                    if grouped.resource() == "SELECT ..." {
                        matching_groups.push(grouped);
                    }
                }
            }
        }

        assert_eq!(
            matching_groups.len(),
            1,
            "spans with different peer IPs must group after quantization"
        );
        assert_eq!(
            matching_groups[0].hits(),
            2,
            "both spans' hits must combine into the single group"
        );

        let peer_tags: Vec<&str> = matching_groups[0].peer_tags().iter().map(|t| t.as_ref()).collect();
        assert!(
            peer_tags.contains(&"db.instance:blocked-ip-address:5432"),
            "peer tags must carry the quantized value, got: {:?}",
            peer_tags
        );
        assert!(
            peer_tags.iter().all(|t| !t.contains("10.0.0.")),
            "no peer tag may retain the raw IP, got: {:?}",
            peer_tags
        );
    }
}

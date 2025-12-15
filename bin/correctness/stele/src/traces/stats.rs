use std::hash::{Hash as _, Hasher as _};

use datadog_protos::traces::{self as proto, Trilean};
use saluki_common::{collections::FastHashMap, hash::StableHasher};
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentMetadata {
    hostname: MetaString,
    env: MetaString,
    agent_version: MetaString,
    client_computed: bool,
    split_payload: bool,
}

impl From<&proto::StatsPayload> for AgentMetadata {
    fn from(payload: &proto::StatsPayload) -> Self {
        Self {
            hostname: (*payload.agentHostname).into(),
            env: (*payload.agentEnv).into(),
            agent_version: (*payload.agentVersion).into(),
            client_computed: payload.clientComputed,
            split_payload: payload.splitPayload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TracerMetadata {
    hostname: MetaString,
    env: MetaString,
    version: MetaString,
    service: MetaString,
    language: MetaString,
    tracer_version: MetaString,
    runtime_id: MetaString,
    sequence: u64,
    agent_aggregation: MetaString,
    container_id: MetaString,
    tags: Vec<MetaString>,
    git_commit_sha: MetaString,
    image_tag: MetaString,
    process_tags_hash: u64,
    process_tags: MetaString,
}

impl From<&proto::ClientStatsPayload> for TracerMetadata {
    fn from(payload: &proto::ClientStatsPayload) -> Self {
        Self {
            hostname: (*payload.hostname).into(),
            env: (*payload.env).into(),
            version: (*payload.version).into(),
            service: (*payload.service).into(),
            language: (*payload.lang).into(),
            tracer_version: (*payload.tracerVersion).into(),
            runtime_id: (*payload.runtimeID).into(),
            sequence: payload.sequence,
            agent_aggregation: (*payload.agentAggregation).into(),
            container_id: (*payload.containerID).into(),
            tags: payload.tags.iter().map(|t| MetaString::from(&**t)).collect(),
            git_commit_sha: (*payload.git_commit_sha).into(),
            image_tag: (*payload.image_tag).into(),
            process_tags_hash: payload.process_tags_hash,
            process_tags: (*payload.process_tags).into(),
        }
    }
}

/// Time frame covered by a bucket.
#[derive(Clone, Copy, Debug, Deserialize, Hash, Eq, PartialEq, Serialize)]
pub struct BucketTimeframe {
    /// Start time of the bucket, in nanoseconds.
    pub start_time_ns: u64,

    /// Width of the bucket, in nanoseconds.
    pub duration_ns: u64,
}

impl From<&proto::ClientStatsBucket> for BucketTimeframe {
    fn from(bucket: &proto::ClientStatsBucket) -> Self {
        Self {
            start_time_ns: bucket.start,
            duration_ns: bucket.duration,
        }
    }
}

/// Client statistics grouped by time frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BucketedClientStatistics {
    agent_metadata: AgentMetadata,
    tracer_metadata: TracerMetadata,
    buckets: FastHashMap<BucketTimeframe, Vec<ClientStatistics>>,
}

impl BucketedClientStatistics {
    /// Merge the given stats into the given bucket, creating the bucket if it does not already exist.
    pub fn merge(&mut self, bucket_timeframe: BucketTimeframe, stats: ClientStatistics) {
        let bucket = self.buckets.entry(bucket_timeframe).or_default();
        bucket.push(stats);
    }

    /// Returns an iterator over each bucket.
    pub fn buckets(&self) -> impl Iterator<Item = (&BucketTimeframe, &[ClientStatistics])> {
        self.buckets.iter().map(|(key, value)| (key, value.as_slice()))
    }
}

/// Client statistics for a given span.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientStatistics {
    service: MetaString,
    name: MetaString,
    resource: MetaString,
    type_: MetaString,
    hits: u64,
    errors: u64,
    duration_ns: u64,
    // TODO: decode these to native DDSketch
    ok_summary: Vec<u8>,
    // TODO: decode these to native DDSketch
    error_summary: Vec<u8>,
    synthetics: bool,
    top_level_hits: u64,
    span_kind: MetaString,
    peer_tags: Vec<MetaString>,
    is_trace_root: Option<bool>,
    db_type: MetaString,
    grpc_status_code: MetaString,
    http_status_code: u32,
    http_method: MetaString,
    http_endpoint: MetaString,
}

impl From<&proto::ClientGroupedStats> for ClientStatistics {
    fn from(payload: &proto::ClientGroupedStats) -> Self {
        let is_trace_root = match payload.is_trace_root.enum_value() {
            Ok(Trilean::NOT_SET) => None,
            Ok(Trilean::TRUE) => Some(true),
            Ok(Trilean::FALSE) => Some(false),
            Err(_) => None,
        };

        Self {
            service: (*payload.service).into(),
            name: (*payload.name).into(),
            resource: (*payload.resource).into(),
            type_: (*payload.type_).into(),
            hits: payload.hits,
            errors: payload.errors,
            duration_ns: payload.duration,
            ok_summary: payload.okSummary.to_vec(),
            error_summary: payload.errorSummary.to_vec(),
            synthetics: payload.synthetics,
            top_level_hits: payload.topLevelHits,
            span_kind: (*payload.span_kind).into(),
            peer_tags: payload.peer_tags.iter().map(|t| MetaString::from(&**t)).collect(),
            is_trace_root,
            db_type: (*payload.DB_type).into(),
            grpc_status_code: (*payload.GRPC_status_code).into(),
            http_status_code: payload.HTTP_status_code,
            http_method: (*payload.HTTP_method).into(),
            http_endpoint: (*payload.HTTP_endpoint).into(),
        }
    }
}

/// Aggregation key for client statistics.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct AggregationKey(u64);

impl From<&proto::ClientGroupedStats> for AggregationKey {
    fn from(payload: &proto::ClientGroupedStats) -> Self {
        // We manually hash the various fields to come up with our aggregation key.
        //
        // TODO: This follows the logic in `libdd-trace-stats` and it would be nice to eventually converge on using
        // that code directly, but there's a number of changes that would need to be made upstream in order to make
        // doing so possible.
        let mut hasher = StableHasher::default();

        payload.resource.hash(&mut hasher);
        payload.service.hash(&mut hasher);
        payload.name.hash(&mut hasher);
        payload.type_.hash(&mut hasher);
        payload.span_kind.hash(&mut hasher);
        payload.HTTP_status_code.hash(&mut hasher);
        payload.synthetics.hash(&mut hasher);

        // TODO: technically, peer tags should only be included if they match the _configured_ peer keys to aggregate on
        // so we're doing this wrong but we can iterate on it later
        payload.peer_tags.hash(&mut hasher);

        payload.is_trace_root.value().hash(&mut hasher);
        payload.HTTP_method.hash(&mut hasher);
        payload.HTTP_endpoint.hash(&mut hasher);

        Self(hasher.finish())
    }
}

/// Aggregator for client statistics.
///
/// Client statistics are aggregated by a number of fields that generate correspond to a specific span: service, name,
/// and operation. Additional fields are used to further group the statistics, such as response codes and tags. This is
/// referred to as the "aggregation key".
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClientStatisticsAggregator {
    groups: FastHashMap<AggregationKey, BucketedClientStatistics>,
}

impl ClientStatisticsAggregator {
    /// Merges the given payload into the aggregator.
    pub fn merge_payload(&mut self, payload: &proto::StatsPayload) -> Result<(), GenericError> {
        let agent_metadata = AgentMetadata::from(payload);
        for client_stats_payload in payload.stats() {
            let tracer_metadata = TracerMetadata::from(client_stats_payload);

            for stats_bucket in client_stats_payload.stats() {
                let bucket_timeframe = BucketTimeframe::from(stats_bucket);

                for grouped_stat in stats_bucket.stats() {
                    let aggregation_key = AggregationKey::from(grouped_stat);
                    let stats_group = self
                        .groups
                        .entry(aggregation_key)
                        .or_insert_with(|| BucketedClientStatistics {
                            agent_metadata: agent_metadata.clone(),
                            tracer_metadata: tracer_metadata.clone(),
                            buckets: FastHashMap::default(),
                        });

                    if stats_group.agent_metadata != agent_metadata {
                        return Err(generic_error!("agent metadata mismatch"));
                    }
                    if stats_group.tracer_metadata != tracer_metadata {
                        return Err(generic_error!("tracer metadata mismatch"));
                    }

                    stats_group.merge(bucket_timeframe, ClientStatistics::from(grouped_stat));
                }
            }
        }

        Ok(())
    }

    /// Returns an iterator over each group of statistics.
    ///
    /// Groups are split by "aggregation key", which is a combination of select fields in each client stats payload,
    /// roughly corresponding to a specific span: name, operation, kind, tags, and so on.
    pub fn groups(&self) -> impl Iterator<Item = (&AggregationKey, &BucketedClientStatistics)> {
        self.groups.iter()
    }
}

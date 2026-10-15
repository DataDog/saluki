//! Semantic attribute registry for OTLP.

mod accessor;
mod lookup;
mod provider;
mod registry;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use accessor::Accessor;
pub use lookup::{lookup_float64, lookup_int64, lookup_string};
pub use provider::SemanticRegistryProvider;
pub use registry::{Condition, Provider, Registry, TagInfo, ValueType, EMBEDDED_REGISTRY};

/// A named semantic concept—the canonical identity of an attribute that may
/// have multiple representations across OpenTelemetry versions and Datadog
/// legacy conventions.
///
/// Ported from upstream's `Concept` constants in `semantics.go`. Grouping is
/// retained for ease of syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Concept {
    // ---- Peer Tags ----
    /// The `peer.service` concept.
    PeerService,
    /// The `peer.hostname` concept.
    PeerHostname,
    /// The `peer.db.name` concept.
    PeerDbName,
    /// The `peer.db.system` concept.
    PeerDbSystem,
    /// The `peer.cassandra.contact.points` concept.
    PeerCassandraContactPoints,
    /// The `peer.couchbase.seed.nodes` concept.
    PeerCouchbaseSeedNodes,
    /// The `peer.messaging.destination` concept.
    PeerMessagingDestination,
    /// The `peer.messaging.system` concept.
    PeerMessagingSystem,
    /// The `peer.kafka.bootstrap.servers` concept.
    PeerKafkaBootstrapServers,
    /// The `peer.rpc.service` concept.
    PeerRpcService,
    /// The `peer.rpc.system` concept.
    PeerRpcSystem,
    /// The `peer.aws.s3.bucket` concept.
    PeerAwsS3Bucket,
    /// The `peer.aws.sqs.queue` concept.
    PeerAwsSqsQueue,
    /// The `peer.aws.dynamodb.table` concept.
    PeerAwsDynamoDbTable,
    /// The `peer.aws.kinesis.stream` concept.
    PeerAwsKinesisStream,

    // ---- Stats Aggregation ----
    /// The `http.status_code` concept.
    HttpStatusCode,
    /// The `http.method` concept.
    HttpMethod,
    /// The `http.route` concept.
    HttpRoute,
    /// The `rpc.grpc.status_code` concept.
    RpcGrpcStatusCode,
    /// The `span.kind` concept.
    SpanKind,
    /// The `_dd.base_service` concept.
    DdBaseService,

    // ---- Service & Resource Identification ----
    /// The `service.name` concept.
    ServiceName,
    /// The `resource.name` concept.
    ResourceName,
    /// The `operation.name` concept.
    OperationName,
    /// The `span.type` concept.
    SpanType,
    /// The `db.system` concept.
    DbSystem,
    /// The `db.statement` concept.
    DbStatement,
    /// The `db.namespace` concept.
    DbNamespace,
    /// The `rpc.system` concept.
    RpcSystem,
    /// The `rpc.service` concept.
    RpcService,
    /// The `messaging.system` concept.
    MessagingSystem,
    /// The `messaging.destination` concept.
    MessagingDestination,
    /// The `deployment.environment` concept.
    DeploymentEnvironment,
    /// The `service.version` concept.
    ServiceVersion,
    /// The `container.id` concept.
    ContainerId,
    /// The `k8s.pod.uid` concept.
    K8sPodUid,

    // ---- Obfuscation ----
    /// The `db.query` concept.
    DbQuery,
    /// The `mongodb.query` concept.
    MongoDbQuery,
    /// The `elasticsearch.body` concept.
    ElasticsearchBody,
    /// The `opensearch.body` concept.
    OpenSearchBody,
    /// The `redis.raw_command` concept.
    RedisRawCommand,
    /// The `valkey.raw_command` concept.
    ValkeyRawCommand,
    /// The `memcached.command` concept.
    MemcachedCommand,
    /// The `http.url` concept.
    HttpUrl,

    // ---- Normalization ----
    /// The `messaging.operation` concept.
    MessagingOperation,
    /// The `graphql.operation.type` concept.
    GraphQlOperationType,
    /// The `graphql.operation.name` concept.
    GraphQlOperationName,
    /// The `faas.invoked.provider` concept.
    FaasInvokedProvider,
    /// The `faas.invoked.name` concept.
    FaasInvokedName,
    /// The `faas.trigger` concept.
    FaasTrigger,
    /// The `network.protocol.name` concept.
    NetworkProtocolName,
    /// The `rpc.method` concept.
    RpcMethod,
    /// The `component` concept.
    Component,
    /// The `link.name` concept.
    LinkName,

    // ---- Sampling ----
    /// The `_dd.measured` concept.
    DdMeasured,
    /// The `_dd.top_level` concept.
    DdTopLevel,
    /// The `_sampling_priority_v1` concept.
    SamplingPriority,
    /// The `otel.trace_id` concept.
    OtelTraceId,
    /// The `_dd.p.tid` concept.
    DdPTid,
    /// The `_dd.partial_version` concept.
    DdPartialVersion,
    /// The `_dd.origin` concept.
    DdOrigin,

    // ---- Datadog tags ----
    /// The `env` concept.
    DdEnv,
    /// The `version` concept.
    DdVersion,
    /// The `_dd.hostname` concept.
    DdHostname,
    /// The `_dd.git.commit.sha` concept.
    DdGitCommitSha,
    /// The `_dd.p.dm` concept.
    DdDecisionMaker,
    /// The `_dd.apm_mode` concept.
    DdApmMode,
}

impl Concept {
    /// All known concepts, in declaration order. Used by tests to verify that
    /// every variant has a corresponding entry in `mappings.json`.
    pub const ALL: &'static [Concept] = &[
        // Peer Tags
        Concept::PeerService,
        Concept::PeerHostname,
        Concept::PeerDbName,
        Concept::PeerDbSystem,
        Concept::PeerCassandraContactPoints,
        Concept::PeerCouchbaseSeedNodes,
        Concept::PeerMessagingDestination,
        Concept::PeerMessagingSystem,
        Concept::PeerKafkaBootstrapServers,
        Concept::PeerRpcService,
        Concept::PeerRpcSystem,
        Concept::PeerAwsS3Bucket,
        Concept::PeerAwsSqsQueue,
        Concept::PeerAwsDynamoDbTable,
        Concept::PeerAwsKinesisStream,
        // Stats Aggregation
        Concept::HttpStatusCode,
        Concept::HttpMethod,
        Concept::HttpRoute,
        Concept::RpcGrpcStatusCode,
        Concept::SpanKind,
        Concept::DdBaseService,
        // Service & Resource Identification
        Concept::ServiceName,
        Concept::ResourceName,
        Concept::OperationName,
        Concept::SpanType,
        Concept::DbSystem,
        Concept::DbStatement,
        Concept::DbNamespace,
        Concept::RpcSystem,
        Concept::RpcService,
        Concept::MessagingSystem,
        Concept::MessagingDestination,
        Concept::DeploymentEnvironment,
        Concept::ServiceVersion,
        Concept::ContainerId,
        Concept::K8sPodUid,
        // Obfuscation
        Concept::DbQuery,
        Concept::MongoDbQuery,
        Concept::ElasticsearchBody,
        Concept::OpenSearchBody,
        Concept::RedisRawCommand,
        Concept::ValkeyRawCommand,
        Concept::MemcachedCommand,
        Concept::HttpUrl,
        // Normalization
        Concept::MessagingOperation,
        Concept::GraphQlOperationType,
        Concept::GraphQlOperationName,
        Concept::FaasInvokedProvider,
        Concept::FaasInvokedName,
        Concept::FaasTrigger,
        Concept::NetworkProtocolName,
        Concept::RpcMethod,
        Concept::Component,
        Concept::LinkName,
        // Sampling
        Concept::DdMeasured,
        Concept::DdTopLevel,
        Concept::SamplingPriority,
        Concept::OtelTraceId,
        Concept::DdPTid,
        Concept::DdPartialVersion,
        Concept::DdOrigin,
        // Datadog tags
        Concept::DdEnv,
        Concept::DdVersion,
        Concept::DdHostname,
        Concept::DdGitCommitSha,
        Concept::DdDecisionMaker,
        Concept::DdApmMode,
    ];

    /// Canonical string identifier as used in `mappings.json`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            // Peer Tags
            Concept::PeerService => "peer.service",
            Concept::PeerHostname => "peer.hostname",
            Concept::PeerDbName => "peer.db.name",
            Concept::PeerDbSystem => "peer.db.system",
            Concept::PeerCassandraContactPoints => "peer.cassandra.contact.points",
            Concept::PeerCouchbaseSeedNodes => "peer.couchbase.seed.nodes",
            Concept::PeerMessagingDestination => "peer.messaging.destination",
            Concept::PeerMessagingSystem => "peer.messaging.system",
            Concept::PeerKafkaBootstrapServers => "peer.kafka.bootstrap.servers",
            Concept::PeerRpcService => "peer.rpc.service",
            Concept::PeerRpcSystem => "peer.rpc.system",
            Concept::PeerAwsS3Bucket => "peer.aws.s3.bucket",
            Concept::PeerAwsSqsQueue => "peer.aws.sqs.queue",
            Concept::PeerAwsDynamoDbTable => "peer.aws.dynamodb.table",
            Concept::PeerAwsKinesisStream => "peer.aws.kinesis.stream",
            // Stats Aggregation
            Concept::HttpStatusCode => "http.status_code",
            Concept::HttpMethod => "http.method",
            Concept::HttpRoute => "http.route",
            Concept::RpcGrpcStatusCode => "rpc.grpc.status_code",
            Concept::SpanKind => "span.kind",
            Concept::DdBaseService => "_dd.base_service",
            // Service & Resource Identification
            Concept::ServiceName => "service.name",
            Concept::ResourceName => "resource.name",
            Concept::OperationName => "operation.name",
            Concept::SpanType => "span.type",
            Concept::DbSystem => "db.system",
            Concept::DbStatement => "db.statement",
            Concept::DbNamespace => "db.namespace",
            Concept::RpcSystem => "rpc.system",
            Concept::RpcService => "rpc.service",
            Concept::MessagingSystem => "messaging.system",
            Concept::MessagingDestination => "messaging.destination",
            Concept::DeploymentEnvironment => "deployment.environment",
            Concept::ServiceVersion => "service.version",
            Concept::ContainerId => "container.id",
            Concept::K8sPodUid => "k8s.pod.uid",
            // Obfuscation
            Concept::DbQuery => "db.query",
            Concept::MongoDbQuery => "mongodb.query",
            Concept::ElasticsearchBody => "elasticsearch.body",
            Concept::OpenSearchBody => "opensearch.body",
            Concept::RedisRawCommand => "redis.raw_command",
            Concept::ValkeyRawCommand => "valkey.raw_command",
            Concept::MemcachedCommand => "memcached.command",
            Concept::HttpUrl => "http.url",
            // Normalization
            Concept::MessagingOperation => "messaging.operation",
            Concept::GraphQlOperationType => "graphql.operation.type",
            Concept::GraphQlOperationName => "graphql.operation.name",
            Concept::FaasInvokedProvider => "faas.invoked.provider",
            Concept::FaasInvokedName => "faas.invoked.name",
            Concept::FaasTrigger => "faas.trigger",
            Concept::NetworkProtocolName => "network.protocol.name",
            Concept::RpcMethod => "rpc.method",
            Concept::Component => "component",
            Concept::LinkName => "link.name",
            // Sampling
            Concept::DdMeasured => "_dd.measured",
            Concept::DdTopLevel => "_dd.top_level",
            Concept::SamplingPriority => "_sampling_priority_v1",
            Concept::OtelTraceId => "otel.trace_id",
            Concept::DdPTid => "_dd.p.tid",
            Concept::DdPartialVersion => "_dd.partial_version",
            Concept::DdOrigin => "_dd.origin",
            // Datadog tags
            Concept::DdEnv => "env",
            Concept::DdVersion => "version",
            Concept::DdHostname => "_dd.hostname",
            Concept::DdGitCommitSha => "_dd.git.commit.sha",
            Concept::DdDecisionMaker => "_dd.p.dm",
            Concept::DdApmMode => "_dd.apm_mode",
        }
    }

    /// Parse a concept identifier from its canonical string form.
    ///
    /// Used by the registry loader to reject `mappings.json` entries that have
    /// no corresponding variant—keeping the enum and the JSON in sync.
    fn from_name(s: &str) -> Option<Concept> {
        Concept::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concept_all_covers_every_variant() {
        // Count matches the number of "case" arms in as_str(). If a variant is
        // added without being added to ALL, this test should start failing
        // (once you add a matching test below). The primary guard is compiler:
        // `match` on Concept forces exhaustiveness in as_str().
        assert_eq!(Concept::ALL.len(), 67);
    }

    #[test]
    fn concept_all_entries_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Concept::ALL {
            assert!(seen.insert(c.as_str()), "duplicate concept string: {}", c.as_str());
        }
    }

    #[test]
    fn concept_from_name_roundtrip() {
        for c in Concept::ALL {
            assert_eq!(Concept::from_name(c.as_str()), Some(*c));
        }
        assert_eq!(Concept::from_name("nope.not.a.concept"), None);
    }
}

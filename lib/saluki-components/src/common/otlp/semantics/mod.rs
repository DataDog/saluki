//! Semantic attribute registry for OTLP.
#![allow(dead_code, unused_imports)]

pub mod accessor;
pub mod lookup;
pub mod registry;

pub use accessor::{Accessor, OtelSpanAccessor, OtlpAttributesAccessor};
pub use lookup::{lookup_float64, lookup_int64, lookup_string};
pub use registry::{Registry, REGISTRY};

/// A named semantic concept — the canonical identity of an attribute that may
/// have multiple representations across OpenTelemetry versions and Datadog
/// legacy conventions.
///
/// Ported from upstream's `Concept` constants in `semantics.go`. Grouping is
/// retained for ease of syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Concept {
    // ---- Peer Tags ----
    PeerService,
    PeerHostname,
    PeerDbName,
    PeerDbSystem,
    PeerCassandraContactPoints,
    PeerCouchbaseSeedNodes,
    PeerMessagingDestination,
    PeerMessagingSystem,
    PeerKafkaBootstrapServers,
    PeerRpcService,
    PeerRpcSystem,
    PeerAwsS3Bucket,
    PeerAwsSqsQueue,
    PeerAwsDynamoDbTable,
    PeerAwsKinesisStream,

    // ---- Stats Aggregation ----
    HttpStatusCode,
    HttpMethod,
    HttpRoute,
    RpcGrpcStatusCode,
    SpanKind,
    DdBaseService,

    // ---- Service & Resource Identification ----
    ServiceName,
    ResourceName,
    OperationName,
    SpanType,
    DbSystem,
    DbStatement,
    DbNamespace,
    RpcSystem,
    RpcService,
    MessagingSystem,
    MessagingDestination,
    DeploymentEnvironment,
    ServiceVersion,
    ContainerId,
    K8sPodUid,

    // ---- Obfuscation ----
    DbQuery,
    MongoDbQuery,
    ElasticsearchBody,
    OpenSearchBody,
    RedisRawCommand,
    ValkeyRawCommand,
    MemcachedCommand,
    HttpUrl,

    // ---- Normalization ----
    MessagingOperation,
    GraphQlOperationType,
    GraphQlOperationName,
    FaasInvokedProvider,
    FaasInvokedName,
    FaasTrigger,
    NetworkProtocolName,
    RpcMethod,
    Component,
    LinkName,

    // ---- Sampling ----
    DdMeasured,
    DdTopLevel,
    SamplingPriority,
    OtelTraceId,
    DdPTid,
    DdPartialVersion,
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
        }
    }

    /// Parse a concept identifier from its canonical string form.
    ///
    /// Used by the registry loader to reject `mappings.json` entries that have
    /// no corresponding variant — keeping the enum and the JSON in sync.
    pub fn from_str(s: &str) -> Option<Concept> {
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
        assert_eq!(Concept::ALL.len(), 60);
    }

    #[test]
    fn concept_all_entries_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Concept::ALL {
            assert!(seen.insert(c.as_str()), "duplicate concept string: {}", c.as_str());
        }
    }

    #[test]
    fn concept_from_str_roundtrip() {
        for c in Concept::ALL {
            assert_eq!(Concept::from_str(c.as_str()), Some(*c));
        }
        assert_eq!(Concept::from_str("nope.not.a.concept"), None);
    }
}

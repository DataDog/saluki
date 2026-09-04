//! Peer tag key set derivation from the semantic registry.
//!
//! Peer tags identify the remote endpoint a span talked to, and their values are hashed into the
//! stats aggregation key in the order of the configured key list. The snapshot also records the
//! registry's content hash so it can be rebuilt when the registry changes.

use std::sync::Arc;

use saluki_common::collections::FastHashSet;
use stringtheory::MetaString;

use crate::common::otlp::semantics::{Concept, Registry};

/// The peer concepts whose precedence lists make up the base key set.
const PEER_CONCEPTS: &[Concept] = &[
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
    Concept::DdBaseService,
];

/// A snapshot of the peer tag key set, pinned to a registry content hash.
///
/// The key list is `Arc`-backed, so snapshots clone cheaply and can be shared. Whether the snapshot
/// is stale is answered with a single `u64` comparison against the live registry's content hash.
#[derive(Clone)]
pub(crate) struct PeerTagKeys {
    content_hash: u64,
    keys: Arc<[MetaString]>,
}

impl PeerTagKeys {
    /// Builds the key set from the registry's peer concept precedence lists, plus operator-configured tags.
    ///
    /// The result is sorted and deduplicated, which makes the key order canonical regardless of
    /// registry iteration order or the order of the configured tags.
    pub(crate) fn build(registry: &Registry, custom_peer_tags: &[MetaString]) -> Self {
        let mut keys = base_peer_tag_keys(registry);
        keys.extend(custom_peer_tags.iter().cloned());
        keys.sort_unstable();
        keys.dedup();

        Self {
            content_hash: registry.content_hash(),
            keys: keys.into(),
        }
    }

    /// Rebuilds the key set if the registry content hash changed since this snapshot was taken.
    ///
    /// Returns `true` when a rebuild happened, `false` when the snapshot was already current. The
    /// comparison is a single integer check, so calling this on every flush cycle is effectively
    /// free; only an actual registry change pays the cost of re-deriving the key set.
    pub(crate) fn refresh(&mut self, registry: &Registry, custom_peer_tags: &[MetaString]) -> bool {
        if self.content_hash == registry.content_hash() {
            return false;
        }

        *self = Self::build(registry, custom_peer_tags);
        true
    }

    /// Returns the snapshot's peer tag keys, in canonical (sorted, deduplicated) order.
    pub(crate) fn keys(&self) -> &[MetaString] {
        &self.keys
    }

    /// Returns `true` if the key set is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Collects every attribute name from every peer concept's precedence list.
///
/// A concept maps to several attributes ordered by precedence; for key-set purposes, all of the
/// names are collected since the span's raw attributes are keyed by name, not by concept.
fn base_peer_tag_keys(registry: &Registry) -> Vec<MetaString> {
    let mut names = FastHashSet::default();
    for concept in PEER_CONCEPTS {
        if let Some(fallbacks) = registry.get_attribute_precedence(*concept) {
            names.extend(fallbacks.iter().map(|tag| tag.name.as_str()));
        }
    }
    names.into_iter().map(MetaString::from).collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::common::otlp::semantics::REGISTRY;

    /// The base key set derived from the embedded registry, pinned so that registry or derivation
    /// drift surfaces as a test failure instead of as split stats aggregates in mixed clusters.
    ///
    /// This is the legacy `peer_tags.ini` list plus `db.system.name`, which the registry's
    /// `peer.db.system` precedence list includes, matching upstream's registry-derived behavior.
    const EXPECTED_BASE_KEYS: &[&str] = &[
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
        "db.system.name",
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

    #[test]
    fn base_keys_match_upstream_peer_tags() {
        let keys = PeerTagKeys::build(&REGISTRY, &[]);
        assert_eq!(
            keys.keys().iter().map(|k| k.as_ref()).collect::<Vec<&str>>(),
            EXPECTED_BASE_KEYS,
            "base peer tag keys must match the pinned upstream list"
        );
    }

    #[test]
    fn property_test_keys_are_sorted_and_deduplicated() {
        proptest!(|(custom_tags in proptest::collection::vec("[a-z_]{1,12}", 0..16))| {
            let custom: Vec<MetaString> =
                custom_tags.iter().map(|t: &String| MetaString::from(t.as_str())).collect();
            let keys = PeerTagKeys::build(&REGISTRY, &custom);

            for window in keys.keys().windows(2) {
                prop_assert!(window[0] < window[1], "keys must be strictly sorted");
            }
        });
    }

    #[test]
    fn custom_tags_are_appended_and_deduplicated() {
        let custom = vec![
            MetaString::from("my.custom.peer.tag"),
            // A duplicate of a base key: must not appear twice.
            MetaString::from("db.name"),
            // A duplicate within the custom list itself.
            MetaString::from("my.custom.peer.tag"),
        ];
        let keys = PeerTagKeys::build(&REGISTRY, &custom);
        let names: Vec<&str> = keys.keys().iter().map(|k| k.as_ref()).collect();

        assert!(names.contains(&"my.custom.peer.tag"));
        assert_eq!(names.iter().filter(|n| **n == "my.custom.peer.tag").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "db.name").count(), 1);
    }

    #[test]
    fn refresh_is_a_noop_when_registry_is_unchanged() {
        let mut keys = PeerTagKeys::build(&REGISTRY, &[]);
        assert!(!keys.refresh(&REGISTRY, &[]));
    }

    #[test]
    fn refresh_rebuilds_when_registry_content_changes() {
        let mut keys = PeerTagKeys::build(&REGISTRY, &[]);

        // A modified registry: one new attribute in the `peer.service` precedence list.
        let modified = r#"{
            "version": "modified",
            "concepts": {
                "peer.service": {
                    "fallbacks": [{"name": "peer.service", "provider": "otel", "type": "string"},
                                  {"name": "custom.remote.service", "provider": "otel", "type": "string"}]
                }
            }
        }"#;
        let registry = Registry::from_json(modified).expect("modified registry should parse");

        assert!(keys.refresh(&registry, &[]), "content change must trigger a rebuild");

        let names: Vec<&str> = keys.keys().iter().map(|k| k.as_ref()).collect();
        assert_eq!(names, ["custom.remote.service", "peer.service"]);
        assert!(
            !keys.refresh(&registry, &[]),
            "a second refresh with no change must be a no-op"
        );
    }

    #[test]
    fn build_with_empty_concepts_yields_empty_keys() {
        let empty = r#"{"concepts": {}}"#;
        let registry = Registry::from_json(empty).expect("empty registry should parse");
        let keys = PeerTagKeys::build(&registry, &[]);
        assert!(keys.is_empty());
    }
}

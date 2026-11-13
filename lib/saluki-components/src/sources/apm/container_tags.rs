//! `Datadog-Container-Tags-Hash`, which the trace-agent adds to its `/info` response.
//!
//! The header hashes the tags that identify the service of the tracer's container, so that a tracer can tell when its
//! container's identity changes. The trace-agent finds the container as its ID provider does, and reads its tags from
//! the tagger at high cardinality.

use std::sync::Arc;

use axum::extract::ConnectInfo;
use http::{HeaderMap, HeaderName, HeaderValue};
use saluki_core::data_model::origin::{OriginTagCardinality, RawOrigin};
use saluki_env::workload::{CaptureEntityResolver, EntityId, WorkloadProvider};
use saluki_io::net::ConnectionAddress;
use sha2::{Digest as _, Sha256};

use super::{CONTAINER_ID_HEADER, ENTITY_ID_HEADER};

/// Header carrying the hash of the tracer container's service tags.
pub(super) const CONTAINER_TAGS_HASH_HEADER: HeaderName = HeaderName::from_static("datadog-container-tags-hash");

/// Header a tracer sends its External Data in, which identifies its pod and container in Kubernetes.
const EXTERNAL_DATA_HEADER: HeaderName = HeaderName::from_static("datadog-external-env");

/// Tag keys that identify the service of a container (`serviceOriginTags`).
const SERVICE_ORIGIN_TAGS: &[&str] = &[
    "kube_deployment",
    "kube_cronjob",
    "kube_container_name",
    "kube_namespace",
    "kube_app_name",
    "kube_app_managed_by",
    "service",
    "short_image",
    "kube_cluster_name",
];

/// Resolves the container of a tracer and hashes its service tags.
#[derive(Clone, Default)]
pub(super) struct ContainerTagsHasher {
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl ContainerTagsHasher {
    pub(super) fn new(
        capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
        workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    ) -> Self {
        Self {
            capture_entity_resolver,
            workload_provider,
        }
    }

    /// Returns the `Datadog-Container-Tags-Hash` of the tracer that sent a request with `headers`, over `connection`.
    ///
    /// As in the trace-agent, a tracer whose container isn't found gets the hash of no tags.
    pub(super) fn header_value(
        &self, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
    ) -> HeaderValue {
        let tags = self.container_entity(headers, connection).and_then(|entity_id| {
            self.workload_provider
                .as_ref()?
                .get_tags_for_entity(&entity_id, OriginTagCardinality::High)
        });
        let tags = tags
            .iter()
            .flat_map(|tags| tags.into_iter())
            .map(|tag| tag.as_str().to_string());
        let hash = hash_service_tags(tags);
        HeaderValue::from_str(&hash).expect("a hex digest is a valid header value")
    }

    /// Finds the container of the tracer, in the trace-agent's order: the container ID in its Local Data, its
    /// `Datadog-Container-ID`, the container of its process, the container of its cgroup inode, then its External
    /// Data.
    fn container_entity(
        &self, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
    ) -> Option<EntityId> {
        let header = |name: &HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
        };

        let local_data = header(&ENTITY_ID_HEADER).and_then(EntityId::from_local_data);
        if let Some(container @ EntityId::Container(_)) = &local_data {
            return Some(container.clone());
        }
        if let Some(container_id) = header(&CONTAINER_ID_HEADER) {
            return Some(EntityId::Container(container_id.into()));
        }

        let process_id = connection
            .and_then(|ConnectInfo(addr)| addr.process_credentials())
            .and_then(|creds| u32::try_from(creds.pid).ok());
        if let (Some(process_id), Some(resolver)) = (process_id, self.capture_entity_resolver.as_deref()) {
            if let Some(container @ EntityId::Container(_)) = resolver.resolve_container_entity_for_live_pid(process_id)
            {
                return Some(container);
            }
        }

        let workload_provider = self.workload_provider.as_deref()?;
        // Tag lookups follow the alias from a cgroup inode to its container.
        if let Some(inode @ EntityId::ContainerInode(_)) = local_data {
            if workload_provider
                .get_tags_for_entity(&inode, OriginTagCardinality::High)
                .is_some()
            {
                return Some(inode);
            }
        }

        let external_data = header(&EXTERNAL_DATA_HEADER)?;
        let mut origin = RawOrigin::default();
        origin.set_external_data(external_data);
        workload_provider
            .get_resolved_origin(origin)?
            .resolved_external_data()
            .map(|external_data| external_data.container_entity_id().clone())
    }
}

/// Hashes the service tags among `tags`, as the trace-agent's `computeContainerTagsHash` does: the `key:value` tags
/// whose key identifies a service, sorted and joined with commas, in hexadecimal SHA-256.
fn hash_service_tags(tags: impl IntoIterator<Item = String>) -> String {
    let mut service_tags: Vec<String> = tags
        .into_iter()
        .filter(|tag| {
            tag.split_once(':')
                .is_some_and(|(key, _)| SERVICE_ORIGIN_TAGS.contains(&key))
        })
        .collect();
    service_tags.sort_unstable();
    faster_hex::hex_string(&Sha256::digest(service_tags.join(",").as_bytes()))
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::tags::{SharedTagSet, Tag, TagSet};
    use saluki_env::workload::origin::{ResolvedExternalData, ResolvedOrigin};
    use saluki_io::net::ProcessCredentials;

    use super::*;

    /// The hash of no tags.
    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn hashes_the_sorted_service_tags_only() {
        assert_eq!(hash_service_tags(Vec::new()), EMPTY_HASH);

        let tags = [
            "service:web",
            "kube_namespace:prod",
            "env:prod",
            "short_image:nginx",
            "service",
        ]
        .map(str::to_string);
        let expected = faster_hex::hex_string(&Sha256::digest(b"kube_namespace:prod,service:web,short_image:nginx"));
        assert_eq!(hash_service_tags(tags), expected);
    }

    /// A workload provider that knows the tags of one container, of an inode aliased to it, and of the container of
    /// one pod.
    struct Workload;

    fn container_tags() -> SharedTagSet {
        let mut tags = TagSet::default();
        tags.insert_tag(Tag::from("service:web"));
        tags.insert_tag(Tag::from("container_id:known"));
        tags.into_shared()
    }

    impl WorkloadProvider for Workload {
        fn get_tags_for_entity(&self, entity_id: &EntityId, _: OriginTagCardinality) -> Option<SharedTagSet> {
            match entity_id {
                EntityId::Container(id) if &**id == "known" => Some(container_tags()),
                EntityId::ContainerInode(42) => Some(container_tags()),
                _ => None,
            }
        }

        fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
            // Any valid External Data belongs to the known container.
            origin.external_data()?;
            let container = EntityId::Container("known".into());
            let pod = EntityId::PodUid("pod".into());
            Some(ResolvedOrigin::from_parts(
                None,
                None,
                None,
                None,
                Some(ResolvedExternalData::new(pod, container)),
            ))
        }
    }

    struct Resolver;

    impl CaptureEntityResolver for Resolver {
        fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
            (process_id == 7).then(|| EntityId::Container("known".into()))
        }
    }

    fn hasher() -> ContainerTagsHasher {
        ContainerTagsHasher::new(Some(Arc::new(Resolver)), Some(Arc::new(Workload)))
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    fn unix_peer(pid: i32) -> ConnectInfo<ConnectionAddress> {
        ConnectInfo(ProcessCredentials { pid, uid: 0, gid: 0 }.into())
    }

    #[test]
    fn finds_the_container_as_the_trace_agent_does() {
        let hasher = hasher();
        let known = Some(EntityId::Container("known".into()));
        let find = |pairs: &[(&str, &str)], peer: Option<ConnectInfo<ConnectionAddress>>| {
            hasher.container_entity(&headers(pairs), peer.as_ref())
        };

        // The container ID of the Local Data wins over every other source.
        assert_eq!(
            find(
                &[
                    ("Datadog-Entity-ID", "ci-known,in-42"),
                    ("Datadog-Container-ID", "other")
                ],
                None
            ),
            known
        );
        assert_eq!(find(&[("Datadog-Container-ID", "known")], Some(unix_peer(8))), known);
        assert_eq!(find(&[], Some(unix_peer(7))), known);
        assert_eq!(
            find(&[("Datadog-Entity-ID", "in-42")], None),
            Some(EntityId::ContainerInode(42))
        );
        assert_eq!(
            find(&[("Datadog-External-Env", "it-false,cn-app,pu-1234")], None),
            known
        );
        assert_eq!(find(&[("Datadog-Entity-ID", "in-43")], Some(unix_peer(8))), None);
    }

    #[test]
    fn hashes_the_tags_of_the_tracer_container_or_none() {
        let hasher = hasher();
        let expected = faster_hex::hex_string(&Sha256::digest(b"service:web"));

        let found = hasher.header_value(&headers(&[("Datadog-Container-ID", "known")]), None);
        assert_eq!(found, expected.as_str());

        let not_found = hasher.header_value(&headers(&[("Datadog-Container-ID", "unknown")]), None);
        assert_eq!(not_found, EMPTY_HASH);

        let standalone =
            ContainerTagsHasher::default().header_value(&headers(&[("Datadog-Container-ID", "known")]), None);
        assert_eq!(standalone, EMPTY_HASH);
    }
}

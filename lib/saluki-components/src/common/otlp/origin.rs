use std::sync::Arc;

use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality as ConfigOriginTagCardinality;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::FastHashSet;
use saluki_context::{
    origin::{OriginTagCardinality, OriginTagsResolver, RawOrigin},
    tags::{SharedTagSet, TagSet},
};
use saluki_env::{workload::EntityId, WorkloadProvider};
use stringtheory::MetaString;
use tracing::trace;

const CONTAINER_ID: &str = "container.id";
const OCI_MANIFEST_DIGEST: &str = "oci.manifest.digest";
const ECS_TASK_ARN: &str = "aws.ecs.task.arn";
const K8S_CONTAINER_NAME: &str = "k8s.container.name";
const K8S_DEPLOYMENT_NAME: &str = "k8s.deployment.name";
const K8S_NAMESPACE_NAME: &str = "k8s.namespace.name";
const K8S_NODE_NAME: &str = "k8s.node.name";
const K8S_POD_UID: &str = "k8s.pod.uid";
const PROCESS_PID: &str = "process.pid";
const CGROUP_INODE: &str = "datadog.container.cgroup_inode";
const INIT_CONTAINER: &str = "datadog.container.is_init";

/// Recognized OTLP resource attributes used for entity tag resolution.
#[derive(Default)]
struct OtlpResourceAttributes<'a> {
    container_id: Option<&'a str>,
    oci_manifest_digest: Option<&'a str>,
    ecs_task_arn: Option<&'a str>,
    k8s_container_name: Option<&'a str>,
    k8s_deployment_name: Option<&'a str>,
    k8s_namespace_name: Option<&'a str>,
    k8s_node_name: Option<&'a str>,
    k8s_pod_uid: Option<&'a str>,
    process_pid: Option<i64>,
    cgroup_inode: Option<i64>,
    init_container: Option<bool>,
}

impl<'a> OtlpResourceAttributes<'a> {
    fn from_attributes(attributes: &'a [otlp_common::KeyValue]) -> Self {
        let mut extracted = Self::default();

        for attribute in attributes {
            let Some(value) = attribute.value.as_ref().and_then(|value| value.value.as_ref()) else {
                continue;
            };

            match (attribute.key.as_str(), value) {
                (CONTAINER_ID, Value::StringValue(value)) => extracted.container_id = Some(value),
                (OCI_MANIFEST_DIGEST, Value::StringValue(value)) => extracted.oci_manifest_digest = Some(value),
                (ECS_TASK_ARN, Value::StringValue(value)) => extracted.ecs_task_arn = Some(value),
                (K8S_CONTAINER_NAME, Value::StringValue(value)) => extracted.k8s_container_name = Some(value),
                (K8S_DEPLOYMENT_NAME, Value::StringValue(value)) => extracted.k8s_deployment_name = Some(value),
                (K8S_NAMESPACE_NAME, Value::StringValue(value)) => extracted.k8s_namespace_name = Some(value),
                (K8S_NODE_NAME, Value::StringValue(value)) => extracted.k8s_node_name = Some(value),
                (K8S_POD_UID, Value::StringValue(value)) => extracted.k8s_pod_uid = Some(value),
                (PROCESS_PID, Value::IntValue(value)) => extracted.process_pid = Some(*value),
                (CGROUP_INODE, Value::IntValue(value)) => extracted.cgroup_inode = Some(*value),
                (INIT_CONTAINER, Value::BoolValue(value)) => extracted.init_container = Some(*value),
                _ => {}
            }
        }

        extracted
    }
}

#[derive(Clone)]
pub struct OtlpOriginTagResolver {
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
}

impl OtlpOriginTagResolver {
    pub fn new(workload_provider: Arc<dyn WorkloadProvider + Send + Sync>) -> Self {
        Self { workload_provider }
    }

    /// Resolves entity and global tags for one OTLP metrics resource by entity precedence.
    pub fn resolve_resource_tags(
        &self, attributes: &[otlp_common::KeyValue], tag_cardinality: ConfigOriginTagCardinality,
    ) -> SharedTagSet {
        let tag_cardinality = match tag_cardinality {
            ConfigOriginTagCardinality::Low => OriginTagCardinality::Low,
            ConfigOriginTagCardinality::Orchestrator => OriginTagCardinality::Orchestrator,
            ConfigOriginTagCardinality::High => OriginTagCardinality::High,
            ConfigOriginTagCardinality::None => return SharedTagSet::default(),
        };

        let attributes = OtlpResourceAttributes::from_attributes(attributes);
        let mut tags = TagSet::default();
        let mut written_keys = FastHashSet::default();

        if let Some(container_entity) = self.container_entity_from_attributes(&attributes, tag_cardinality) {
            self.collect_tags_for_entity(container_entity, tag_cardinality, &mut written_keys, &mut tags);
        }

        for entity_id in entity_ids_from_attributes(&attributes) {
            self.collect_tags_for_entity(entity_id, tag_cardinality, &mut written_keys, &mut tags);
        }

        self.collect_tags_for_entity(EntityId::Global, tag_cardinality, &mut written_keys, &mut tags);
        tags.into_shared()
    }

    fn container_entity_from_attributes(
        &self, attributes: &OtlpResourceAttributes<'_>, tag_cardinality: OriginTagCardinality,
    ) -> Option<EntityId> {
        if let Some(container_id) = attributes.container_id.filter(|value| !value.is_empty()) {
            return Some(EntityId::Container(container_id.into()));
        }

        let mut fallback_entities = Vec::with_capacity(3);
        if let Some(process_id) = attributes
            .process_pid
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value != 0)
        {
            fallback_entities.push(EntityId::ContainerPid(process_id));
        }
        if let Some(cgroup_inode) = attributes
            .cgroup_inode
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value != 0)
        {
            fallback_entities.push(EntityId::ContainerInode(cgroup_inode));
        }
        if let Some(container_entity) = self.container_entity_from_external_data(attributes) {
            fallback_entities.push(container_entity);
        }

        fallback_entities.into_iter().find(|entity_id| {
            self.workload_provider
                .get_tags_for_entity(entity_id, tag_cardinality)
                .is_some_and(|tags| !tags.is_empty())
        })
    }

    fn container_entity_from_external_data(&self, attributes: &OtlpResourceAttributes<'_>) -> Option<EntityId> {
        let pod_uid = attributes.k8s_pod_uid?;
        let container_name = attributes.k8s_container_name?;
        let init_container = attributes.init_container.unwrap_or(false);
        let external_data = format!("pu-{pod_uid},cn-{container_name},it-{init_container}");

        let mut origin = RawOrigin::default();
        origin.set_external_data(external_data.as_str());
        self.workload_provider.get_resolved_origin(origin).and_then(|origin| {
            origin
                .resolved_external_data()
                .map(|external_data| external_data.container_entity_id().clone())
        })
    }

    fn collect_tags_for_entity(
        &self, entity_id: EntityId, tag_cardinality: OriginTagCardinality, written_keys: &mut FastHashSet<String>,
        tags: &mut TagSet,
    ) {
        let Some(entity_tags) = self.workload_provider.get_tags_for_entity(&entity_id, tag_cardinality) else {
            trace!(
                ?entity_id,
                cardinality = tag_cardinality.as_str(),
                "No tags found for entity."
            );
            return;
        };

        for tag in &entity_tags {
            let Some((key, value)) = tag.as_str().split_once(':') else {
                continue;
            };
            if key.is_empty() || value.is_empty() || !written_keys.insert(key.to_owned()) {
                continue;
            }
            tags.insert_tag(tag.clone());
        }
    }

    fn collect_origin_tags(&self, resolved_origin: &saluki_env::workload::origin::ResolvedOrigin) -> SharedTagSet {
        let mut collected_tags = SharedTagSet::default();
        let tag_cardinality = resolved_origin.cardinality().unwrap_or(OriginTagCardinality::Low);

        // Logs retain their existing origin-only behavior. Metrics use `resolve_resource_tags` above to include the
        // full OTLP entity list and global tags.
        let entity_ids = [
            resolved_origin.local_data(),
            resolved_origin.pod_uid(),
            resolved_origin.process_id(),
        ];

        for entity_id in entity_ids.iter().flatten() {
            if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                if !tags.is_empty() {
                    collected_tags.extend_from_shared(&tags);
                    return collected_tags;
                }
            } else {
                trace!(
                    ?entity_id,
                    cardinality = tag_cardinality.as_str(),
                    "No tags found for entity."
                );
            }
        }
        collected_tags
    }
}

impl OriginTagsResolver for OtlpOriginTagResolver {
    fn resolve_origin_tags(&self, origin: RawOrigin<'_>) -> SharedTagSet {
        match self.workload_provider.get_resolved_origin(origin.clone()) {
            Some(resolved_origin) => self.collect_origin_tags(&resolved_origin),
            None => {
                trace!(?origin, "No resolved origin found for raw origin.");
                SharedTagSet::default()
            }
        }
    }
}

fn entity_ids_from_attributes(attributes: &OtlpResourceAttributes<'_>) -> Vec<EntityId> {
    let mut entity_ids = Vec::with_capacity(7);

    if let Some(oci_manifest_digest) = attributes.oci_manifest_digest {
        if let Some((_, digest)) = oci_manifest_digest.split_once("@sha256:") {
            entity_ids.push(EntityId::ContainerImageMetadata(format!("sha256:{digest}").into()));
        }
    }
    if let Some(ecs_task_arn) = attributes.ecs_task_arn {
        entity_ids.push(EntityId::EcsTask(ecs_task_arn.into()));
    }
    if let (Some(deployment), Some(namespace)) = (attributes.k8s_deployment_name, attributes.k8s_namespace_name) {
        entity_ids.push(EntityId::KubernetesDeployment(
            format!("{namespace}/{deployment}").into(),
        ));
    }
    if let Some(namespace) = attributes.k8s_namespace_name {
        entity_ids.push(EntityId::KubernetesMetadata(format!("/namespaces//{namespace}").into()));
    }
    if let Some(node_name) = attributes.k8s_node_name {
        entity_ids.push(EntityId::KubernetesNode(node_name.into()));
    }
    if let Some(pod_uid) = attributes.k8s_pod_uid {
        if let Some(entity_id) = EntityId::from_pod_uid(pod_uid) {
            entity_ids.push(entity_id);
        }
    }
    if let Some(process_id) = attributes.process_pid {
        entity_ids.push(EntityId::Process(MetaString::from(process_id.to_string())));
    }

    entity_ids
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otlp_common::{any_value::Value, AnyValue, KeyValue};
    use saluki_env::workload::providers::TestWorkloadProvider;

    use super::*;

    fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn tags(tags: SharedTagSet) -> Vec<String> {
        let mut tags = tags.into_iter().map(|tag| tag.as_str().to_owned()).collect::<Vec<_>>();
        tags.sort();
        tags
    }

    struct ExternalDataWorkloadProvider {
        tags: TestWorkloadProvider,
    }

    impl WorkloadProvider for ExternalDataWorkloadProvider {
        fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
            self.tags.get_tags_for_entity(entity_id, cardinality)
        }

        fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<saluki_env::workload::origin::ResolvedOrigin> {
            (!origin.is_empty()).then(|| {
                saluki_env::workload::origin::ResolvedOrigin::from_parts(
                    origin.cardinality(),
                    origin.process_id().map(EntityId::ContainerPid),
                    origin.local_data().and_then(EntityId::from_local_data),
                    origin.pod_uid().and_then(EntityId::from_pod_uid),
                    Some(saluki_env::workload::origin::ResolvedExternalData::new(
                        EntityId::PodUid("pod-1".into()),
                        EntityId::Container("external-container".into()),
                    )),
                )
            })
        }
    }

    #[test]
    fn resolves_all_resource_entities_in_core_order_and_merges_global_tags_last() {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_entity(
                EntityId::Container("container-id".into()),
                &["service:container", "container:present"],
            )
            .add_entity(
                EntityId::ContainerImageMetadata("sha256:digest".into()),
                &["service:image", "image:present"],
            )
            .add_entity(EntityId::EcsTask("task-arn".into()), &["task:present"])
            .add_entity(
                EntityId::KubernetesDeployment("default/api".into()),
                &["deployment:present"],
            )
            .add_entity(
                EntityId::KubernetesMetadata("/namespaces//default".into()),
                &["namespace:present"],
            )
            .add_entity(EntityId::KubernetesNode("node-1".into()), &["node:present"])
            .add_entity(EntityId::PodUid("pod-1".into()), &["pod:present"])
            .add_entity(EntityId::Process("42".into()), &["process:present"])
            .add_entity(EntityId::Global, &["service:global", "global:present"]);
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));

        let resource = vec![
            attribute(CONTAINER_ID, Value::StringValue("container-id".into())),
            attribute(OCI_MANIFEST_DIGEST, Value::StringValue("image@sha256:digest".into())),
            attribute(ECS_TASK_ARN, Value::StringValue("task-arn".into())),
            attribute(K8S_DEPLOYMENT_NAME, Value::StringValue("api".into())),
            attribute(K8S_NAMESPACE_NAME, Value::StringValue("default".into())),
            attribute(K8S_NODE_NAME, Value::StringValue("node-1".into())),
            attribute(K8S_POD_UID, Value::StringValue("pod-1".into())),
            attribute(PROCESS_PID, Value::IntValue(42)),
        ];

        assert_eq!(
            tags(resolver.resolve_resource_tags(&resource, ConfigOriginTagCardinality::Low)),
            vec![
                "container:present",
                "deployment:present",
                "global:present",
                "image:present",
                "namespace:present",
                "node:present",
                "pod:present",
                "process:present",
                "service:container",
                "task:present",
            ]
        );
    }

    #[test]
    fn falls_back_from_process_id_to_container_tags_before_other_entities() {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_entity(EntityId::ContainerPid(42), &["service:container", "container:present"])
            .add_entity(EntityId::Process("42".into()), &["service:process", "process:present"]);
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
        let resource = vec![attribute(PROCESS_PID, Value::IntValue(42))];

        assert_eq!(
            tags(resolver.resolve_resource_tags(&resource, ConfigOriginTagCardinality::Low)),
            vec!["container:present", "process:present", "service:container"]
        );
    }

    #[test]
    fn falls_back_from_cgroup_inode_to_container_tags() {
        let provider = TestWorkloadProvider::with_entity(
            EntityId::ContainerInode(123),
            &["service:container", "container:present"],
        );
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
        let resource = vec![attribute(CGROUP_INODE, Value::IntValue(123))];

        assert_eq!(
            tags(resolver.resolve_resource_tags(&resource, ConfigOriginTagCardinality::Low)),
            vec!["container:present", "service:container"]
        );
    }

    #[test]
    fn falls_back_from_pod_uid_and_container_name_to_container_tags() {
        let provider = ExternalDataWorkloadProvider {
            tags: TestWorkloadProvider::with_entity(
                EntityId::Container("external-container".into()),
                &["service:container", "container:present"],
            ),
        };
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
        let resource = vec![
            attribute(K8S_POD_UID, Value::StringValue("pod-1".into())),
            attribute(K8S_CONTAINER_NAME, Value::StringValue("app".into())),
        ];

        assert_eq!(
            tags(resolver.resolve_resource_tags(&resource, ConfigOriginTagCardinality::Low)),
            vec!["container:present", "service:container"]
        );
    }

    #[test]
    fn uses_the_requested_tag_cardinality() {
        let provider = TestWorkloadProvider::with_entity_cardinalities(
            EntityId::Global,
            &["low:present"],
            &["orchestrator:present"],
            &["high:present"],
        );
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));

        assert_eq!(
            tags(resolver.resolve_resource_tags(&[], ConfigOriginTagCardinality::Low)),
            vec!["low:present"]
        );
        assert_eq!(
            tags(resolver.resolve_resource_tags(&[], ConfigOriginTagCardinality::High)),
            vec!["high:present", "low:present", "orchestrator:present"]
        );
        assert!(resolver
            .resolve_resource_tags(&[], ConfigOriginTagCardinality::None)
            .is_empty());
    }
}

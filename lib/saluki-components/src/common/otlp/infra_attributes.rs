//! Infra attributes: entity and global tags written into OTLP resource attributes.
//!
//! This matches `ProcessTags` in the Datadog Agent's infra attributes processor
//! (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/common.go#L87-L156>),
//! with container tag promotion off, as the Agent's logs pipeline uses it:
//!
//! 1. The resource attributes give an ordered list of entities, as `entityIDsFromAttributes` does.
//! 2. The tags of each entity, and then the global tags, give one value for each tag key. The first value wins.
//! 3. Each unified service tag (`service`, `env`, `version`) fills its OpenTelemetry attribute only when the resource
//!    has none of the attributes for that tag. Every other tag overwrites the resource attribute with its key.
//!
//! The translator then turns known keys into tags, as it does for any resource attribute.

use otlp_protos::opentelemetry::proto::common::v1::KeyValue;
use saluki_core::data_model::origin::OriginTagCardinality;
use saluki_env::workload::EntityId;

use super::origin::OtlpOriginTagResolver;
use super::util::{first_attribute, put_str_attribute};
use super::value::otlp_value_as_string;

const CONTAINER_ID: &str = "container.id";
const OCI_MANIFEST_DIGEST: &str = "oci.manifest.digest";
const ECS_TASK_ARN: &str = "aws.ecs.task.arn";
const K8S_DEPLOYMENT_NAME: &str = "k8s.deployment.name";
const K8S_NAMESPACE_NAME: &str = "k8s.namespace.name";
const K8S_NODE_NAME: &str = "k8s.node.name";
const K8S_POD_UID: &str = "k8s.pod.uid";
const PROCESS_PID: &str = "process.pid";

/// The OpenTelemetry attributes of each unified service tag (`unifiedServiceTagMap`).
///
/// A tag fills the first attribute, and only when the resource has none of the attributes.
const UNIFIED_SERVICE_TAGS: [(&str, &[&str]); 3] = [
    ("service", &["service.name"]),
    ("env", &["deployment.environment", "deployment.environment.name"]),
    ("version", &["service.version"]),
];

/// Writes the entity and global tags of an OTLP resource into its attributes.
///
/// The tags use low cardinality: the Agent's logs pipeline never sets the processor's cardinality, and its default
/// is low (`factory.go`).
///
/// When the resource has no `container.id`, the container found from the origin attributes comes first in the entity
/// list. The Agent instead detects the container ID, writes it into `container.id`, and so also sends it as a tag.
/// ADP doesn't write `container.id` yet (`gap/origin-detection`). The lookup also reads the origin attributes as the
/// metrics path does, not as the Agent's `originInfoFromAttributes` does: it uses the last valid attribute with a key,
/// reads the pod UID and the container name only as strings, and skips a process ID that doesn't fit a `u32`.
pub fn apply_infra_attributes(attributes: &mut Vec<KeyValue>, resolver: &OtlpOriginTagResolver) {
    let origin_container = match first_attribute(attributes, CONTAINER_ID) {
        Some(_) => None,
        None => resolver.container_entity_from_origin_attributes(attributes, OriginTagCardinality::Low),
    };
    let entity_ids = origin_container
        .into_iter()
        .chain(entity_ids_from_attributes(attributes));
    let tags = resolver.resolve_ordered_tags(entity_ids, OriginTagCardinality::Low);

    for tag in &tags {
        let Some((key, value)) = tag.as_str().split_once(':') else {
            continue;
        };
        match UNIFIED_SERVICE_TAGS.iter().find(|(tag_key, _)| *tag_key == key) {
            Some((_, otel_keys)) => {
                if otel_keys
                    .iter()
                    .all(|otel_key| first_attribute(attributes, otel_key).is_none())
                {
                    put_str_attribute(attributes, otel_keys[0], value);
                }
            }
            None => put_str_attribute(attributes, key, value),
        }
    }
}

/// Returns the entities of an OTLP resource, in the order of `entityIDsFromAttributes`.
///
/// Every value of an entity attribute counts, whatever its type: it converts to a string as `pdata`'s `AsString` does.
fn entity_ids_from_attributes(attributes: &[KeyValue]) -> impl Iterator<Item = EntityId> {
    let value = |key| first_attribute(attributes, key).map(otlp_value_as_string);
    let namespace = value(K8S_NAMESPACE_NAME);
    let deployment = value(K8S_DEPLOYMENT_NAME)
        .zip(namespace.as_ref())
        .map(|(deployment, namespace)| EntityId::KubernetesDeployment(format!("{namespace}/{deployment}").into()));

    [
        value(CONTAINER_ID).map(|container_id| EntityId::Container(container_id.as_ref().into())),
        value(OCI_MANIFEST_DIGEST).and_then(|digest| {
            let (_, digest) = digest.split_once("@sha256:")?;
            Some(EntityId::ContainerImageMetadata(format!("sha256:{digest}").into()))
        }),
        value(ECS_TASK_ARN).map(|ecs_task_arn| EntityId::EcsTask(ecs_task_arn.as_ref().into())),
        deployment,
        namespace.map(|namespace| EntityId::KubernetesMetadata(format!("/namespaces//{namespace}").into())),
        value(K8S_NODE_NAME).map(|node_name| EntityId::KubernetesNode(node_name.as_ref().into())),
        value(K8S_POD_UID).map(|pod_uid| EntityId::PodUid(pod_uid.as_ref().into())),
        value(PROCESS_PID).map(|process_id| EntityId::Process(process_id.as_ref().into())),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue};
    use saluki_env::workload::providers::TestWorkloadProvider;

    use super::*;

    fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn string(key: &str, value: &str) -> KeyValue {
        attribute(key, Value::StringValue(value.to_owned()))
    }

    fn strings(attributes: &[(&str, &str)]) -> Vec<KeyValue> {
        attributes.iter().map(|(key, value)| string(key, value)).collect()
    }

    fn apply(provider: TestWorkloadProvider, mut attributes: Vec<KeyValue>) -> Vec<KeyValue> {
        apply_infra_attributes(&mut attributes, &OtlpOriginTagResolver::new(Arc::new(provider)));
        attributes
    }

    /// Returns the attributes as a map, for comparisons that don't depend on attribute order.
    fn as_map(attributes: &[KeyValue]) -> BTreeMap<String, Option<Value>> {
        attributes
            .iter()
            .map(|attribute| {
                let value = attribute.value.as_ref().and_then(|value| value.value.clone());
                (attribute.key.clone(), value)
            })
            .collect()
    }

    fn string_map(attributes: &[(&str, &str)]) -> BTreeMap<String, Option<Value>> {
        as_map(&strings(attributes))
    }

    /// Ports `standardLogTests` from `TestInfraAttributesLogProcessor`
    /// (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/logs_test.go#L37-L231>).
    ///
    /// The detection cases find the container from the origin attributes, but don't write `container.id`
    /// (`gap/origin-detection`). The External Data case is in the tests of `OtlpOriginTagResolver`.
    #[test]
    fn standard_log_tests() {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_entity(EntityId::Container("test".into()), &["container:id"])
            .add_entity(EntityId::ContainerPid(12345), &["container:id"])
            .add_entity(EntityId::ContainerInode(12345), &["container:id"])
            .add_entity(
                EntityId::KubernetesDeployment("namespace/deployment".into()),
                &["deployment:name"],
            )
            .add_entity(EntityId::Global, &["global:tag"]);
        let origin_tag_resolver = OtlpOriginTagResolver::new(Arc::new(provider));

        let cases: Vec<(&str, Vec<KeyValue>, Vec<KeyValue>)> = vec![
            (
                "one tag with global",
                strings(&[("container.id", "test")]),
                strings(&[("global", "tag"), ("container.id", "test"), ("container", "id")]),
            ),
            (
                "two tags with global",
                strings(&[
                    ("container.id", "test"),
                    ("k8s.namespace.name", "namespace"),
                    ("k8s.deployment.name", "deployment"),
                ]),
                strings(&[
                    ("global", "tag"),
                    ("container.id", "test"),
                    ("k8s.namespace.name", "namespace"),
                    ("k8s.deployment.name", "deployment"),
                    ("container", "id"),
                    ("deployment", "name"),
                ]),
            ),
            (
                "second resource of two",
                strings(&[
                    ("k8s.namespace.name", "namespace"),
                    ("k8s.deployment.name", "deployment"),
                ]),
                strings(&[
                    ("global", "tag"),
                    ("k8s.namespace.name", "namespace"),
                    ("k8s.deployment.name", "deployment"),
                    ("deployment", "name"),
                ]),
            ),
            (
                "detect container from PID",
                vec![attribute("process.pid", Value::IntValue(12345))],
                [
                    vec![attribute("process.pid", Value::IntValue(12345))],
                    strings(&[("global", "tag"), ("container", "id")]),
                ]
                .concat(),
            ),
            (
                "detect container from cgroup inode",
                vec![attribute("datadog.container.cgroup_inode", Value::IntValue(12345))],
                [
                    vec![attribute("datadog.container.cgroup_inode", Value::IntValue(12345))],
                    strings(&[("global", "tag"), ("container", "id")]),
                ]
                .concat(),
            ),
        ];

        for (name, mut attributes, expected) in cases {
            apply_infra_attributes(&mut attributes, &origin_tag_resolver);
            assert_eq!(as_map(&attributes), as_map(&expected), "case: {name}");
        }
    }

    /// A `container.id` attribute stops the origin lookup, even when it's empty, as it stops the Agent's container ID
    /// detection.
    #[test]
    fn container_id_attribute_skips_the_origin_container() {
        for container_id in ["", "other"] {
            let provider = TestWorkloadProvider::with_entity(EntityId::ContainerPid(12345), &["origin:container"]);
            let attributes = vec![
                string("container.id", container_id),
                attribute("process.pid", Value::IntValue(12345)),
            ];
            let attributes = apply(provider, attributes);
            assert!(
                !as_map(&attributes).contains_key("origin"),
                "container.id: {container_id:?}"
            );
        }
    }

    #[test]
    fn unified_service_tags_fill_only_missing_attributes() {
        let global = ["service:tag-service", "env:tag-env", "version:tag-version"];
        type Case<'a> = (&'a str, Vec<KeyValue>, Vec<(&'a str, &'a str)>);
        let cases: Vec<Case> = vec![
            (
                "all missing",
                vec![],
                vec![
                    ("service.name", "tag-service"),
                    ("deployment.environment", "tag-env"),
                    ("service.version", "tag-version"),
                ],
            ),
            (
                "all present",
                strings(&[
                    ("service.name", "svc"),
                    ("deployment.environment", "dev"),
                    ("service.version", "1"),
                ]),
                vec![
                    ("service.name", "svc"),
                    ("deployment.environment", "dev"),
                    ("service.version", "1"),
                ],
            ),
            (
                "env from deployment.environment.name",
                strings(&[("deployment.environment.name", "dev")]),
                vec![
                    ("deployment.environment.name", "dev"),
                    ("service.name", "tag-service"),
                    ("service.version", "tag-version"),
                ],
            ),
            (
                "empty service.name counts as present",
                strings(&[("service.name", "")]),
                vec![
                    ("service.name", ""),
                    ("deployment.environment", "tag-env"),
                    ("service.version", "tag-version"),
                ],
            ),
        ];

        for (name, attributes, expected) in cases {
            let provider = TestWorkloadProvider::with_entity(EntityId::Global, &global);
            assert_eq!(
                as_map(&apply(provider, attributes)),
                string_map(&expected),
                "case: {name}"
            );
        }

        // A present attribute of any type stops the fill.
        let provider = TestWorkloadProvider::with_entity(EntityId::Global, &global);
        let attributes = apply(provider, vec![attribute("service.version", Value::IntValue(2))]);
        assert_eq!(
            as_map(&attributes)["service.version"],
            Some(Value::IntValue(2)),
            "integer service.version"
        );
        // Unified service tags never write their own keys.
        assert!(first_attribute(&attributes, "service").is_none() && first_attribute(&attributes, "env").is_none());
    }

    #[test]
    fn first_tag_value_wins_and_overwrites_resource_attributes() {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_entity(
                EntityId::Container("c1".into()),
                &["team:container", "service:container"],
            )
            .add_entity(EntityId::KubernetesNode("node".into()), &["team:node", "rack:node"])
            .add_entity(
                EntityId::Global,
                &["team:global", "rack:global", "region:global", "service:global"],
            );
        let attributes = apply(
            provider,
            [
                strings(&[
                    ("container.id", "c1"),
                    ("k8s.node.name", "node"),
                    ("region", "attribute"),
                ]),
                strings(&[("region", "duplicate")]),
            ]
            .concat(),
        );

        assert_eq!(
            attributes,
            [
                strings(&[("container.id", "c1"), ("k8s.node.name", "node"), ("region", "global")]),
                strings(&[
                    ("region", "duplicate"),
                    ("team", "container"),
                    ("service.name", "container"),
                    ("rack", "node"),
                ]),
            ]
            .concat()
        );
    }

    #[test]
    fn skips_invalid_tags() {
        let provider = TestWorkloadProvider::with_entity(
            EntityId::Global,
            &["no-separator", ":no-key", "no-value:", "nested:a:b"],
        );
        assert_eq!(as_map(&apply(provider, vec![])), string_map(&[("nested", "a:b")]));
    }

    #[test]
    fn resolves_every_supported_entity() {
        let cases: Vec<(&str, Vec<KeyValue>, EntityId)> = vec![
            (
                "container",
                strings(&[("container.id", "c1")]),
                EntityId::Container("c1".into()),
            ),
            (
                "integer container ID",
                vec![attribute("container.id", Value::IntValue(7))],
                EntityId::Container("7".into()),
            ),
            (
                "image",
                strings(&[("oci.manifest.digest", "image@sha256:abc")]),
                EntityId::ContainerImageMetadata("sha256:abc".into()),
            ),
            (
                "ECS task",
                strings(&[("aws.ecs.task.arn", "arn")]),
                EntityId::EcsTask("arn".into()),
            ),
            (
                "deployment",
                strings(&[("k8s.deployment.name", "api"), ("k8s.namespace.name", "default")]),
                EntityId::KubernetesDeployment("default/api".into()),
            ),
            (
                "namespace",
                strings(&[("k8s.namespace.name", "default")]),
                EntityId::KubernetesMetadata("/namespaces//default".into()),
            ),
            (
                "node",
                strings(&[("k8s.node.name", "node")]),
                EntityId::KubernetesNode("node".into()),
            ),
            (
                "pod",
                strings(&[("k8s.pod.uid", "pod")]),
                EntityId::PodUid("pod".into()),
            ),
            (
                "pod UID `none`",
                strings(&[("k8s.pod.uid", "none")]),
                EntityId::PodUid("none".into()),
            ),
            (
                "integer process ID",
                vec![attribute("process.pid", Value::IntValue(42))],
                EntityId::Process("42".into()),
            ),
            (
                "string process ID",
                strings(&[("process.pid", "42")]),
                EntityId::Process("42".into()),
            ),
        ];

        for (name, attributes, entity_id) in cases {
            let provider = TestWorkloadProvider::with_entity(entity_id, &["found:yes"]);
            let attributes = apply(provider, attributes);
            assert_eq!(
                as_map(&attributes).get("found"),
                Some(&Some(Value::StringValue("yes".into()))),
                "case: {name}"
            );
        }

        // A deployment needs a namespace, and an image digest needs `@sha256:`.
        for attributes in [
            strings(&[("k8s.deployment.name", "api")]),
            strings(&[("oci.manifest.digest", "image:latest")]),
        ] {
            let mut provider = TestWorkloadProvider::new();
            provider
                .add_entity(EntityId::KubernetesDeployment("/api".into()), &["found:yes"])
                .add_entity(EntityId::ContainerImageMetadata("sha256:".into()), &["found:yes"]);
            let applied = apply(provider, attributes.clone());
            assert_eq!(applied, attributes);
        }
    }

    /// The entity lookup converts doubles and bytes as `pdata`'s `AsString` does.
    #[test]
    fn entity_ids_convert_values_as_pdata_does() {
        let cases = [
            (
                "double",
                attribute("process.pid", Value::DoubleValue(12345.0)),
                EntityId::Process("12345".into()),
            ),
            (
                "bytes",
                attribute("container.id", Value::BytesValue(b"abc".to_vec())),
                EntityId::Container("YWJj".into()),
            ),
        ];
        for (name, attribute, entity_id) in cases {
            let provider = TestWorkloadProvider::with_entity(entity_id, &["found:yes"]);
            let attributes = apply(provider, vec![attribute]);
            assert_eq!(
                as_map(&attributes).get("found"),
                Some(&Some(Value::StringValue("yes".into()))),
                "case: {name}"
            );
        }
    }

    #[test]
    fn uses_low_cardinality_tags() {
        let provider = TestWorkloadProvider::with_entity_cardinalities(
            EntityId::Global,
            &["low:present"],
            &["orchestrator:present"],
            &["high:present"],
        );
        assert_eq!(as_map(&apply(provider, vec![])), string_map(&[("low", "present")]));
    }
}

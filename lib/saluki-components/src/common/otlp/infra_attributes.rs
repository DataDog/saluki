//! Infra attributes: entity and global tags written into OTLP resource attributes.
//!
//! [`process_tags`] matches `ProcessTags` in the Datadog Agent's infra attributes processor
//! (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/common.go#L87-L206>),
//! which the Agent runs on the resources of OTLP metrics, traces, and logs:
//!
//! 1. The resource attributes give an ordered list of entities, as `entityIDsFromAttributes` does.
//! 2. The tags of each entity, and then the global tags, give one value for each tag key. The first value wins.
//! 3. Each unified service tag (`service`, `env`, `version`) fills its OpenTelemetry attribute only when the resource
//!    has none of the attributes for that tag.
//! 4. Every other tag goes where [`CustomTags`] says, as `writeTagAttribute` does. A key with the
//!    `datadog.container.tag.` prefix, or a known convention key, always overwrites the resource attribute with
//!    that key.
//!
//! Each signal's translator then turns the resource attributes into tags, as it does for any resource attribute.

use otlp_protos::opentelemetry::proto::common::v1::KeyValue;
use saluki_core::data_model::{origin::OriginTagCardinality, tags::Tag};
use saluki_env::workload::EntityId;

use super::attributes::is_kubernetes_dd_tag;
use super::origin::OtlpOriginTagResolver;
use super::util::{
    first_attribute, is_custom_container_tag_key, is_known_convention_key, put_str_attribute,
    CUSTOM_CONTAINER_TAG_PREFIX,
};
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

/// How custom infrastructure tags are promoted into container tags (`ContainerTagPromotionMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContainerTagPromotion {
    /// Writes tags as-is. Only convention keys reach container tags; custom keys remain plain resource attributes.
    #[default]
    Off,

    /// Writes both the original key and a `datadog.container.tag.` prefixed copy.
    Duplicate,

    /// Writes only the `datadog.container.tag.` form.
    Rename,
}

/// Where [`process_tags`] puts a custom tag: a tag that isn't a unified service tag, has no `datadog.container.tag.`
/// prefix, and isn't a known convention key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomTags {
    /// Writes custom tags into the resource attributes, promoted as the mode says. A prefixed copy never replaces an
    /// attribute that the resource already has.
    Attributes(ContainerTagPromotion),

    /// Returns custom tags for the caller to send as `ddtags`. The logs pipeline uses it for
    /// `otlp_config.logs.infra_attributes.tags_as_ddtags`, as the Agent does for `logs_tags_as_ddtags`. A Kubernetes
    /// Datadog tag key still goes into the resource attributes.
    DdTags,
}

/// Writes the entity and global tags of an OTLP resource into its attributes, as `ProcessTags` does.
///
/// Returns the custom tags that [`CustomTags::DdTags`] keeps out of the attributes, in resolution order. The result is
/// empty for [`CustomTags::Attributes`].
///
/// Container ID detection comes later. Until then, the container found from the origin attributes comes first.
pub fn process_tags(
    attributes: &mut Vec<KeyValue>, resolver: &OtlpOriginTagResolver, cardinality: OriginTagCardinality,
    custom_tags: CustomTags,
) -> Vec<Tag> {
    // As in the Agent, any `container.id` attribute stops the origin lookup, even an empty or unknown one. The Agent
    // detects the container ID only when `Map.Get` finds no `container.id`.
    let origin_container = match first_attribute(attributes, CONTAINER_ID) {
        Some(_) => None,
        None => resolver.container_entity_from_origin_attributes(attributes, cardinality),
    };
    let entity_ids = origin_container
        .into_iter()
        .chain(entity_ids_from_attributes(attributes));
    let tags = resolver.resolve_ordered_tags(entity_ids, cardinality);
    let mut ddtags = Vec::new();

    for tag in tags {
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
            None => {
                if write_tag_attribute(attributes, key, value, custom_tags) {
                    ddtags.push(tag);
                }
            }
        }
    }

    ddtags
}

/// Writes a tag that isn't a unified service tag, as `writeTagAttribute` does.
///
/// Returns `true` if the tag goes to `ddtags` instead.
fn write_tag_attribute(attributes: &mut Vec<KeyValue>, key: &str, value: &str, custom_tags: CustomTags) -> bool {
    if is_custom_container_tag_key(key) || is_known_convention_key(key) {
        put_str_attribute(attributes, key, value);
        return false;
    }
    match custom_tags {
        CustomTags::DdTags if is_kubernetes_dd_tag(key) => put_str_attribute(attributes, key, value),
        CustomTags::DdTags => return true,
        CustomTags::Attributes(ContainerTagPromotion::Off) => put_str_attribute(attributes, key, value),
        CustomTags::Attributes(ContainerTagPromotion::Duplicate) => {
            put_str_attribute(attributes, key, value);
            put_prefixed_str_if_absent(attributes, key, value);
        }
        CustomTags::Attributes(ContainerTagPromotion::Rename) => put_prefixed_str_if_absent(attributes, key, value),
    }
    false
}

/// Writes the `datadog.container.tag.` copy of a tag unless the resource has that attribute (`putStrIfAbsent`).
fn put_prefixed_str_if_absent(attributes: &mut Vec<KeyValue>, key: &str, value: &str) {
    let prefixed = format!("{CUSTOM_CONTAINER_TAG_PREFIX}{key}");
    if first_attribute(attributes, &prefixed).is_none() {
        put_str_attribute(attributes, &prefixed, value);
    }
}

/// Returns the entities of an OTLP resource, in the order of `entityIDsFromAttributes`.
///
/// Every value of an entity attribute counts, whatever its type: it converts to a string with
/// [`otlp_value_as_string`], as `pdata`'s `AsString` does. Arrays and maps differ from `pdata` until #3397 lands.
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
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
        let ddtags = process_tags(
            &mut attributes,
            &resolver,
            OriginTagCardinality::Low,
            CustomTags::Attributes(ContainerTagPromotion::Off),
        );
        assert!(ddtags.is_empty());
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
            process_tags(
                &mut attributes,
                &origin_tag_resolver,
                OriginTagCardinality::Low,
                CustomTags::Attributes(ContainerTagPromotion::Off),
            );
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

    /// Each promotion mode writes a custom tag as `writeTagAttribute` does. Prefixed and known convention keys keep
    /// their key in every mode, and a prefixed copy doesn't replace an attribute that the resource already has.
    #[test]
    fn custom_tags_follow_each_promotion_mode() {
        let tagger_tags = [
            "team:platform",
            "owner:infra",
            "kube_namespace:ns",
            "datadog.container.tag.zone:a",
        ];
        let existing = || strings(&[("datadog.container.tag.owner", "sender")]);
        let cases = [
            (
                ContainerTagPromotion::Off,
                vec![
                    ("datadog.container.tag.owner", "sender"),
                    ("team", "platform"),
                    ("owner", "infra"),
                    ("kube_namespace", "ns"),
                    ("datadog.container.tag.zone", "a"),
                ],
            ),
            (
                ContainerTagPromotion::Duplicate,
                vec![
                    ("datadog.container.tag.owner", "sender"),
                    ("team", "platform"),
                    ("datadog.container.tag.team", "platform"),
                    ("owner", "infra"),
                    ("kube_namespace", "ns"),
                    ("datadog.container.tag.zone", "a"),
                ],
            ),
            (
                ContainerTagPromotion::Rename,
                vec![
                    ("datadog.container.tag.owner", "sender"),
                    ("datadog.container.tag.team", "platform"),
                    ("kube_namespace", "ns"),
                    ("datadog.container.tag.zone", "a"),
                ],
            ),
        ];
        for (promotion, expected) in cases {
            let provider = TestWorkloadProvider::with_entity(EntityId::Global, &tagger_tags);
            let mut attributes = existing();
            let ddtags = process_tags(
                &mut attributes,
                &OtlpOriginTagResolver::new(Arc::new(provider)),
                OriginTagCardinality::Low,
                CustomTags::Attributes(promotion),
            );
            assert!(ddtags.is_empty(), "promotion: {promotion:?}");
            assert_eq!(as_map(&attributes), string_map(&expected), "promotion: {promotion:?}");
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

    #[test]
    fn tags_as_ddtags_keeps_known_and_prefixed_keys_as_resource_attributes() {
        let provider = TestWorkloadProvider::with_entity(
            EntityId::Global,
            &[
                "datadog.container.tag.team:prefixed",
                "k8s.pod.name:convention-key",
                "kube_namespace:convention-name",
                "container.image.tags:image-tags",
                "eks_cluster_arn:eks",
                "env:prod",
                "custom:diverted",
            ],
        );
        let mut attributes = vec![];
        let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
        let ddtags = process_tags(
            &mut attributes,
            &resolver,
            OriginTagCardinality::Low,
            CustomTags::DdTags,
        );

        assert_eq!(ddtags, [Tag::from("custom:diverted")]);
        assert_eq!(
            as_map(&attributes),
            string_map(&[
                ("datadog.container.tag.team", "prefixed"),
                ("k8s.pod.name", "convention-key"),
                ("kube_namespace", "convention-name"),
                ("container.image.tags", "image-tags"),
                ("eks_cluster_arn", "eks"),
                ("deployment.environment", "prod"),
            ])
        );
    }
}

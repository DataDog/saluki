//! Container ID lookup from OTLP origin information.
//!
//! The lookup matches `GenerateContainerIDFromOriginInfo` in the Datadog Agent's tagger
//! (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/core/tagger/impl/tagger.go#L296-L353>).
//! It tries each source of origin information in this order, and skips a source whose value is zero or empty:
//!
//! 1. the process ID
//! 2. the cgroup inode
//! 3. the pod UID and the container name, as External Data
//!
//! The Agent asks its container runtime collectors and procfs for the container of a process ID, in an order that
//! depends on the deployment. This lookup asks the workload provider first, which knows the processes of containerd
//! containers, and then procfs. The order matches the tag lookup of process IDs in the workload provider. It also makes
//! a container that the workload provider tracks win over a cached procfs result, which stays cached while it's in
//! use and can be stale after the process ID is reused.
//!
//! The Agent's first step reads a container ID from Local Data. OTLP origin information never carries one, so the
//! lookup starts at the process ID.
//!
//! A container doesn't need tags to be found.
//!
//! [`inject_container_id`] reads the origin information from the resource attributes and adds `container.id`, as
//! `ProcessTags` and `originInfoFromAttributes` do in the Agent's infra attributes processor
//! (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/common.go>).

use std::{borrow::Cow, sync::Arc};

use opentelemetry_semantic_conventions::resource::{CONTAINER_ID, K8S_CONTAINER_NAME, K8S_POD_UID, PROCESS_PID};
use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, KeyValue};
use saluki_core::data_model::origin::RawExternalData;
use saluki_env::{
    workload::{CaptureEntityResolver, EntityId},
    WorkloadProvider,
};
use stringtheory::MetaString;

use super::util::{KEY_DATADOG_CONTAINER_CGROUP_INODE, KEY_DATADOG_CONTAINER_IS_INIT};
use super::value::otlp_value_as_string;

/// Origin information that identifies the container of an OTLP resource.
///
/// A zero or empty value means that the value is absent.
#[derive(Clone, Debug, Default)]
pub struct OriginInfo<'a> {
    /// The process ID.
    pub process_id: u32,

    /// The inode of the cgroup.
    pub cgroup_inode: u64,

    /// The UID of the pod.
    pub pod_uid: Cow<'a, str>,

    /// The name of the container in the pod.
    pub container_name: Cow<'a, str>,

    /// Whether the container is an init container.
    pub init_container: bool,
}

/// Looks up the container ID of an OTLP resource from its origin information.
#[derive(Clone)]
pub struct ContainerIdResolver {
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
}

impl ContainerIdResolver {
    /// Creates a new `ContainerIdResolver`.
    ///
    /// Without `capture_entity_resolver`, the lookup doesn't ask procfs for the container of a process ID.
    pub fn new(
        capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
        workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
    ) -> Self {
        Self {
            capture_entity_resolver,
            workload_provider,
        }
    }

    /// Returns the ID of the container that `origin` identifies.
    ///
    /// Returns `None` if no source of origin information identifies a container.
    pub fn resolve(&self, origin: &OriginInfo<'_>) -> Option<MetaString> {
        self.container_for_process(origin.process_id)
            .or_else(|| self.container_for_inode(origin.cgroup_inode))
            .or_else(|| self.container_for_external_data(origin))
            .and_then(|entity_id| match entity_id {
                EntityId::Container(container_id) => Some(container_id),
                _ => None,
            })
    }

    fn container_for_process(&self, process_id: u32) -> Option<EntityId> {
        if process_id == 0 {
            return None;
        }
        self.workload_provider
            .get_aliased_container_entity(&EntityId::ContainerPid(process_id))
            .filter(is_container_id)
            .or_else(|| {
                self.capture_entity_resolver
                    .as_deref()?
                    .resolve_container_entity_for_live_pid(process_id)
                    .filter(is_container_id)
            })
    }

    fn container_for_inode(&self, inode: u64) -> Option<EntityId> {
        if inode == 0 {
            return None;
        }
        self.workload_provider
            .get_aliased_container_entity(&EntityId::ContainerInode(inode))
            .filter(is_container_id)
    }

    fn container_for_external_data(&self, origin: &OriginInfo<'_>) -> Option<EntityId> {
        let (pod_uid, container_name) = (origin.pod_uid.as_ref(), origin.container_name.as_ref());
        if pod_uid.is_empty() || container_name.is_empty() {
            return None;
        }
        // The Agent's kubelet lookup ignores the init container flag, and searches all containers of the pod
        // (https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/pkg/util/containers/metrics/kubelet/collector.go#L106-L129).
        // The External Data store keys entries by the flag, so the lookup tries both values. Container names are
        // unique in a pod, so at most one value matches.
        [origin.init_container, !origin.init_container]
            .into_iter()
            .find_map(|init_container| {
                let external_data = RawExternalData::new(pod_uid, container_name, init_container);
                self.workload_provider
                    .get_container_entity_for_external_data(&external_data)
                    .filter(is_container_id)
            })
    }
}

impl<'a> OriginInfo<'a> {
    /// Reads origin information from OTLP resource attributes.
    ///
    /// As in the Agent, the process ID and the cgroup inode count only as integers, and the init container flag only as
    /// a boolean. An integer converts with two's-complement truncation, as Go's `uint32(...)` and `uint64(...)` do. The
    /// pod UID and the container name accept any type, and convert to a string as `pdata`'s `AsString` does.
    pub fn from_attributes(attributes: &'a [KeyValue]) -> Self {
        // Each slot holds the value of the first attribute with its key, as `Map.Get` does. A valueless attribute
        // fills its slot with `None`, so a later attribute with the same key doesn't count.
        let mut process_id = None;
        let mut pod_uid = None;
        let mut container_name = None;
        let mut cgroup_inode = None;
        let mut init_container = None;
        for attribute in attributes {
            let slot = match attribute.key.as_str() {
                PROCESS_PID => &mut process_id,
                K8S_POD_UID => &mut pod_uid,
                K8S_CONTAINER_NAME => &mut container_name,
                KEY_DATADOG_CONTAINER_CGROUP_INODE => &mut cgroup_inode,
                KEY_DATADOG_CONTAINER_IS_INIT => &mut init_container,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(attribute_value(attribute));
            }
        }

        let mut origin = Self::default();
        if let Some(Some(Value::IntValue(process_id))) = process_id {
            origin.process_id = *process_id as u32;
        }
        if let Some(value) = pod_uid {
            origin.pod_uid = otlp_value_as_string(value);
        }
        if let Some(value) = container_name {
            origin.container_name = otlp_value_as_string(value);
        }
        if let Some(Some(Value::IntValue(inode))) = cgroup_inode {
            origin.cgroup_inode = *inode as u64;
        }
        if let Some(Some(Value::BoolValue(is_init))) = init_container {
            origin.init_container = *is_init;
        }
        origin
    }
}

/// Adds `container.id` to OTLP resource attributes, if the key is absent and origin detection finds the container.
///
/// A `container.id` of any type, including an empty value, counts as present.
pub fn inject_container_id(attributes: &mut Vec<KeyValue>, resolver: &ContainerIdResolver) {
    if has_attribute(attributes, CONTAINER_ID) {
        return;
    }
    let Some(container_id) = resolver.resolve(&OriginInfo::from_attributes(attributes)) else {
        return;
    };
    attributes.push(KeyValue {
        key: CONTAINER_ID.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(container_id.to_string())),
        }),
    });
}

/// Returns `true` if an attribute with `key` is present, with or without a value, as `Map.Get` reports.
fn has_attribute(attributes: &[KeyValue], key: &str) -> bool {
    attributes.iter().any(|attribute| attribute.key == key)
}

/// Returns the value of an attribute, or `None` for an attribute without a value, which `pdata` reads as empty.
fn attribute_value(attribute: &KeyValue) -> Option<&Value> {
    attribute.value.as_ref().and_then(|value| value.value.as_ref())
}

/// Returns `true` if `entity_id` is a container with an ID that isn't empty.
///
/// As in the Agent, a step that finds an empty container ID falls through to the next step.
fn is_container_id(entity_id: &EntityId) -> bool {
    matches!(entity_id, EntityId::Container(id) if !id.is_empty())
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{
        origin::{OriginTagCardinality, RawOrigin},
        tags::SharedTagSet,
    };
    use saluki_env::workload::{
        origin::{ResolvedExternalData, ResolvedOrigin},
        providers::TestWorkloadProvider,
    };

    use super::*;

    fn container(id: &str) -> EntityId {
        EntityId::Container(id.into())
    }

    /// Builds a provider where each source of origin information identifies a different container, none with tags.
    fn provider() -> TestWorkloadProvider {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_live_process(42, container("pid-container"))
            .add_entity_alias(EntityId::ContainerInode(4242), container("inode-container"))
            .add_external_data("pod-uid", "app", false, container("app-container"))
            .add_external_data("pod-uid", "init", true, container("init-container"));
        provider
    }

    fn resolver(provider: TestWorkloadProvider) -> ContainerIdResolver {
        let provider = Arc::new(provider);
        ContainerIdResolver::new(Some(provider.clone()), provider)
    }

    #[test]
    fn resolves_each_source_for_a_container_without_tags() {
        let resolver = resolver(provider());
        let cases = [
            (
                "process ID",
                OriginInfo {
                    process_id: 42,
                    ..Default::default()
                },
                Some("pid-container"),
            ),
            (
                "cgroup inode",
                OriginInfo {
                    cgroup_inode: 4242,
                    ..Default::default()
                },
                Some("inode-container"),
            ),
            (
                "External Data",
                OriginInfo {
                    pod_uid: "pod-uid".into(),
                    container_name: "app".into(),
                    ..Default::default()
                },
                Some("app-container"),
            ),
            ("nothing", OriginInfo::default(), None),
            (
                "unknown values",
                OriginInfo {
                    process_id: 1,
                    cgroup_inode: 1,
                    pod_uid: "other-pod".into(),
                    container_name: "app".into(),
                    init_container: false,
                },
                None,
            ),
        ];

        for (name, origin, expected) in cases {
            assert_eq!(resolver.resolve(&origin).as_deref(), expected, "case: {name}");
        }
    }

    #[test]
    fn tries_sources_in_agent_order_and_falls_through_misses() {
        let resolver = resolver(provider());
        let all = OriginInfo {
            process_id: 42,
            cgroup_inode: 4242,
            pod_uid: "pod-uid".into(),
            container_name: "app".into(),
            init_container: false,
        };
        let cases = [
            ("process ID first", all.clone(), "pid-container"),
            (
                "inode after an unknown process ID",
                OriginInfo {
                    process_id: 7,
                    ..all.clone()
                },
                "inode-container",
            ),
            (
                "inode after a zero process ID",
                OriginInfo {
                    process_id: 0,
                    ..all.clone()
                },
                "inode-container",
            ),
            (
                "External Data after unknown process ID and inode",
                OriginInfo {
                    process_id: 7,
                    cgroup_inode: 7,
                    ..all.clone()
                },
                "app-container",
            ),
        ];

        for (name, origin, expected) in cases {
            assert_eq!(resolver.resolve(&origin).as_deref(), Some(expected), "case: {name}");
        }
    }

    #[test]
    fn external_data_needs_both_the_pod_uid_and_the_container_name() {
        let resolver = resolver(provider());
        for origin in [
            OriginInfo {
                pod_uid: "pod-uid".into(),
                ..Default::default()
            },
            OriginInfo {
                container_name: "app".into(),
                ..Default::default()
            },
        ] {
            assert_eq!(resolver.resolve(&origin), None, "origin: {origin:?}");
        }
    }

    #[test]
    fn external_data_ignores_the_init_container_flag() {
        let resolver = resolver(provider());
        for (container_name, init_container, expected) in [
            ("app", false, "app-container"),
            ("app", true, "app-container"),
            ("init", false, "init-container"),
            ("init", true, "init-container"),
        ] {
            let origin = OriginInfo {
                pod_uid: "pod-uid".into(),
                container_name: container_name.into(),
                init_container,
                ..Default::default()
            };
            assert_eq!(
                resolver.resolve(&origin).as_deref(),
                Some(expected),
                "origin: {origin:?}"
            );
        }
    }

    /// Regression test: the External Data step went through the origin cache, which keeps a resolved origin, misses
    /// included, while it's in use. A container that started after a miss, or that replaced another one, then never
    /// showed up while logs kept arriving.
    #[test]
    fn external_data_reads_current_workload_metadata_not_the_origin_cache() {
        struct StaleOriginCache;

        impl WorkloadProvider for StaleOriginCache {
            fn get_tags_for_entity(&self, _: &EntityId, _: OriginTagCardinality) -> Option<SharedTagSet> {
                None
            }

            fn get_resolved_origin(&self, _: RawOrigin<'_>) -> Option<ResolvedOrigin> {
                Some(ResolvedOrigin::from_parts(
                    None,
                    None,
                    None,
                    None,
                    Some(ResolvedExternalData::new(
                        EntityId::PodUid("pod-uid".into()),
                        container("previous-container"),
                    )),
                ))
            }

            fn get_container_entity_for_external_data(&self, external_data: &RawExternalData<'_>) -> Option<EntityId> {
                (external_data == &RawExternalData::new("pod-uid", "app", false))
                    .then(|| container("current-container"))
            }
        }

        let resolver = ContainerIdResolver::new(None, Arc::new(StaleOriginCache));
        let origin = OriginInfo {
            pod_uid: "pod-uid".into(),
            container_name: "app".into(),
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("current-container"));
    }

    #[test]
    fn external_data_matches_only_the_exact_values() {
        // The lookup passes the pod UID and the container name as they are, as the Agent does, so a value with a comma
        // can't make another container match.
        let resolver = resolver(provider());
        let origin = OriginInfo {
            pod_uid: "pod-uid".into(),
            container_name: "other,cn-app".into(),
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin), None);
    }

    #[test]
    fn an_empty_container_id_falls_through_to_the_next_source() {
        let mut provider = provider();
        provider
            .add_live_process(1, container(""))
            .add_entity_alias(EntityId::ContainerInode(1), container(""));
        let resolver = resolver(provider);
        let origin = OriginInfo {
            process_id: 1,
            cgroup_inode: 1,
            pod_uid: "pod-uid".into(),
            container_name: "app".into(),
            init_container: false,
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("app-container"));
    }

    #[test]
    fn prefers_the_workload_provider_for_the_process_id() {
        let mut provider = provider();
        provider.add_entity_alias(EntityId::ContainerPid(42), container("containerd-container"));
        let resolver = resolver(provider);
        let origin = OriginInfo {
            process_id: 42,
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("containerd-container"));
    }

    #[test]
    fn finds_the_process_id_in_the_workload_provider_without_a_capture_entity_resolver() {
        let mut provider = provider();
        provider.add_entity_alias(EntityId::ContainerPid(7), container("containerd-container"));
        let resolver = ContainerIdResolver::new(None, Arc::new(provider));
        let origin = OriginInfo {
            process_id: 7,
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("containerd-container"));
    }

    #[test]
    fn an_empty_container_id_for_one_init_flag_falls_through_to_the_other() {
        let mut provider = TestWorkloadProvider::new();
        provider
            .add_external_data("pod-uid", "app", false, container(""))
            .add_external_data("pod-uid", "app", true, container("init-container"));
        let resolver = resolver(provider);
        let origin = OriginInfo {
            pod_uid: "pod-uid".into(),
            container_name: "app".into(),
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("init-container"));
    }

    #[test]
    fn skips_procfs_without_a_capture_entity_resolver() {
        let resolver = ContainerIdResolver::new(None, Arc::new(provider()));
        let origin = OriginInfo {
            process_id: 42,
            cgroup_inode: 4242,
            ..Default::default()
        };
        assert_eq!(resolver.resolve(&origin).as_deref(), Some("inode-container"));
    }

    fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    /// Builds a provider for the injection matrix, where each container has no tags.
    fn matrix_provider() -> TestWorkloadProvider {
        let mut provider = provider();
        provider
            .add_live_process(u32::MAX, container("max-pid-container"))
            .add_entity_alias(EntityId::ContainerInode(u64::MAX), container("max-inode-container"))
            .add_external_data("123", "456", false, container("int-container"))
            .add_external_data("true", "false", false, container("bool-container"));
        provider
    }

    #[test]
    fn injection_matrix() {
        // `None` means that the attributes don't change, and `Some` gives the injected `container.id`.
        use Value::{BoolValue, DoubleValue, IntValue, StringValue};

        let pid = |value| attribute(PROCESS_PID, value);
        let inode = |value| attribute(KEY_DATADOG_CONTAINER_CGROUP_INODE, value);
        let pod_uid = |value| attribute(K8S_POD_UID, value);
        let container_name = |value| attribute(K8S_CONTAINER_NAME, value);
        let string = |value: &str| StringValue(value.to_owned());
        let external_data = || vec![pod_uid(string("pod-uid")), container_name(string("app"))];

        let cases: Vec<(&str, Vec<KeyValue>, Option<&str>)> = vec![
            // `container.id` counts as present with any type, even when it's empty.
            ("missing container.id", vec![pid(IntValue(42))], Some("pid-container")),
            (
                "empty string container.id",
                vec![attribute(CONTAINER_ID, string("")), pid(IntValue(42))],
                None,
            ),
            (
                "integer container.id",
                vec![attribute(CONTAINER_ID, IntValue(7)), pid(IntValue(42))],
                None,
            ),
            (
                "boolean container.id",
                vec![pid(IntValue(42)), attribute(CONTAINER_ID, BoolValue(false))],
                None,
            ),
            (
                "container.id without a value",
                vec![
                    KeyValue {
                        key: CONTAINER_ID.to_owned(),
                        value: None,
                    },
                    pid(IntValue(42)),
                ],
                None,
            ),
            ("nothing to detect from", vec![], None),
            // The process ID counts only as an integer, truncated as Go's `uint32(...)` does.
            ("negative PID", vec![pid(IntValue(-1))], Some("max-pid-container")),
            (
                "overflowing PID",
                vec![pid(IntValue((1 << 32) + 42))],
                Some("pid-container"),
            ),
            ("PID that truncates to zero", vec![pid(IntValue(1 << 32))], None),
            ("zero PID", vec![pid(IntValue(0))], None),
            ("string PID", vec![pid(string("42"))], None),
            ("double PID", vec![pid(DoubleValue(42.0))], None),
            // The cgroup inode counts only as an integer, converted as Go's `uint64(...)` does.
            ("negative inode", vec![inode(IntValue(-1))], Some("max-inode-container")),
            ("zero inode", vec![inode(IntValue(0))], None),
            ("string inode", vec![inode(string("4242"))], None),
            // External Data needs both the pod UID and the container name.
            ("pod UID only", vec![pod_uid(string("pod-uid"))], None),
            ("container name only", vec![container_name(string("app"))], None),
            (
                "empty pod UID",
                vec![pod_uid(string("")), container_name(string("app"))],
                None,
            ),
            ("pod UID and container name", external_data(), Some("app-container")),
            (
                "integer pod UID and container name",
                vec![pod_uid(IntValue(123)), container_name(IntValue(456))],
                Some("int-container"),
            ),
            (
                "boolean pod UID and container name",
                vec![pod_uid(BoolValue(true)), container_name(BoolValue(false))],
                Some("bool-container"),
            ),
            // Sources go in the Agent's order: process ID, cgroup inode, then External Data.
            (
                "all sources",
                [vec![pid(IntValue(42)), inode(IntValue(4242))], external_data()].concat(),
                Some("pid-container"),
            ),
            (
                "unknown PID before inode",
                [vec![pid(IntValue(7)), inode(IntValue(4242))], external_data()].concat(),
                Some("inode-container"),
            ),
            (
                "unknown PID and inode before External Data",
                [vec![pid(IntValue(7)), inode(IntValue(7))], external_data()].concat(),
                Some("app-container"),
            ),
            // Only the first attribute with a key counts, as in `pdata`'s `Map.Get`.
            (
                "string PID before integer PID",
                vec![pid(string("42")), pid(IntValue(42))],
                None,
            ),
            (
                "valueless PID before integer PID",
                vec![
                    KeyValue {
                        key: PROCESS_PID.to_owned(),
                        value: None,
                    },
                    pid(IntValue(42)),
                ],
                None,
            ),
        ];

        let resolver = resolver(matrix_provider());
        for (name, attributes, expected) in cases {
            let mut injected = attributes.clone();
            inject_container_id(&mut injected, &resolver);

            let outcome = if injected == attributes {
                None
            } else {
                assert_eq!(injected.len(), attributes.len() + 1, "case: {name}");
                assert_eq!(injected[..attributes.len()], attributes[..], "case: {name}");
                match injected.last() {
                    Some(KeyValue {
                        key,
                        value:
                            Some(AnyValue {
                                value: Some(StringValue(value)),
                            }),
                    }) if key == CONTAINER_ID => Some(value.as_str()),
                    other => panic!("case {name}: unexpected attribute {other:?}"),
                }
            };
            assert_eq!(outcome, expected, "case: {name}");
        }
    }

    #[test]
    fn reads_the_init_container_flag_only_as_a_boolean() {
        for (value, expected) in [
            (Value::BoolValue(true), true),
            (Value::BoolValue(false), false),
            (Value::StringValue("true".into()), false),
            (Value::IntValue(1), false),
        ] {
            let attributes = [attribute(KEY_DATADOG_CONTAINER_IS_INIT, value.clone())];
            assert_eq!(
                OriginInfo::from_attributes(&attributes).init_container,
                expected,
                "value: {value:?}"
            );
        }
    }

    /// The pod UID and the container name convert doubles, bytes, arrays, and maps as `pdata`'s `AsString` does.
    #[test]
    fn converts_other_value_types_as_pdata_does() {
        use otlp_protos::opentelemetry::proto::common::v1::{ArrayValue, KeyValueList};

        let mut provider = TestWorkloadProvider::new();
        provider
            .add_external_data("1.5", "app", false, container("double-container"))
            .add_external_data("AQI=", "app", false, container("bytes-container"))
            .add_external_data(r#"["pod"]"#, "app", false, container("array-container"))
            .add_external_data(r#"{"pod":"uid"}"#, "app", false, container("map-container"));
        let resolver = resolver(provider);

        let cases = [
            (Value::DoubleValue(1.5), "double-container"),
            (Value::BytesValue(vec![1, 2]), "bytes-container"),
            (
                Value::ArrayValue(ArrayValue {
                    values: vec![AnyValue {
                        value: Some(Value::StringValue("pod".into())),
                    }],
                }),
                "array-container",
            ),
            (
                Value::KvlistValue(KeyValueList {
                    values: vec![attribute("pod", Value::StringValue("uid".into()))],
                }),
                "map-container",
            ),
        ];
        for (pod_uid, expected) in cases {
            let attributes = [
                attribute(K8S_POD_UID, pod_uid.clone()),
                attribute(K8S_CONTAINER_NAME, Value::StringValue("app".into())),
            ];
            assert_eq!(
                resolver.resolve(&OriginInfo::from_attributes(&attributes)).as_deref(),
                Some(expected),
                "pod UID: {pod_uid:?}"
            );
        }
    }
}

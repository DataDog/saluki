//! OTLP semantic conventions for attributes.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::{resource::*, trace::*};
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::{origin::RawOrigin, tags::TagSet};

pub mod source;
pub mod translator;

const CUSTOM_CONTAINER_TAG_PREFIX: &str = "datadog.container.tag.";

static CORE_MAPPING: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = FastHashMap::default();
    m.insert("deployment.environment", "env"); // For older semconv versions
    m.insert(DEPLOYMENT_ENVIRONMENT_NAME, "env");
    m.insert(SERVICE_NAME, "service");
    m.insert(SERVICE_VERSION, "version");
    m
});

static CONTAINER_MAPPINGS: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = FastHashMap::default();
    // Containers
    m.insert(CONTAINER_ID, "container_id");
    m.insert(CONTAINER_NAME, "container_name");
    m.insert(CONTAINER_IMAGE_NAME, "image_name");
    m.insert("container.image.tag", "image_tag"); // For older semconv versions
    m.insert(CONTAINER_RUNTIME, "runtime");

    // Cloud conventions
    // https://www.datadoghq.com/blog/tagging-best-practices/
    m.insert(CLOUD_PROVIDER, "cloud_provider");
    m.insert(CLOUD_REGION, "region");
    m.insert(CLOUD_AVAILABILITY_ZONE, "zone");

    // ECS conventions
    // https://github.com/DataDog/datadog-agent/blob/e081bed/pkg/tagger/collectors/ecs_extract.go
    m.insert(AWS_ECS_TASK_FAMILY, "task_family");
    m.insert(AWS_ECS_TASK_ARN, "task_arn");
    m.insert(AWS_ECS_CLUSTER_ARN, "ecs_cluster_name");
    m.insert(AWS_ECS_TASK_REVISION, "task_version");
    m.insert(AWS_ECS_CONTAINER_ARN, "ecs_container_name");

    // Kubernetes resource name (via semantic conventions)
    // https://github.com/DataDog/datadog-agent/blob/e081bed/pkg/util/kubernetes/const.go
    m.insert(K8S_CONTAINER_NAME, "kube_container_name");
    m.insert(K8S_CLUSTER_NAME, "kube_cluster_name");
    m.insert(K8S_DEPLOYMENT_NAME, "kube_deployment");
    m.insert(K8S_REPLICASET_NAME, "kube_replica_set");
    m.insert(K8S_STATEFULSET_NAME, "kube_stateful_set");
    m.insert(K8S_DAEMONSET_NAME, "kube_daemon_set");
    m.insert(K8S_JOB_NAME, "kube_job");
    m.insert(K8S_CRONJOB_NAME, "kube_cronjob");
    m.insert(K8S_NAMESPACE_NAME, "kube_namespace");
    m.insert(K8S_POD_NAME, "pod_name");
    m
});

// Kubernetes mappings defines the mapping between Kubernetes conventions (both general and Datadog specific)
// and Datadog Agent conventions. The Datadog Agent conventions can be found at
// https://github.com/DataDog/datadog-agent/blob/e081bed/pkg/tagger/collectors/const.go and
// https://github.com/DataDog/datadog-agent/blob/e081bed/pkg/util/kubernetes/const.go
static KUBERNETES_MAPPING: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = FastHashMap::default();
    // Standard Datadog labels
    m.insert("tags.datadoghq.com/env", "env");
    m.insert("tags.datadoghq.com/service", "service");
    m.insert("tags.datadoghq.com/version", "version");

    // Standard Kubernetes labels
    m.insert("app.kubernetes.io/name", "kube_app_name");
    m.insert("app.kubernetes.io/instance", "kube_app_instance");
    m.insert("app.kubernetes.io/version", "kube_app_version");
    m.insert("app.kubernetes.io/component", "kube_app_component");
    m.insert("app.kubernetes.io/part-of", "kube_app_part_of");
    m.insert("app.kubernetes.io/managed-by", "kube_app_managed_by");
    m
});

// Kubernetes out of the box Datadog tags
// https://docs.datadoghq.com/containers/kubernetes/tag/?tab=containerizedagent#out-of-the-box-tags
// https://github.com/DataDog/datadog-agent/blob/d33d042d6786e8b85f72bb627fbf06ad8a658031/comp/core/tagger/taggerimpl/collectors/workloadmeta_extract.go
// Note: if any OTel semantics happen to overlap with these tag names, they will also be added as Datadog tags.
static KUBERNETES_DD_TAGS: LazyLock<FastHashSet<&'static str>> = LazyLock::new(|| {
    let tags = vec![
        "architecture",
        "availability-zone",
        "chronos_job",
        "chronos_job_owner",
        "cluster_name",
        "container_id",
        "container_name",
        "dd_remote_config_id",
        "dd_remote_config_rev",
        "display_container_name",
        "docker_image",
        "ecs_cluster_name",
        "ecs_container_name",
        "eks_fargate_node",
        "env",
        "git.commit.sha",
        "git.repository_url",
        "image_id",
        "image_name",
        "image_tag",
        "kube_app_component",
        "kube_app_instance",
        "kube_app_managed_by",
        "kube_app_name",
        "kube_app_part_of",
        "kube_app_version",
        "kube_container_name",
        "kube_cronjob",
        "kube_daemon_set",
        "kube_deployment",
        "kube_job",
        "kube_namespace",
        "kube_ownerref_kind",
        "kube_ownerref_name",
        "kube_priority_class",
        "kube_qos",
        "kube_replica_set",
        "kube_replication_controller",
        "kube_service",
        "kube_stateful_set",
        "language",
        "marathon_app",
        "mesos_task",
        "nomad_dc",
        "nomad_group",
        "nomad_job",
        "nomad_namespace",
        "nomad_task",
        "oshift_deployment",
        "oshift_deployment_config",
        "os_name",
        "os_version",
        "persistentvolumeclaim",
        "pod_name",
        "pod_phase",
        "rancher_container",
        "rancher_service",
        "rancher_stack",
        "region",
        "service",
        "short_image",
        "swarm_namespace",
        "swarm_service",
        "task_name",
        "task_family",
        "task_version",
        "task_arn",
        "version",
    ];
    tags.into_iter().collect()
});

// HTTPMappings defines the mapping between OpenTelemetry semantic conventions
// and Datadog Agent conventions for HTTP attributes.
#[allow(dead_code)]
static HTTP_MAPPINGS: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = FastHashMap::default();
    m.insert(CLIENT_ADDRESS, "http.client_ip");
    m.insert(HTTP_RESPONSE_BODY_SIZE, "http.response.content_length");
    m.insert(HTTP_RESPONSE_STATUS_CODE, "http.status_code");
    m.insert(HTTP_REQUEST_BODY_SIZE, "http.request.content_length");
    m.insert("http.request.header.referrer", "http.referrer"); // No semconv for this one
    m.insert(HTTP_REQUEST_METHOD, "http.method");
    m.insert(HTTP_ROUTE, "http.route");
    m.insert(NETWORK_PROTOCOL_VERSION, "http.version");
    m.insert(SERVER_ADDRESS, "http.server_name");
    m.insert(URL_FULL, "http.url");
    m.insert(USER_AGENT_ORIGINAL, "http.useragent");
    m
});

pub(crate) fn extract_container_tags_from_resource_attributes(attributes: &[otlp_common::KeyValue], tags: &mut TagSet) {
    let mut extracted_tags = FastHashSet::default();

    for kv in attributes {
        if let Some(Value::StringValue(s_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            if s_val.is_empty() {
                continue;
            }

            // Semantic Conventions
            if let Some(datadog_key) = CONTAINER_MAPPINGS.get(kv.key.as_str()) {
                tags.insert_tag(format!("{}:{}", datadog_key, s_val));
                extracted_tags.insert(*datadog_key);
            }

            // Custom (datadog.container.tag namespace)
            if kv.key.starts_with(CUSTOM_CONTAINER_TAG_PREFIX) {
                if let Some(custom_key) = kv.key.get(CUSTOM_CONTAINER_TAG_PREFIX.len()..) {
                    if !custom_key.is_empty() {
                        // Do not replace if set via semantic conventions mappings.
                        if !extracted_tags.insert(custom_key) {
                            tags.insert_tag(format!("{}:{}", custom_key, s_val));
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn tags_from_attributes(attributes: &[otlp_common::KeyValue]) -> TagSet {
    let mut tags = TagSet::default();

    for kv in attributes {
        match (kv.key.as_str(), kv.value.as_ref().and_then(|v| v.value.as_ref())) {
            // Process attributes
            (PROCESS_EXECUTABLE_NAME, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_EXECUTABLE_NAME, s_val));
            }
            (PROCESS_EXECUTABLE_PATH, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_EXECUTABLE_PATH, s_val));
            }
            (PROCESS_COMMAND, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_COMMAND, s_val));
            }
            (PROCESS_COMMAND_LINE, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_COMMAND_LINE, s_val));
            }
            (PROCESS_PID, Some(Value::IntValue(i_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_PID, i_val));
            }
            (PROCESS_OWNER, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", PROCESS_OWNER, s_val));
            }

            // System attributes
            (OS_TYPE, Some(Value::StringValue(s_val))) => {
                tags.insert_tag(format!("{}:{}", OS_TYPE, s_val));
            }

            // Other mappings
            (key, Some(Value::StringValue(s_val))) => {
                if s_val.is_empty() {
                    continue;
                }

                // core attributes mapping
                if let Some(datadog_key) = CORE_MAPPING.get(key) {
                    tags.insert_tag(format!("{}:{}", datadog_key, s_val));
                }

                // Kubernetes labels mapping
                if let Some(datadog_key) = KUBERNETES_MAPPING.get(key) {
                    tags.insert_tag(format!("{}:{}", datadog_key, s_val));
                }

                // Kubernetes DD tags
                if KUBERNETES_DD_TAGS.contains(key) {
                    tags.insert_tag(format!("{}:{}", key, s_val));
                }
            }
            _ => {}
        }
    }

    // Container Tag mappings
    extract_container_tags_from_resource_attributes(attributes, &mut tags);

    tags
}

#[allow(dead_code)]
pub(super) fn origin_id_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let mut pod_uid = None;

    for kv in attributes {
        if let Some(Value::StringValue(s_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            match kv.key.as_str() {
                CONTAINER_ID => {
                    // Container ID is preferred.
                    return Some(format!("container_id://{}", s_val));
                }
                K8S_POD_UID => {
                    pod_uid = Some(s_val.as_str());
                }
                _ => {}
            }
        }
    }

    // Fallback to Kubernetes pod UID.
    pod_uid.map(|uid| format!("kubernetes_pod_uid://{}", uid))
}

/// Creates a `RawOrigin` from OTLP resource attributes.
pub fn raw_origin_from_attributes<'a>(attributes: &'a [otlp_common::KeyValue]) -> RawOrigin<'a> {
    let mut origin = RawOrigin::default();

    for kv in attributes {
        match kv.key.as_str() {
            CONTAINER_ID => {
                if let Some(value) = try_get_string_from_value(kv.value.as_ref().and_then(|v| v.value.as_ref())) {
                    origin.set_container_id(value);
                }
            }
            K8S_POD_UID => {
                if let Some(value) = try_get_string_from_value(kv.value.as_ref().and_then(|v| v.value.as_ref())) {
                    origin.set_pod_uid(value);
                }
            }
            PROCESS_PID => {
                if let Some(pid) = try_get_int_from_value(kv.value.as_ref().and_then(|v| v.value.as_ref())) {
                    origin.set_process_id(pid as u32);
                }
            }
            _ => {}
        }
    }

    origin
}

fn try_get_string_from_value(value: Option<&otlp_common::any_value::Value>) -> Option<&str> {
    if let Some(otlp_common::any_value::Value::StringValue(s)) = value {
        Some(s.as_str())
    } else {
        None
    }
}

fn try_get_int_from_value(value: Option<&otlp_common::any_value::Value>) -> Option<i64> {
    if let Some(otlp_common::any_value::Value::IntValue(i)) = value {
        Some(*i)
    } else {
        None
    }
}

pub(super) fn get_string_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
    attributes.iter().find_map(|kv| {
        if kv.key == key {
            if let Some(Value::StringValue(s_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(s_val.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

pub(super) fn get_int_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a i64> {
    attributes.iter().find_map(|kv| {
        if kv.key == key {
            if let Some(Value::IntValue(i_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(i_val)
            } else {
                None
            }
        } else {
            None
        }
    })
}

pub(super) fn resource_to_source(
    resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource,
) -> Option<source::Source> {
    let attributes = &resource.attributes;

    // AWS ECS Fargate
    if get_string_attribute(attributes, CLOUD_PROVIDER) == Some("aws")
        && get_string_attribute(attributes, opentelemetry_semantic_conventions::resource::CLOUD_PLATFORM)
            == Some("aws_ecs")
        && get_string_attribute(
            attributes,
            opentelemetry_semantic_conventions::resource::AWS_ECS_LAUNCHTYPE,
        ) == Some("fargate")
    {
        if let Some(task_arn) = get_string_attribute(attributes, AWS_ECS_TASK_ARN) {
            return Some(source::Source {
                kind: source::SourceKind::AwsEcsFargateKind,
                identifier: task_arn.to_string(),
            });
        }
    }

    // Hostname from attributes
    if let Some(host_name) = get_string_attribute(attributes, opentelemetry_semantic_conventions::resource::HOST_NAME) {
        return Some(source::Source {
            kind: source::SourceKind::HostnameKind,
            identifier: host_name.to_string(),
        });
    }

    None
}

//! OTLP semantic conventions for attributes.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::{resource::*, trace::*};
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::{origin::RawOrigin, tags::TagSet};

use crate::common::otlp::util::{extract_container_tags_from_resource_attributes, resource_to_source};

pub mod source;
pub mod translator;

static CORE_MAPPING: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = FastHashMap::default();
    m.insert("deployment.environment", "env"); // For older semconv versions
    m.insert(DEPLOYMENT_ENVIRONMENT_NAME, "env");
    m.insert(SERVICE_NAME, "service");
    m.insert(SERVICE_VERSION, "version");
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
pub(crate) static HTTP_MAPPINGS: LazyLock<FastHashMap<&'static str, &'static str>> = LazyLock::new(|| {
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
                    origin.set_local_data(value);
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

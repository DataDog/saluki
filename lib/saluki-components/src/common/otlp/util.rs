//! Shared OTLP utility functions and types.
//!
//! This module contains helpers used by both the OTLP source (translator) and the Datadog trace encoder.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::resource::*;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::tags::TagSet;

// ============================================================================
// Datadog attribute key constants shared across the encoder and translator
// ============================================================================

pub const KEY_DATADOG_VERSION: &str = "datadog.version";
pub const KEY_DATADOG_HOST: &str = "datadog.host";
pub const KEY_DATADOG_ENVIRONMENT: &str = "datadog.env";
pub const KEY_DATADOG_CONTAINER_ID: &str = "datadog.container_id";
pub const KEY_DATADOG_CONTAINER_TAGS: &str = "datadog.container_tags";
pub const DEPLOYMENT_ENVIRONMENT_KEY: &str = "deployment.environment";

/// The kind of source that produced telemetry data.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    /// Hostname-based source.
    HostnameKind,
    /// AWS ECS Fargate source.
    AwsEcsFargateKind,
}

/// Represents the source of telemetry data.
#[derive(Debug, Clone)]
pub struct Source {
    /// The kind of source.
    pub kind: SourceKind,
    /// The identifier for this source.
    pub identifier: String,
}

impl Source {
    /// Returns a tag representation of this source.
    pub fn tag(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.identifier)
    }
}

impl SourceKind {
    fn as_str(&self) -> &'static str {
        match self {
            SourceKind::HostnameKind => "host",
            SourceKind::AwsEcsFargateKind => "task_arn",
        }
    }
}

const CUSTOM_CONTAINER_TAG_PREFIX: &str = "datadog.container.tag.";

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

// ============================================================================
// Attribute helper functions
// ============================================================================

/// Extracts a string attribute value from OTLP KeyValue attributes by key.
pub fn get_string_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
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

/// Extracts container tags from OTLP resource attributes and inserts them into the provided TagSet.
/// This function is based on the agent implementation here
/// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/attributes/attributes.go#L277
pub fn extract_container_tags_from_resource_attributes(attributes: &[otlp_common::KeyValue], tags: &mut TagSet) {
    let mut extracted_tags = FastHashSet::default();

    for kv in attributes {
        if let Some(Value::StringValue(s_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
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

/// Extracts container tags from a resource tagset and inserts them into the provided TagSet.
///
/// This mirrors `extract_container_tags_from_resource_attributes`, but operates on a `TagSet` representation of the
/// resource.
pub fn extract_container_tags_from_resource_tagset(resource_tags: &TagSet, tags: &mut TagSet) {
    let mut extracted_tags = FastHashSet::default();

    for tag in resource_tags {
        let Some(value) = tag.value() else {
            continue;
        };

        // Semantic Conventions
        if let Some(datadog_key) = CONTAINER_MAPPINGS.get(tag.name()) {
            tags.insert_tag(format!("{}:{}", datadog_key, value));
            extracted_tags.insert(*datadog_key);
        }

        // Custom (datadog.container.tag namespace)
        if tag.name().starts_with(CUSTOM_CONTAINER_TAG_PREFIX) {
            if let Some(custom_key) = tag.name().get(CUSTOM_CONTAINER_TAG_PREFIX.len()..) {
                if !custom_key.is_empty() {
                    // Do not replace if set via semantic conventions mappings.
                    if !extracted_tags.insert(custom_key) {
                        tags.insert_tag(format!("{}:{}", custom_key, value));
                    }
                }
            }
        }
    }
}

/// Resolves the source metadata from OTLP resource attributes.
///
/// This determines whether the telemetry came from a hostname or serverless environment.
pub fn resource_to_source(resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource) -> Option<Source> {
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
            return Some(Source {
                kind: SourceKind::AwsEcsFargateKind,
                identifier: task_arn.to_string(),
            });
        }
    }

    // Hostname from attributes
    if let Some(host_name) = get_string_attribute(attributes, opentelemetry_semantic_conventions::resource::HOST_NAME) {
        return Some(Source {
            kind: SourceKind::HostnameKind,
            identifier: host_name.to_string(),
        });
    }

    None
}

/// Resolves the source metadata from a resource `TagSet`.
///
/// This is equivalent to `resource_to_source`, but avoids the OTLP protobuf resource type.
pub fn tags_to_source(resource_tags: &TagSet) -> Option<Source> {
    let get = |key: &str| -> Option<&str> { resource_tags.get_single_tag(key).and_then(|t| t.value()) };

    // AWS ECS Fargate
    if get(CLOUD_PROVIDER) == Some("aws")
        && get(opentelemetry_semantic_conventions::resource::CLOUD_PLATFORM) == Some("aws_ecs")
        && get(opentelemetry_semantic_conventions::resource::AWS_ECS_LAUNCHTYPE) == Some("fargate")
    {
        if let Some(task_arn) = get(AWS_ECS_TASK_ARN) {
            return Some(Source {
                kind: SourceKind::AwsEcsFargateKind,
                identifier: task_arn.to_string(),
            });
        }
    }

    // Hostname from attributes
    if let Some(host_name) = get(opentelemetry_semantic_conventions::resource::HOST_NAME) {
        return Some(Source {
            kind: SourceKind::HostnameKind,
            identifier: host_name.to_string(),
        });
    }

    None
}

//! Shared OTLP utility functions and types.
//!
//! This module contains helpers used by both the OTLP source (translator) and the Datadog trace encoder.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::resource::*;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::trace::{
    AttributeScalarValue, AttributeValue, EntityRef as CoreEntityRef, KeyValue as CoreKeyValue,
};
use stringtheory::MetaString;

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

/// Extracts a string attribute value from ADP KeyValue attributes by key.
pub fn get_string_attribute_adp<'a>(attributes: &'a [CoreKeyValue], key: &str) -> Option<&'a str> {
    attributes.iter().find_map(|kv| {
        if kv.key.as_ref() == key {
            match kv.value.as_ref()? {
                AttributeValue::String(s) => Some(s.as_ref()),
                _ => None,
            }
        } else {
            None
        }
    })
}

/// Extracts a string attribute value from ADP KeyValue attributes by trying multiple keys.
pub fn get_string_attribute_from_list_adp<'a>(attributes: &'a [CoreKeyValue], keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(value) = get_string_attribute_adp(attributes, key) {
            return Some(value);
        }
    }
    None
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

/// Extracts container tags from ADP resource attributes and inserts them into the provided TagSet.
pub fn extract_container_tags_from_adp_resource_attributes(attributes: &[CoreKeyValue], tags: &mut TagSet) {
    let mut extracted_tags = FastHashSet::default();

    for kv in attributes {
        if let Some(AttributeValue::String(s_val)) = &kv.value {
            // Semantic Conventions
            if let Some(datadog_key) = CONTAINER_MAPPINGS.get(kv.key.as_ref()) {
                tags.insert_tag(format!("{}:{}", datadog_key, s_val));
                extracted_tags.insert(*datadog_key);
            }

            // Custom (datadog.container.tag namespace)
            if kv.key.as_ref().starts_with(CUSTOM_CONTAINER_TAG_PREFIX) {
                if let Some(custom_key) = kv.key.as_ref().get(CUSTOM_CONTAINER_TAG_PREFIX.len()..) {
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

/// Resolves the source metadata from OTLP resource attributes.
///
/// This determines whether the telemetry came from a hostname or serverless environment.
pub fn resource_attributes_to_source(attributes: &[otlp_common::KeyValue]) -> Option<Source> {
    // AWS ECS Fargate
    if get_string_attribute(attributes, CLOUD_PROVIDER) == Some("aws")
        && get_string_attribute(attributes, CLOUD_PLATFORM)
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
    if let Some(host_name) = get_string_attribute(attributes, HOST_NAME) {
        return Some(Source {
            kind: SourceKind::HostnameKind,
            identifier: host_name.to_string(),
        });
    }

    None
}

/// Resolves the source metadata from ADP resource attributes.
pub fn resource_attributes_to_source_adp(attributes: &[CoreKeyValue]) -> Option<Source> {
    // AWS ECS Fargate
    if get_string_attribute_adp(attributes, CLOUD_PROVIDER) == Some("aws")
        && get_string_attribute_adp(attributes, CLOUD_PLATFORM) == Some("aws_ecs")
        && get_string_attribute_adp(attributes, AWS_ECS_LAUNCHTYPE) == Some("fargate")
    {
        if let Some(task_arn) = get_string_attribute_adp(attributes, AWS_ECS_TASK_ARN) {
            return Some(Source {
                kind: SourceKind::AwsEcsFargateKind,
                identifier: task_arn.to_string(),
            });
        }
    }

    // Hostname from attributes
    if let Some(host_name) = get_string_attribute_adp(attributes, HOST_NAME) {
        return Some(Source {
            kind: SourceKind::HostnameKind,
            identifier: host_name.to_string(),
        });
    }

    None
}

/// Converts an OTLP AnyValue to an ADP AttributeValue.
///
/// Returns `None` for unsupported types (bytes, kvlist) or if the value is unset.
pub fn otlp_any_value_to_adp(value: &otlp_common::AnyValue) -> Option<AttributeValue> {
    use otlp_common::any_value::Value;

    match value.value.as_ref()? {
        Value::StringValue(s) => Some(AttributeValue::String(MetaString::from(s.as_str()))),
        Value::BoolValue(b) => Some(AttributeValue::Bool(*b)),
        Value::IntValue(i) => Some(AttributeValue::Int(*i)),
        Value::DoubleValue(d) => Some(AttributeValue::Double(*d)),
        Value::ArrayValue(arr) => {
            // Convert array elements, filtering out unsupported types (nested arrays/kvlist/bytes).
            let scalar_values: Vec<AttributeScalarValue> = arr
                .values
                .iter()
                .filter_map(|v| match v.value.as_ref()? {
                    Value::StringValue(s) => Some(AttributeScalarValue::String(MetaString::from(s.as_str()))),
                    Value::BoolValue(b) => Some(AttributeScalarValue::Bool(*b)),
                    Value::IntValue(i) => Some(AttributeScalarValue::Int(*i)),
                    Value::DoubleValue(d) => Some(AttributeScalarValue::Double(*d)),
                    _ => None,
                })
                .collect();

            if scalar_values.is_empty() {
                None
            } else {
                Some(AttributeValue::Array(scalar_values))
            }
        }
        Value::BytesValue(_) | Value::KvlistValue(_) => None,
    }
}

/// Converts an OTLP KeyValue to an ADP KeyValue.
pub fn otlp_key_value_to_adp(kv: &otlp_common::KeyValue) -> CoreKeyValue {
    CoreKeyValue {
        key: MetaString::from(kv.key.as_str()),
        value: kv.value.as_ref().and_then(otlp_any_value_to_adp),
    }
}

/// Converts an OTLP EntityRef to an ADP EntityRef.
pub fn otlp_entity_ref_to_adp(er: &otlp_common::EntityRef) -> CoreEntityRef {
    CoreEntityRef {
        schema_url: MetaString::from(er.schema_url.as_str()),
        entity_type: MetaString::from(er.r#type.as_str()),
        id_keys: er.id_keys.iter().map(|k| MetaString::from(k.as_str())).collect(),
        description_keys: er
            .description_keys
            .iter()
            .map(|k| MetaString::from(k.as_str()))
            .collect(),
    }
}

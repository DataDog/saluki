//! Shared OTLP utility functions and types.
//!
//! This module contains helpers used by both the OTLP source (translator) and the Datadog trace encoder.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::resource::*;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::trace::AttributeValue;
use stringtheory::MetaString;

// ============================================================================
// Datadog attribute key constants shared across the encoder and translator
// ============================================================================

pub const KEY_DATADOG_VERSION: &str = "datadog.version";
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

/// Extracts container tags from a typed attributes map and inserts them into the provided TagSet.
pub fn extract_container_tags_from_attributes_map(
    attributes: &FastHashMap<MetaString, AttributeValue>, tags: &mut TagSet,
) {
    let mut extracted_tags = FastHashSet::default();

    for (key, value) in attributes {
        let Some(str_val) = value.as_string() else {
            continue;
        };

        // Semantic Conventions
        if let Some(datadog_key) = CONTAINER_MAPPINGS.get(key.as_ref()) {
            tags.insert_tag(format!("{}:{}", datadog_key, str_val));
            extracted_tags.insert(*datadog_key);
        }

        // Custom (datadog.container.tag namespace)
        if key.starts_with(CUSTOM_CONTAINER_TAG_PREFIX) {
            if let Some(custom_key) = key.get(CUSTOM_CONTAINER_TAG_PREFIX.len()..) {
                if !custom_key.is_empty() {
                    // Do not replace if set via semantic conventions mappings.
                    if !extracted_tags.insert(custom_key) {
                        tags.insert_tag(format!("{}:{}", custom_key, str_val));
                    }
                }
            }
        }
    }
}

const ATTRIBUTE_DATADOG_HOSTNAME: &str = "datadog.host.name";
const ATTRIBUTE_HOST: &str = "host";
const AZURE_RESOURCE_GROUP_NAME: &str = "azure.resourcegroup.name";
const EC2_CLUSTER_TAG_PREFIX: &str = "ec2.tag.kubernetes.io/cluster/";
const INVALID_HOSTNAMES: [&str; 6] = [
    "0.0.0.0",
    "127.0.0.1",
    "localhost",
    "localhost.localdomain",
    "localhost6.localdomain6",
    "ip6-localhost",
];

/// Resolves an AKS cluster name from an Azure managed resource group name.
fn aks_cluster_name(resource_group_name: &str) -> Option<&str> {
    let segments: Vec<_> = resource_group_name.split('_').collect();
    if segments.len() < 4 || !segments[0].eq_ignore_ascii_case("mc") {
        return None;
    }

    Some(segments[segments.len() - 2])
}

/// Resolves an EKS cluster name from an EC2 Kubernetes cluster tag key.
fn ec2_cluster_name(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    attributes
        .iter()
        .find(|attribute| attribute.key.starts_with(EC2_CLUSTER_TAG_PREFIX))
        .and_then(|attribute| attribute.key.split('/').nth(2))
}

/// Resolves the Kubernetes cluster name from resource attributes.
fn kubernetes_cluster_name(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    get_string_attribute(attributes, K8S_CLUSTER_NAME).or_else(|| {
        match get_string_attribute(attributes, CLOUD_PROVIDER) {
            Some("azure") => get_string_attribute(attributes, AZURE_RESOURCE_GROUP_NAME).and_then(aks_cluster_name),
            Some("aws") => ec2_cluster_name(attributes),
            _ => None,
        }
    })
}

/// Resolves the hostname for an Azure resource.
fn azure_hostname(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    get_string_attribute(attributes, HOST_ID).or_else(|| get_string_attribute(attributes, HOST_NAME))
}

/// Resolves the hostname for an EC2 resource.
fn ec2_hostname(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    get_string_attribute(attributes, HOST_ID)
}

/// Resolves the hostname for a GCP resource.
fn gcp_hostname(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let host_name = get_string_attribute(attributes, HOST_NAME)?;
    let cloud_account_id = get_string_attribute(attributes, CLOUD_ACCOUNT_ID)?;
    let host_name = if host_name.matches('.').count() >= 3 {
        host_name.split_once('.').map_or(host_name, |(name, _)| name)
    } else {
        host_name
    };

    Some(format!("{host_name}.{cloud_account_id}"))
}

/// Resolves a Kubernetes hostname from resource attributes.
fn kubernetes_hostname(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let node_name = get_string_attribute(attributes, K8S_NODE_NAME)?;
    let cluster_name = kubernetes_cluster_name(attributes);

    Some(match cluster_name {
        Some(cluster_name) => format!("{node_name}-{cluster_name}"),
        None => node_name.to_string(),
    })
}

/// Resolves an unsanitized hostname from resource attributes.
fn unsanitized_hostname_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    if let Some(hostname) = get_string_attribute(attributes, ATTRIBUTE_HOST) {
        return Some(hostname.to_string());
    }

    if let Some(hostname) = get_string_attribute(attributes, ATTRIBUTE_DATADOG_HOSTNAME) {
        return Some(hostname.to_string());
    }

    if get_string_attribute(attributes, AWS_ECS_LAUNCHTYPE) == Some("fargate") {
        return None;
    }

    match get_string_attribute(attributes, CLOUD_PROVIDER) {
        Some("aws") => return ec2_hostname(attributes).map(str::to_owned),
        Some("gcp") => return gcp_hostname(attributes),
        Some("azure") => return azure_hostname(attributes).map(str::to_owned),
        _ => {}
    }

    kubernetes_hostname(attributes)
        .or_else(|| get_string_attribute(attributes, HOST_ID).map(str::to_owned))
        .or_else(|| get_string_attribute(attributes, HOST_NAME).map(str::to_owned))
}

/// Resolves a valid hostname from resource attributes.
fn hostname_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let hostname = unsanitized_hostname_from_attributes(attributes)?;
    (!INVALID_HOSTNAMES.contains(&hostname.as_str())).then_some(hostname)
}

/// Resolves the source metadata from OTLP metric resource attributes.
///
/// Keeps resolution scoped to the metrics pipeline
pub fn resource_to_metric_source(
    resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource,
) -> Option<Source> {
    let attributes = &resource.attributes;

    //If the metric comes from an AWS ECS Fargate resource and a task_arn is present, we omit the hostname
    if get_string_attribute(attributes, AWS_ECS_LAUNCHTYPE) == Some("fargate") {
        if let Some(task_arn) = get_string_attribute(attributes, AWS_ECS_TASK_ARN) {
            return Some(Source {
                kind: SourceKind::AwsEcsFargateKind,
                identifier: task_arn.to_string(),
            });
        }
    }

    hostname_from_attributes(attributes).map(|identifier| Source {
        kind: SourceKind::HostnameKind,
        identifier,
    })
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

/// Resolves the source metadata from a typed attributes map.
///
/// This is equivalent to `resource_to_source`, but works on the unified trace attribute map
/// instead of the OTLP protobuf resource type.
pub fn attributes_to_source(attributes: &FastHashMap<MetaString, AttributeValue>) -> Option<Source> {
    let get = |key: &str| -> Option<&str> {
        attributes
            .get(key)
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref())
    };

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

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::{
        common::v1::{any_value::Value, AnyValue, KeyValue},
        resource::v1::Resource,
    };

    use super::*;

    fn string_attribute(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
        }
    }

    fn metric_source(attributes: Vec<KeyValue>) -> Option<Source> {
        resource_to_metric_source(&Resource {
            attributes,
            ..Default::default()
        })
    }

    fn assert_hostname(attributes: Vec<KeyValue>, expected: &str) {
        let source = metric_source(attributes).expect("hostname source should resolve");
        assert_eq!(source.kind, SourceKind::HostnameKind);
        assert_eq!(source.identifier, expected);
    }

    #[test]
    fn literal_host_takes_precedence_over_all_other_hostname_attributes() {
        assert_hostname(
            vec![
                string_attribute(ATTRIBUTE_HOST, "literal-host"),
                string_attribute(ATTRIBUTE_DATADOG_HOSTNAME, "datadog-host"),
                string_attribute(HOST_ID, "host-id"),
                string_attribute(HOST_NAME, "host-name"),
            ],
            "literal-host",
        );
    }

    #[test]
    fn datadog_hostname_takes_precedence_over_cloud_and_generic_attributes() {
        assert_hostname(
            vec![
                string_attribute(ATTRIBUTE_DATADOG_HOSTNAME, "datadog-host"),
                string_attribute(CLOUD_PROVIDER, "azure"),
                string_attribute(HOST_ID, "host-id"),
                string_attribute(HOST_NAME, "host-name"),
            ],
            "datadog-host",
        );
    }

    #[test]
    fn fargate_resource_is_not_a_hostname_source() {
        let source = metric_source(vec![
            string_attribute(AWS_ECS_LAUNCHTYPE, "fargate"),
            string_attribute(AWS_ECS_TASK_ARN, "task-arn"),
            string_attribute(HOST_NAME, "host-name"),
        ])
        .expect("Fargate source should resolve");

        assert_eq!(source.kind, SourceKind::AwsEcsFargateKind);
        assert_eq!(source.identifier, "task-arn");
    }

    #[test]
    fn cloud_hostname_helpers_use_provider_specific_rules() {
        assert_hostname(
            vec![
                string_attribute(CLOUD_PROVIDER, "aws"),
                string_attribute(HOST_ID, "i-123"),
                string_attribute(HOST_NAME, "ec2-host"),
            ],
            "i-123",
        );
        assert_hostname(
            vec![
                string_attribute(CLOUD_PROVIDER, "gcp"),
                string_attribute(HOST_NAME, "gce-host.us-central1-a.c.project.internal"),
                string_attribute(CLOUD_ACCOUNT_ID, "project-id"),
            ],
            "gce-host.project-id",
        );
        assert_hostname(
            vec![
                string_attribute(CLOUD_PROVIDER, "azure"),
                string_attribute(HOST_ID, "vm-id"),
                string_attribute(HOST_NAME, "azure-host"),
            ],
            "vm-id",
        );
    }

    #[test]
    fn kubernetes_hostname_uses_the_explicit_cluster_name() {
        assert_hostname(
            vec![
                string_attribute(K8S_NODE_NAME, "node-name"),
                string_attribute(K8S_CLUSTER_NAME, "explicit-cluster"),
            ],
            "node-name-explicit-cluster",
        );
    }

    #[test]
    fn aks_and_ec2_cluster_name_helpers_extract_cluster_names() {
        let azure_attributes = vec![
            string_attribute(CLOUD_PROVIDER, "azure"),
            string_attribute(AZURE_RESOURCE_GROUP_NAME, "mC_resource-group_aks-cluster_region"),
        ];
        assert_eq!(kubernetes_cluster_name(&azure_attributes), Some("aks-cluster"));

        let aws_attributes = vec![
            string_attribute(CLOUD_PROVIDER, "aws"),
            string_attribute("ec2.tag.kubernetes.io/cluster/eks-cluster", "owned"),
        ];
        assert_eq!(kubernetes_cluster_name(&aws_attributes), Some("eks-cluster"));
    }

    #[test]
    fn invalid_aks_resource_group_has_no_cluster_name() {
        for resource_group_name in ["MC_too_few", "not-mc_group_cluster_region"] {
            let attributes = vec![
                string_attribute(CLOUD_PROVIDER, "azure"),
                string_attribute(AZURE_RESOURCE_GROUP_NAME, resource_group_name),
            ];
            assert_eq!(kubernetes_cluster_name(&attributes), None);
        }
    }

    #[test]
    fn recognized_cloud_providers_do_not_fall_through_to_kubernetes_or_generic_hosts() {
        assert!(metric_source(vec![
            string_attribute(CLOUD_PROVIDER, "azure"),
            string_attribute(K8S_NODE_NAME, "node-name"),
        ])
        .is_none());
    }

    #[test]
    fn generic_host_id_and_name_are_final_fallbacks() {
        assert_hostname(
            vec![
                string_attribute(HOST_ID, "host-id"),
                string_attribute(HOST_NAME, "host-name"),
            ],
            "host-id",
        );
        assert_hostname(vec![string_attribute(HOST_NAME, "host-name")], "host-name");
    }

    #[test]
    fn blocklisted_hostnames_do_not_produce_a_source() {
        for hostname in INVALID_HOSTNAMES {
            assert!(
                metric_source(vec![string_attribute(HOST_NAME, hostname)]).is_none(),
                "{hostname}"
            );
        }
    }

    #[test]
    fn metric_source_resolution_does_not_change_the_shared_logs_resolver() {
        let resource = Resource {
            attributes: vec![
                string_attribute(CLOUD_PROVIDER, "azure"),
                string_attribute(HOST_ID, "vm-id"),
                string_attribute(HOST_NAME, "host-name"),
            ],
            ..Default::default()
        };

        let logs_source = resource_to_source(&resource).expect("logs source should resolve");
        assert_eq!(logs_source.kind, SourceKind::HostnameKind);
        assert_eq!(logs_source.identifier, "host-name");

        let metric_source = resource_to_metric_source(&resource).expect("metric source should resolve");
        assert_eq!(metric_source.kind, SourceKind::HostnameKind);
        assert_eq!(metric_source.identifier, "vm-id");
    }
}

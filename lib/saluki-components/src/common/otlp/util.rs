//! Shared OTLP utility functions and types.
//!
//! This module contains helpers used by both the OTLP source (translator) and the Datadog trace encoder.

use std::sync::LazyLock;

use opentelemetry_semantic_conventions::resource::*;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_core::data_model::{event::trace::AttributeValue, tags::SharedTagSet, tags::TagSet};
use stringtheory::interning::Interner;
use stringtheory::MetaString;

use crate::common::otlp::traces::translator::ContainerTagPromotion;

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
    Hostname,
    /// AWS ECS Fargate source.
    AwsEcsFargate,
    /// Azure Container Apps source.
    AzureContainerApps,
    /// Azure App Service source.
    AzureAppService,
    /// Azure Functions source.
    AzureFunctions,
    /// Cloud Run service source.
    GcpCloudRun,
    /// Cloud Functions source.
    GcpCloudFunctions,
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
            SourceKind::Hostname => "host",
            SourceKind::AwsEcsFargate => "task_arn",
            SourceKind::AzureContainerApps => "azure_container_apps",
            SourceKind::AzureAppService => "azure_app_service",
            SourceKind::AzureFunctions => "azure_functions",
            SourceKind::GcpCloudRun => "gcp_cloud_run",
            SourceKind::GcpCloudFunctions => "gcp_cloud_functions",
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
    m.insert("container.runtime", "runtime"); // Not re-exported by semconv 0.32.1+

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
    m.insert(K8S_NODE_NAME, "kube_node");
    m
});

// The Datadog tag names produced by the container mappings; an attribute with one of these keys is
// already a container tag.
static CONTAINER_DD_TAG_KEYS: LazyLock<FastHashSet<&'static str>> = LazyLock::new(|| {
    let mut s = FastHashSet::with_capacity_and_hasher(CONTAINER_MAPPINGS.len(), Default::default());
    for datadog_key in CONTAINER_MAPPINGS.values() {
        s.insert(*datadog_key);
    }
    s
});

// Tag sources in increasing precedence: a Datadog-convention key, then a
// `datadog.container.tag.` prefixed attribute, then a semantic-convention mapping.
const TAG_SOURCE_DATADOG_CONVENTION: u8 = 1;
const TAG_SOURCE_CUSTOM: u8 = 2;
const TAG_SOURCE_SEMANTIC_CONVENTION: u8 = 3;

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
///
/// Tags come from semantic-convention mappings and `datadog.container.tag.` prefixed attributes,
/// ranked by the `TAG_SOURCE_*` precedences; empty values produce no tag. A later
/// semantic-convention attribute replaces an earlier one that maps to the same tag, while the first
/// custom attribute for a tag wins.
pub fn extract_container_tags_from_resource_attributes(attributes: &[otlp_common::KeyValue], tags: &mut TagSet) {
    let mut resolved: FastHashMap<&str, (u8, &str)> = FastHashMap::default();

    for kv in attributes {
        let value = kv.value.as_ref().and_then(|v| v.value.as_ref());

        // Semantic conventions. `container.image.tags` is mapped here rather than in
        // `CONTAINER_MAPPINGS`, which traces also use: trace resource arrays reach container-tag
        // extraction as JSON strings, so traces can't read its first element.
        let mapped = match CONTAINER_MAPPINGS.get(kv.key.as_str()) {
            Some(datadog_key) => non_empty_string(value).map(|s_val| (*datadog_key, s_val)),
            None if kv.key == CONTAINER_IMAGE_TAGS => first_element_or_string(value).map(|s_val| ("image_tag", s_val)),
            None => None,
        };
        if let Some((datadog_key, s_val)) = mapped {
            insert_winning_tag(&mut resolved, datadog_key, TAG_SOURCE_SEMANTIC_CONVENTION, s_val, true);
        }

        // Custom (datadog.container.tag namespace)
        if let Some(custom_key) = kv.key.strip_prefix(CUSTOM_CONTAINER_TAG_PREFIX) {
            if let Some(Value::StringValue(s_val)) = value {
                if !custom_key.is_empty() && !s_val.is_empty() {
                    insert_winning_tag(&mut resolved, custom_key, TAG_SOURCE_CUSTOM, s_val, false);
                }
            }
        }
    }

    for (tag_key, (_, value)) in resolved {
        tags.insert_tag(format!("{}:{}", tag_key, value));
    }
}

/// Extracts container tags from a typed attributes map and inserts them into the provided TagSet.
///
/// Tags come from three sources, ranked by the `TAG_SOURCE_*` precedences; empty values produce no
/// tag.
pub fn extract_container_tags_from_attributes_map(
    attributes: &FastHashMap<MetaString, AttributeValue>, tags: &mut TagSet,
) {
    // One entry per resolved tag key: its source precedence and value.
    let mut resolved: FastHashMap<&str, (u8, &str)> = FastHashMap::default();

    for (key, value) in attributes {
        let Some(str_val) = value.as_string() else { continue };
        if str_val.is_empty() {
            continue;
        }

        // Semantic Conventions
        if let Some(datadog_key) = CONTAINER_MAPPINGS.get(key.as_ref()) {
            insert_winning_tag(
                &mut resolved,
                datadog_key,
                TAG_SOURCE_SEMANTIC_CONVENTION,
                str_val.as_ref(),
                false,
            );
        }

        // Pre-mapped Datadog-convention tag name
        if CONTAINER_DD_TAG_KEYS.contains(key.as_ref()) {
            insert_winning_tag(
                &mut resolved,
                key.as_ref(),
                TAG_SOURCE_DATADOG_CONVENTION,
                str_val.as_ref(),
                false,
            );
        }

        // Custom (datadog.container.tag namespace)
        if let Some(custom_key) = key.strip_prefix(CUSTOM_CONTAINER_TAG_PREFIX) {
            if !custom_key.is_empty() {
                insert_winning_tag(&mut resolved, custom_key, TAG_SOURCE_CUSTOM, str_val.as_ref(), false);
            }
        }
    }

    for (tag_key, (_, value)) in resolved {
        tags.insert_tag(format!("{}:{}", tag_key, value));
    }
}

/// Returns a non-empty string value.
fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::StringValue(s_val) if !s_val.is_empty() => Some(s_val),
        _ => None,
    }
}

/// Returns the first element of an array value, or else the value itself, as a non-empty string.
fn first_element_or_string(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::ArrayValue(array) => non_empty_string(array.values.first()?.value.as_ref()),
        value => non_empty_string(Some(value)),
    }
}

/// Records a tag for `tag_key` unless a higher-precedence source already claimed it. With
/// `replace_equal`, the tag also replaces one from the same source.
fn insert_winning_tag<'a>(
    resolved: &mut FastHashMap<&'a str, (u8, &'a str)>, tag_key: &'a str, source: u8, value: &'a str,
    replace_equal: bool,
) {
    resolved
        .entry(tag_key)
        .and_modify(|existing| {
            if existing.0 < source || (replace_equal && existing.0 == source) {
                *existing = (source, value);
            }
        })
        .or_insert((source, value));
}

/// Writes resolved infrastructure tags into a resource attribute map as string attributes.
///
/// Keys already recognized by container-tag extraction are written as-is; every other key follows
/// `promotion`. Writes replace existing entries with the same key, except for generated prefixed
/// copies, which never replace a value the sender provided. Valueless tags are skipped.
///
/// Returns the unprefixed custom tags written as-is (`off` and `duplicate` modes). The Agent leaves
/// these on the resource, where span conversion maps them onto every span; recognized and prefixed
/// keys are consumed as container tags and are not returned.
pub fn write_infra_tag_attributes<I: Interner>(
    tags: SharedTagSet, promotion: ContainerTagPromotion, interner: &I,
    attributes: &mut FastHashMap<MetaString, AttributeValue>,
) -> Vec<otlp_common::KeyValue> {
    let mut span_attributes = Vec::new();

    for tag in &tags {
        let key = tag.name();
        let Some(value) = tag.value() else { continue };

        let write_attribute = |attributes: &mut FastHashMap<MetaString, AttributeValue>, key: &str| {
            attributes.insert(
                MetaString::from_interner(key, interner),
                AttributeValue::String(MetaString::from_interner(value, interner)),
            );
        };

        if key.starts_with(CUSTOM_CONTAINER_TAG_PREFIX) || is_recognized_tag_key(key) {
            write_attribute(attributes, key);
            continue;
        }

        match promotion {
            ContainerTagPromotion::Off => {
                write_attribute(attributes, key);
                span_attributes.push(string_key_value(key, value));
            }
            ContainerTagPromotion::Duplicate => {
                write_attribute(attributes, key);
                write_prefixed_if_absent(attributes, key, value, interner);
                span_attributes.push(string_key_value(key, value));
            }
            ContainerTagPromotion::Rename => {
                write_prefixed_if_absent(attributes, key, value, interner);
            }
        }
    }

    span_attributes
}

/// Sets `entries` on a resource attribute list, replacing any existing attribute with the same key.
pub fn set_resource_attributes(attributes: &mut Vec<otlp_common::KeyValue>, entries: Vec<otlp_common::KeyValue>) {
    if entries.is_empty() {
        return;
    }
    attributes.retain(|existing| entries.iter().all(|entry| entry.key != existing.key));
    attributes.extend(entries);
}

fn string_key_value(key: &str, value: &str) -> otlp_common::KeyValue {
    otlp_common::KeyValue {
        key: key.to_string(),
        value: Some(otlp_common::AnyValue {
            value: Some(Value::StringValue(value.to_string())),
        }),
    }
}

/// Writes the prefixed copy of a custom tag without replacing an entry the sender provided.
fn write_prefixed_if_absent<I: Interner>(
    attributes: &mut FastHashMap<MetaString, AttributeValue>, key: &str, value: &str, interner: &I,
) {
    attributes
        .entry(MetaString::from_interner(
            &format!("{CUSTOM_CONTAINER_TAG_PREFIX}{key}"),
            interner,
        ))
        .or_insert_with(|| AttributeValue::String(MetaString::from_interner(value, interner)));
}

fn is_recognized_tag_key(key: &str) -> bool {
    CONTAINER_MAPPINGS.contains_key(key) || CONTAINER_DD_TAG_KEYS.contains(key)
}

const ATTRIBUTE_DATADOG_HOSTNAME: &str = "datadog.host.name";
const ATTRIBUTE_HOST: &str = "host";
const ATTRIBUTE_AZURE_CONTAINER_APP_INSTANCE_ID: &str = "azure.container_app.instance.id";
// The Azure resource group key differs between cluster-name resolution and serverless identities.
const AZURE_RESOURCE_GROUP_NAME: &str = "azure.resourcegroup.name";
const AZURE_SERVERLESS_RESOURCE_GROUP_NAME: &str = "azure.resource_group.name";
const AZURE_APP_SERVICE_PLATFORMS: [&str; 2] = ["azure.app_service", "azure_app_service"];
const AZURE_FUNCTIONS_PLATFORMS: [&str; 2] = ["azure.functions", "azure_functions"];
const AZURE_CONTAINER_APPS_PLATFORMS: [&str; 2] = ["azure.container_apps", "azure_container_apps"];
const GCP_CLOUD_FUNCTIONS_PLATFORM: &str = "gcp_cloud_functions";
const GCP_CLOUD_RUN_PLATFORM: &str = "gcp_cloud_run";
const GCP_CLOUD_RUN_JOB_ATTRIBUTES: [&str; 2] = ["gcp.cloud_run.job.execution", "gcp.cloud_run.job.task_index"];
const EC2_CLUSTER_TAG_PREFIX: &str = "ec2.tag.kubernetes.io/cluster/";
const INVALID_HOSTNAMES: [&str; 6] = [
    "0.0.0.0",
    "127.0.0.1",
    "localhost",
    "localhost.localdomain",
    "localhost6.localdomain6",
    "ip6-localhost",
];

/// Returns the value of the first attribute named `key`, like `Map.Get` in `pdata`.
///
/// The inner `Option` is `None` for an attribute that is present but has an empty value.
fn get_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<Option<&'a Value>> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_ref().and_then(|v| v.value.as_ref()))
}

/// Returns the first attribute named `key` as `Map.Get` followed by `Value.Str` in `pdata` would.
///
/// Unlike [`get_string_attribute`], a present attribute that isn't a string yields `Some("")`, so a
/// presence check succeeds for it.
fn get_attribute_str<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
    get_attribute(attributes, key).map(|value| match value {
        Some(Value::StringValue(s_val)) => s_val.as_str(),
        _ => "",
    })
}

/// Returns the attribute as `Value.Str` in `pdata` would, but only when it isn't empty.
fn get_non_empty_attribute_str<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
    get_attribute_str(attributes, key).filter(|s_val| !s_val.is_empty())
}

/// Renders the literal `host` attribute as `Value.AsString` in `pdata` does.
///
/// Doubles, bytes, arrays, and maps return `None` because their `AsString` formatting isn't
/// implemented yet, so resolution falls through to the next hostname source for them.
fn literal_host_as_string(value: Option<&Value>) -> Option<String> {
    match value {
        None => Some(String::new()),
        Some(Value::StringValue(s_val)) => Some(s_val.clone()),
        Some(Value::BoolValue(b_val)) => Some(b_val.to_string()),
        Some(Value::IntValue(i_val)) => Some(i_val.to_string()),
        Some(_) => None,
    }
}

/// Resolves an AKS cluster name from an Azure managed resource group name.
fn aks_cluster_name(resource_group_name: &str) -> Option<&str> {
    let segments: Vec<_> = resource_group_name.split('_').collect();
    if segments.len() < 4 || !segments[0].eq_ignore_ascii_case("mc") {
        return None;
    }

    Some(segments[segments.len() - 2])
}

/// Resolves an EKS cluster name from an EC2 Kubernetes cluster tag key.
///
/// The last matching key wins.
fn ec2_cluster_name(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    attributes
        .iter()
        .rev()
        .find(|attribute| attribute.key.starts_with(EC2_CLUSTER_TAG_PREFIX))
        .and_then(|attribute| attribute.key.split('/').nth(2))
}

/// Resolves the Kubernetes cluster name from resource attributes.
fn kubernetes_cluster_name(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    if let Some(cluster_name) = get_attribute_str(attributes, K8S_CLUSTER_NAME) {
        return Some(cluster_name);
    }

    match get_attribute_str(attributes, CLOUD_PROVIDER) {
        Some("azure") => get_attribute_str(attributes, AZURE_RESOURCE_GROUP_NAME).and_then(aks_cluster_name),
        Some("aws") => ec2_cluster_name(attributes),
        _ => None,
    }
}

/// Resolves the hostname for an Azure resource.
fn azure_hostname(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    get_attribute_str(attributes, HOST_ID).or_else(|| get_attribute_str(attributes, HOST_NAME))
}

/// Resolves the hostname for an EC2 resource.
fn ec2_hostname(attributes: &[otlp_common::KeyValue]) -> Option<&str> {
    get_attribute_str(attributes, HOST_ID)
}

/// Resolves the hostname for a GCP resource.
fn gcp_hostname(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let host_name = get_attribute_str(attributes, HOST_NAME)?;
    let host_name = if host_name.matches('.').count() >= 3 {
        host_name.split_once('.').map_or(host_name, |(name, _)| name)
    } else {
        host_name
    };
    let cloud_account_id = get_attribute_str(attributes, CLOUD_ACCOUNT_ID)?;

    Some(format!("{host_name}.{cloud_account_id}"))
}

/// Resolves a Kubernetes hostname from resource attributes.
fn kubernetes_hostname(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let node_name = get_attribute_str(attributes, K8S_NODE_NAME)?;
    let cluster_name = kubernetes_cluster_name(attributes);

    Some(match cluster_name {
        Some(cluster_name) => format!("{node_name}-{cluster_name}"),
        None => node_name.to_string(),
    })
}

/// Resolves an unfiltered hostname from resource attributes.
///
/// Checks, in order: the literal `host` attribute, `datadog.host.name`, ECS Fargate (which has no
/// hostname), the cloud provider's hostname, the Kubernetes node name, `host.id`, and `host.name`.
fn unsanitized_hostname_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    if let Some(hostname) = get_attribute(attributes, ATTRIBUTE_HOST).and_then(literal_host_as_string) {
        return Some(hostname);
    }

    if let Some(hostname) = get_attribute_str(attributes, ATTRIBUTE_DATADOG_HOSTNAME) {
        return Some(hostname.to_string());
    }

    if get_attribute_str(attributes, AWS_ECS_LAUNCHTYPE) == Some("fargate") {
        return None;
    }

    match get_attribute_str(attributes, CLOUD_PROVIDER) {
        Some("aws") => return ec2_hostname(attributes).map(str::to_owned),
        Some("gcp") => return gcp_hostname(attributes),
        Some("azure") => return azure_hostname(attributes).map(str::to_owned),
        _ => {}
    }

    kubernetes_hostname(attributes)
        .or_else(|| get_attribute_str(attributes, HOST_ID).map(str::to_owned))
        .or_else(|| get_attribute_str(attributes, HOST_NAME).map(str::to_owned))
}

/// Resolves a valid hostname from resource attributes.
///
/// The hostname can be empty, for example when `datadog.host.name` isn't a string.
fn hostname_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let hostname = unsanitized_hostname_from_attributes(attributes)?;
    (!INVALID_HOSTNAMES.contains(&hostname.as_str())).then_some(hostname)
}

fn hostname_source(attributes: &[otlp_common::KeyValue]) -> Option<Source> {
    hostname_from_attributes(attributes).map(|identifier| Source {
        kind: SourceKind::Hostname,
        identifier,
    })
}

fn aws_ecs_fargate_source(attributes: &[otlp_common::KeyValue]) -> Option<Source> {
    if get_attribute_str(attributes, AWS_ECS_LAUNCHTYPE) != Some("fargate") {
        return None;
    }

    get_attribute_str(attributes, AWS_ECS_TASK_ARN).map(|task_arn| Source {
        kind: SourceKind::AwsEcsFargate,
        identifier: task_arn.to_string(),
    })
}

/// An Azure App Service or Azure Functions app identity.
struct AzureAppIdentity<'a> {
    name: &'a str,
    subscription_id: &'a str,
    resource_group: &'a str,
}

/// Resolves a complete Azure app identity for a resource on one of `platforms`.
fn azure_app_identity<'a>(attributes: &'a [otlp_common::KeyValue], platforms: &[&str]) -> Option<AzureAppIdentity<'a>> {
    let platform = get_attribute_str(attributes, CLOUD_PLATFORM)?;
    if !platforms.contains(&platform) {
        return None;
    }

    Some(AzureAppIdentity {
        name: get_non_empty_attribute_str(attributes, SERVICE_NAME)?,
        subscription_id: get_non_empty_attribute_str(attributes, CLOUD_ACCOUNT_ID)?,
        resource_group: get_non_empty_attribute_str(attributes, AZURE_SERVERLESS_RESOURCE_GROUP_NAME)?,
    })
}

/// Returns the name segment of an Azure resource ID such as
/// `/subscriptions/{sub}/resourceGroups/{rg}/providers/{provider}/{type}/{name}`.
fn azure_resource_id_name(resource_id: &str) -> Option<&str> {
    resource_id.split('/').nth(8)
}

/// Resolves the identifier of an Azure Container Apps resource: its replica, or else its app name.
///
/// Any resource on the Container Apps platform is a Container Apps source, even when the identifier
/// resolves to an empty string.
fn azure_container_apps_identifier(attributes: &[otlp_common::KeyValue]) -> Option<String> {
    let platform = get_attribute_str(attributes, CLOUD_PLATFORM)?;
    if !AZURE_CONTAINER_APPS_PLATFORMS.contains(&platform) {
        return None;
    }

    let replica = get_non_empty_attribute_str(attributes, ATTRIBUTE_AZURE_CONTAINER_APP_INSTANCE_ID)
        .or_else(|| get_non_empty_attribute_str(attributes, SERVICE_INSTANCE_ID));
    let name = || {
        get_non_empty_attribute_str(attributes, SERVICE_NAME).or_else(|| {
            get_non_empty_attribute_str(attributes, CLOUD_RESOURCE_ID)
                .and_then(azure_resource_id_name)
                .filter(|name| !name.is_empty())
        })
    };

    Some(replica.or_else(name).unwrap_or_default().to_string())
}

/// A Cloud Run or Cloud Functions identity, with the dimensions that are also emitted as tags.
struct GcpServerlessIdentity<'a> {
    kind: SourceKind,
    instance: &'a str,
    required_dimensions: [(&'static str, &'a str); 4],
    revision: Option<&'a str>,
}

impl<'a> GcpServerlessIdentity<'a> {
    fn dimensions(&self) -> impl Iterator<Item = (&'static str, &'a str)> + '_ {
        self.required_dimensions
            .iter()
            .copied()
            .chain(self.revision.map(|revision| ("revision_name", revision)))
    }
}

/// Returns whether the resource is on a Cloud Run or Cloud Functions platform, even with an incomplete
/// identity. Such a resource must not fall back to a default hostname.
pub fn is_gcp_serverless(attributes: &[otlp_common::KeyValue]) -> bool {
    matches!(
        get_attribute_str(attributes, CLOUD_PLATFORM),
        Some(GCP_CLOUD_FUNCTIONS_PLATFORM | GCP_CLOUD_RUN_PLATFORM)
    )
}

/// Resolves a complete Cloud Run service or Cloud Functions identity.
///
/// Cloud Run jobs and incomplete identities resolve to `None`.
fn gcp_serverless_identity(attributes: &[otlp_common::KeyValue]) -> Option<GcpServerlessIdentity<'_>> {
    let kind = match get_attribute_str(attributes, CLOUD_PLATFORM)? {
        GCP_CLOUD_FUNCTIONS_PLATFORM => SourceKind::GcpCloudFunctions,
        GCP_CLOUD_RUN_PLATFORM => SourceKind::GcpCloudRun,
        _ => return None,
    };

    if GCP_CLOUD_RUN_JOB_ATTRIBUTES
        .iter()
        .any(|key| get_attribute(attributes, key).is_some())
    {
        return None;
    }

    // Unlike the other lookups here, these must be strings, not just present.
    let required = |key| match get_attribute(attributes, key) {
        Some(Some(Value::StringValue(s_val))) if !s_val.is_empty() => Some(s_val.as_str()),
        _ => None,
    };
    let instance = required(FAAS_INSTANCE)?;
    let required_dimensions = [
        ("project_id", required(CLOUD_ACCOUNT_ID)?),
        ("location", required(CLOUD_REGION)?),
        ("service_name", required(FAAS_NAME)?),
        ("instance", instance),
    ];

    Some(GcpServerlessIdentity {
        kind,
        instance,
        required_dimensions,
        revision: get_non_empty_attribute_str(attributes, FAAS_VERSION),
    })
}

/// Adds the serverless identity tags derived from resource attributes: Azure App Service app
/// identities and Cloud Run or Cloud Functions dimensions.
pub fn extract_serverless_tags_from_resource_attributes(attributes: &[otlp_common::KeyValue], tags: &mut TagSet) {
    if let Some(app) = azure_app_identity(attributes, &AZURE_APP_SERVICE_PLATFORMS) {
        tags.insert_tag(format!("name:{}", app.name));
        tags.insert_tag(format!("subscription_id:{}", app.subscription_id));
        tags.insert_tag(format!("resource_group:{}", app.resource_group));
    }

    if let Some(identity) = gcp_serverless_identity(attributes) {
        for (key, value) in identity.dimensions() {
            tags.insert_tag(format!("{key}:{value}"));
        }
    }
}

/// Resolves the source metadata from OTLP metric resource attributes with [`source_from_attributes`].
pub fn resource_to_metric_source(
    resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource,
) -> Option<Source> {
    source_from_attributes(&resource.attributes)
}

/// Resolves the source of telemetry from attributes.
///
/// Checks, in order: Azure Functions, Cloud Run and Cloud Functions, Azure App Service, Azure
/// Container Apps, ECS Fargate, and the hostname. A resource on a Cloud Run or Cloud Functions
/// platform never falls back to a host, even when its identity is incomplete.
pub fn source_from_attributes(attributes: &[otlp_common::KeyValue]) -> Option<Source> {
    if let Some(app) = azure_app_identity(attributes, &AZURE_FUNCTIONS_PLATFORMS) {
        return Some(Source {
            kind: SourceKind::AzureFunctions,
            identifier: app.name.to_string(),
        });
    }

    if is_gcp_serverless(attributes) {
        return gcp_serverless_identity(attributes).map(|identity| Source {
            kind: identity.kind,
            identifier: identity.instance.to_string(),
        });
    }

    if let Some(app) = azure_app_identity(attributes, &AZURE_APP_SERVICE_PLATFORMS) {
        return Some(Source {
            kind: SourceKind::AzureAppService,
            identifier: app.name.to_string(),
        });
    }

    if let Some(identifier) = azure_container_apps_identifier(attributes) {
        return Some(Source {
            kind: SourceKind::AzureContainerApps,
            identifier,
        });
    }

    aws_ecs_fargate_source(attributes).or_else(|| hostname_source(attributes))
}

/// Resolves the source metadata from OTLP resource attributes with [`source_from_attributes`].
pub fn resource_to_source(resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource) -> Option<Source> {
    source_from_attributes(&resource.attributes)
}

/// Resolves the source metadata from a typed attributes map.
///
/// The trace encoder only resolves AWS ECS Fargate tasks and `host.name` hosts.
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
                kind: SourceKind::AwsEcsFargate,
                identifier: task_arn.to_string(),
            });
        }
    }

    // Hostname from attributes
    if let Some(host_name) = get(opentelemetry_semantic_conventions::resource::HOST_NAME) {
        return Some(Source {
            kind: SourceKind::Hostname,
            identifier: host_name.to_string(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::{
        common::v1::{any_value::Value, AnyValue, ArrayValue, KeyValue},
        resource::v1::Resource,
    };
    use stringtheory::interning::GenericMapInterner;

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
        assert_eq!(source.kind, SourceKind::Hostname);
        assert_eq!(source.identifier, expected);
    }

    #[test]
    fn map_extraction_promotes_all_three_tag_sources() {
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from("container.id"),
            AttributeValue::String(MetaString::from("abc123")),
        );
        attributes.insert(
            MetaString::from("datadog.container.tag.team"),
            AttributeValue::String(MetaString::from("checkout")),
        );
        attributes.insert(
            MetaString::from("kube_namespace"),
            AttributeValue::String(MetaString::from("payments")),
        );

        let mut tags = TagSet::default();
        extract_container_tags_from_attributes_map(&attributes, &mut tags);

        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        assert_eq!(
            got,
            vec!["container_id:abc123", "kube_namespace:payments", "team:checkout"]
        );
    }

    #[test]
    fn map_extraction_resolves_source_collisions_by_precedence() {
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from("container.id"),
            AttributeValue::String(MetaString::from("from-semconv")),
        );
        attributes.insert(
            MetaString::from("datadog.container.tag.container_id"),
            AttributeValue::String(MetaString::from("from-custom")),
        );
        attributes.insert(
            MetaString::from("kube_namespace"),
            AttributeValue::String(MetaString::from("from-convention")),
        );
        attributes.insert(
            MetaString::from("datadog.container.tag.kube_namespace"),
            AttributeValue::String(MetaString::from("from-custom")),
        );

        let mut tags = TagSet::default();
        extract_container_tags_from_attributes_map(&attributes, &mut tags);

        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        assert_eq!(got, vec!["container_id:from-semconv", "kube_namespace:from-custom"]);
    }

    #[test]
    fn map_extraction_ignores_empty_values() {
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from("datadog.container.tag.team"),
            AttributeValue::String(MetaString::from("")),
        );

        let mut tags = TagSet::default();
        extract_container_tags_from_attributes_map(&attributes, &mut tags);

        assert_eq!(tags.into_iter().count(), 0);
    }

    fn test_interner() -> GenericMapInterner {
        GenericMapInterner::new(std::num::NonZeroUsize::new(64 * 1024).expect("non-zero interner size"))
    }

    fn shared_tags(tags: &[&str]) -> SharedTagSet {
        let mut set = TagSet::default();
        for tag in tags {
            set.insert_tag(tag.to_string());
        }
        set.into_shared()
    }

    fn attr_str(attributes: &FastHashMap<MetaString, AttributeValue>, key: &str) -> Option<String> {
        attributes
            .get(key)
            .and_then(|value| value.as_string())
            .map(|value| value.as_ref().to_string())
    }

    fn written_attributes(attributes: &FastHashMap<MetaString, AttributeValue>) -> Vec<String> {
        let mut keys: Vec<String> = attributes
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    key.as_ref(),
                    value.as_string().map(|v| v.as_ref().to_string()).unwrap_or_default()
                )
            })
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn write_infra_tag_attributes_follows_each_promotion_mode() {
        let tags = shared_tags(&["kube_namespace:payments", "team:checkout"]);

        let mut off = FastHashMap::default();
        write_infra_tag_attributes(tags.clone(), ContainerTagPromotion::Off, &test_interner(), &mut off);
        assert_eq!(
            written_attributes(&off),
            vec!["kube_namespace=payments", "team=checkout"]
        );

        let mut duplicate = FastHashMap::default();
        write_infra_tag_attributes(
            tags.clone(),
            ContainerTagPromotion::Duplicate,
            &test_interner(),
            &mut duplicate,
        );
        assert_eq!(
            written_attributes(&duplicate),
            vec![
                "datadog.container.tag.team=checkout",
                "kube_namespace=payments",
                "team=checkout"
            ]
        );

        let mut rename = FastHashMap::default();
        write_infra_tag_attributes(tags, ContainerTagPromotion::Rename, &test_interner(), &mut rename);
        assert_eq!(
            written_attributes(&rename),
            vec!["datadog.container.tag.team=checkout", "kube_namespace=payments"]
        );
    }

    #[test]
    fn write_infra_tag_attributes_returns_only_unprefixed_custom_tags_for_spans() {
        let tags = shared_tags(&["kube_namespace:payments", "team:checkout"]);
        let span_keys = |promotion| {
            write_infra_tag_attributes(tags.clone(), promotion, &test_interner(), &mut FastHashMap::default())
                .into_iter()
                .map(|kv| kv.key)
                .collect::<Vec<_>>()
        };

        assert_eq!(span_keys(ContainerTagPromotion::Off), vec!["team"]);
        assert_eq!(span_keys(ContainerTagPromotion::Duplicate), vec!["team"]);
        assert!(span_keys(ContainerTagPromotion::Rename).is_empty());
    }

    #[test]
    fn set_resource_attributes_replaces_existing_keys() {
        let kv = |key: &str, value: &str| string_key_value(key, value);
        let mut attributes = vec![kv("team", "sender"), kv("other", "kept")];

        set_resource_attributes(&mut attributes, vec![kv("team", "enriched")]);

        let pairs: Vec<_> = attributes
            .iter()
            .map(|a| match a.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(Value::StringValue(v)) => format!("{}={}", a.key, v),
                _ => a.key.clone(),
            })
            .collect();
        assert_eq!(pairs, vec!["other=kept", "team=enriched"]);
    }

    #[test]
    fn write_infra_tag_attributes_skips_tags_without_values() {
        let tags = shared_tags(&["valueless"]);

        let mut attributes = FastHashMap::default();
        write_infra_tag_attributes(
            tags,
            ContainerTagPromotion::Duplicate,
            &test_interner(),
            &mut attributes,
        );

        assert!(attributes.is_empty());
    }

    #[test]
    fn write_infra_tag_attributes_preserves_already_prefixed_keys() {
        let tags = shared_tags(&["datadog.container.tag.env:prod"]);

        let mut attributes = FastHashMap::default();
        write_infra_tag_attributes(tags, ContainerTagPromotion::Rename, &test_interner(), &mut attributes);

        assert_eq!(written_attributes(&attributes), vec!["datadog.container.tag.env=prod"]);
    }

    #[test]
    fn resource_extraction_prefers_semantic_conventions_regardless_of_order() {
        let attributes = vec![
            string_attribute("datadog.container.tag.container_id", "from-custom"),
            string_attribute("container.id", "from-semconv"),
        ];

        let mut tags = TagSet::default();
        extract_container_tags_from_resource_attributes(&attributes, &mut tags);

        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        assert_eq!(got, vec!["container_id:from-semconv"]);
    }

    fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn string_array(values: &[&str]) -> Value {
        Value::ArrayValue(ArrayValue {
            values: values
                .iter()
                .map(|value| AnyValue {
                    value: Some(Value::StringValue(value.to_string())),
                })
                .collect(),
        })
    }

    fn resource_container_tags(attributes: Vec<KeyValue>) -> Vec<String> {
        let mut tags = TagSet::default();
        extract_container_tags_from_resource_attributes(&attributes, &mut tags);
        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        got
    }

    #[test]
    fn resource_extraction_maps_container_image_tags() {
        let cases: Vec<(&str, Vec<KeyValue>, Vec<&str>)> = vec![
            (
                "array takes the first element",
                vec![attribute(CONTAINER_IMAGE_TAGS, string_array(&["v1", "latest"]))],
                vec!["image_tag:v1"],
            ),
            (
                "string value",
                vec![string_attribute(CONTAINER_IMAGE_TAGS, "v1")],
                vec!["image_tag:v1"],
            ),
            (
                "empty array",
                vec![attribute(CONTAINER_IMAGE_TAGS, string_array(&[]))],
                vec![],
            ),
            (
                "empty first element",
                vec![attribute(CONTAINER_IMAGE_TAGS, string_array(&["", "latest"]))],
                vec![],
            ),
            (
                "non-string first element",
                vec![attribute(
                    CONTAINER_IMAGE_TAGS,
                    Value::ArrayValue(ArrayValue {
                        values: vec![AnyValue {
                            value: Some(Value::IntValue(1)),
                        }],
                    }),
                )],
                vec![],
            ),
            (
                "only container.image.tags reads arrays",
                vec![attribute(CONTAINER_NAME, string_array(&["web"]))],
                vec![],
            ),
            (
                "later container.image.tags wins",
                vec![
                    string_attribute("container.image.tag", "old"),
                    attribute(CONTAINER_IMAGE_TAGS, string_array(&["new"])),
                ],
                vec!["image_tag:new"],
            ),
            (
                "later container.image.tag wins",
                vec![
                    attribute(CONTAINER_IMAGE_TAGS, string_array(&["new"])),
                    string_attribute("container.image.tag", "old"),
                ],
                vec!["image_tag:old"],
            ),
            (
                "empty later value doesn't replace",
                vec![
                    string_attribute("container.image.tag", "old"),
                    attribute(CONTAINER_IMAGE_TAGS, string_array(&[""])),
                ],
                vec!["image_tag:old"],
            ),
            (
                "semantic convention beats custom",
                vec![
                    attribute(CONTAINER_IMAGE_TAGS, string_array(&["v1"])),
                    string_attribute("datadog.container.tag.image_tag", "custom"),
                ],
                vec!["image_tag:v1"],
            ),
        ];

        for (name, attributes, expected) in cases {
            assert_eq!(resource_container_tags(attributes), expected, "{name}");
        }
    }

    #[test]
    fn resource_extraction_keeps_last_duplicate_semantic_convention_and_first_custom() {
        let attributes = vec![
            string_attribute(CONTAINER_ID, "first"),
            string_attribute(CONTAINER_ID, "second"),
            string_attribute("datadog.container.tag.team", "first"),
            string_attribute("datadog.container.tag.team", "second"),
        ];

        assert_eq!(
            resource_container_tags(attributes),
            vec!["container_id:second", "team:first"]
        );
    }

    #[test]
    fn write_infra_tag_attributes_preserves_sender_provided_prefixed_values() {
        // The sender pinned the prefixed form, so the generated copy must not replace it.
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from("datadog.container.tag.team"),
            AttributeValue::String(MetaString::from("sender-value")),
        );

        let tags = shared_tags(&["team:tagger-value"]);
        write_infra_tag_attributes(
            tags,
            ContainerTagPromotion::Duplicate,
            &test_interner(),
            &mut attributes,
        );

        // The unprefixed form is written with replace semantics; the prefixed copy is not.
        assert_eq!(attr_str(&attributes, "team"), Some("tagger-value".to_string()));
        assert_eq!(
            attr_str(&attributes, "datadog.container.tag.team"),
            Some("sender-value".to_string())
        );
    }

    #[test]
    fn write_infra_tag_attributes_rename_preserves_sender_provided_prefixed_values() {
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from("datadog.container.tag.team"),
            AttributeValue::String(MetaString::from("sender-value")),
        );

        let tags = shared_tags(&["team:tagger-value"]);
        write_infra_tag_attributes(tags, ContainerTagPromotion::Rename, &test_interner(), &mut attributes);

        assert_eq!(
            attr_str(&attributes, "datadog.container.tag.team"),
            Some("sender-value".to_string())
        );
    }

    #[test]
    fn resource_extraction_promotes_fresh_custom_tags() {
        let attributes = vec![string_attribute("datadog.container.tag.team", "checkout")];

        let mut tags = TagSet::default();
        extract_container_tags_from_resource_attributes(&attributes, &mut tags);

        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        assert_eq!(got, vec!["team:checkout"]);
    }

    #[test]
    fn resource_extraction_prefers_semantic_conventions_for_duplicate_keys() {
        let attributes = vec![
            string_attribute("container.id", "from-semconv"),
            string_attribute("datadog.container.tag.container_id", "from-custom"),
        ];

        let mut tags = TagSet::default();
        extract_container_tags_from_resource_attributes(&attributes, &mut tags);

        let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
        got.sort();
        assert_eq!(got, vec!["container_id:from-semconv"]);
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

        assert_eq!(source.kind, SourceKind::AwsEcsFargate);
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

    fn attribute_value(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn empty_attribute(key: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: None }),
        }
    }

    /// A named source resolution case: the attributes and the expected kind and identifier.
    type SourceCase = (&'static str, Vec<KeyValue>, Option<(SourceKind, String)>);

    fn resolved_source(attributes: Vec<KeyValue>) -> Option<(SourceKind, String)> {
        source_from_attributes(&attributes).map(|source| (source.kind, source.identifier))
    }

    fn host(identifier: &str) -> Option<(SourceKind, String)> {
        Some((SourceKind::Hostname, identifier.to_string()))
    }

    #[test]
    fn source_from_attributes_resolves_hostnames_in_agent_order() {
        let s = string_attribute;
        let cases: Vec<SourceCase> = vec![
            (
                "literal host beats datadog.host.name",
                vec![s(ATTRIBUTE_DATADOG_HOSTNAME, "custom"), s(ATTRIBUTE_HOST, "literal")],
                host("literal"),
            ),
            (
                "integer literal host",
                vec![
                    attribute_value(ATTRIBUTE_HOST, Value::IntValue(5)),
                    s(HOST_NAME, "name"),
                ],
                host("5"),
            ),
            (
                "boolean literal host",
                vec![attribute_value(ATTRIBUTE_HOST, Value::BoolValue(true))],
                host("true"),
            ),
            (
                "empty literal host",
                vec![empty_attribute(ATTRIBUTE_HOST), s(HOST_NAME, "name")],
                host(""),
            ),
            (
                "double literal host falls through until AsString exists",
                vec![
                    attribute_value(ATTRIBUTE_HOST, Value::DoubleValue(1.5)),
                    s(HOST_NAME, "name"),
                ],
                host("name"),
            ),
            (
                "invalid literal host doesn't fall through",
                vec![s(ATTRIBUTE_HOST, "localhost"), s(HOST_NAME, "name")],
                None,
            ),
            (
                "datadog.host.name beats Fargate and cloud providers",
                vec![
                    s(AWS_ECS_LAUNCHTYPE, "fargate"),
                    s(CLOUD_PROVIDER, "aws"),
                    s(HOST_ID, "i-123"),
                    s(ATTRIBUTE_DATADOG_HOSTNAME, "custom"),
                ],
                host("custom"),
            ),
            (
                "non-string datadog.host.name is an empty host",
                vec![
                    attribute_value(ATTRIBUTE_DATADOG_HOSTNAME, Value::IntValue(1)),
                    s(HOST_NAME, "name"),
                ],
                host(""),
            ),
            (
                "Fargate task",
                vec![
                    s(AWS_ECS_LAUNCHTYPE, "fargate"),
                    s(AWS_ECS_TASK_ARN, "arn"),
                    s(HOST_NAME, "name"),
                ],
                Some((SourceKind::AwsEcsFargate, "arn".to_string())),
            ),
            (
                "Fargate without a task ARN has no host",
                vec![s(AWS_ECS_LAUNCHTYPE, "fargate"), s(HOST_NAME, "name")],
                None,
            ),
            (
                "AWS uses host.id",
                vec![s(CLOUD_PROVIDER, "aws"), s(HOST_ID, "i-123"), s(HOST_NAME, "name")],
                host("i-123"),
            ),
            (
                "AWS without host.id doesn't fall back",
                vec![s(CLOUD_PROVIDER, "aws"), s(K8S_NODE_NAME, "node"), s(HOST_NAME, "name")],
                None,
            ),
            (
                "GCP shortens a qualified host.name",
                vec![
                    s(CLOUD_PROVIDER, "gcp"),
                    s(HOST_NAME, "vm.zone.c.project.internal"),
                    s(CLOUD_ACCOUNT_ID, "project"),
                ],
                host("vm.project"),
            ),
            (
                "GCP keeps a short host.name",
                vec![
                    s(CLOUD_PROVIDER, "gcp"),
                    s(HOST_NAME, "vm.zone"),
                    s(CLOUD_ACCOUNT_ID, "project"),
                ],
                host("vm.zone.project"),
            ),
            (
                "GCP needs the project",
                vec![s(CLOUD_PROVIDER, "gcp"), s(HOST_NAME, "vm")],
                None,
            ),
            (
                "Azure prefers host.id",
                vec![s(CLOUD_PROVIDER, "azure"), s(HOST_NAME, "name"), s(HOST_ID, "vm-id")],
                host("vm-id"),
            ),
            (
                "Azure falls back to host.name",
                vec![s(CLOUD_PROVIDER, "azure"), s(HOST_NAME, "name")],
                host("name"),
            ),
            (
                "Kubernetes node and cluster",
                vec![
                    s(K8S_CLUSTER_NAME, "cluster"),
                    s(K8S_NODE_NAME, "node"),
                    s(HOST_ID, "id"),
                ],
                host("node-cluster"),
            ),
            (
                "Kubernetes node with an empty cluster name",
                vec![s(K8S_NODE_NAME, "node"), s(K8S_CLUSTER_NAME, "")],
                host("node-"),
            ),
            (
                "Kubernetes node without a cluster",
                vec![s(K8S_NODE_NAME, "node"), s(HOST_NAME, "name")],
                host("node"),
            ),
            (
                "host.id beats host.name",
                vec![s(HOST_NAME, "name"), s(HOST_ID, "id")],
                host("id"),
            ),
            ("host.name", vec![s(HOST_NAME, "name")], host("name")),
            (
                "first duplicate wins even when it isn't a string",
                vec![attribute_value(HOST_NAME, Value::IntValue(1)), s(HOST_NAME, "name")],
                host(""),
            ),
            ("localhost", vec![s(HOST_NAME, "localhost")], None),
            ("0.0.0.0", vec![s(HOST_NAME, "0.0.0.0")], None),
            ("127.0.0.1", vec![s(HOST_NAME, "127.0.0.1")], None),
            (
                "localhost.localdomain",
                vec![s(HOST_NAME, "localhost.localdomain")],
                None,
            ),
            (
                "localhost6.localdomain6",
                vec![s(HOST_NAME, "localhost6.localdomain6")],
                None,
            ),
            ("ip6-localhost", vec![s(HOST_NAME, "ip6-localhost")], None),
            ("no hostname attributes", vec![s("other", "value")], None),
        ];

        for (name, attributes, expected) in cases {
            assert_eq!(resolved_source(attributes), expected, "{name}");
        }
    }

    #[test]
    fn source_from_attributes_resolves_serverless_platforms() {
        let s = string_attribute;
        let azure = |platform: &str| {
            vec![
                s(CLOUD_PLATFORM, platform),
                s(SERVICE_NAME, "app"),
                s(CLOUD_ACCOUNT_ID, "sub"),
                s("azure.resource_group.name", "rg"),
                s(HOST_NAME, "name"),
            ]
        };
        let gcp = |platform: &str| {
            vec![
                s(CLOUD_PLATFORM, platform),
                s(CLOUD_ACCOUNT_ID, "project"),
                s(CLOUD_REGION, "us-central1"),
                s(FAAS_NAME, "svc"),
                s(FAAS_INSTANCE, "instance-1"),
                s(HOST_NAME, "name"),
            ]
        };
        let without = |mut attributes: Vec<KeyValue>, key: &str| {
            attributes.retain(|attribute| attribute.key != key);
            attributes
        };
        let with = |mut attributes: Vec<KeyValue>, attribute: KeyValue| {
            attributes.insert(0, attribute);
            attributes
        };
        let kind = |kind: SourceKind, identifier: &str| Some((kind, identifier.to_string()));

        let cases: Vec<SourceCase> = vec![
            (
                "Azure Functions",
                azure("azure.functions"),
                kind(SourceKind::AzureFunctions, "app"),
            ),
            (
                "legacy Azure Functions platform",
                azure("azure_functions"),
                kind(SourceKind::AzureFunctions, "app"),
            ),
            (
                "incomplete Azure Functions falls back to the host",
                without(azure("azure.functions"), "azure.resource_group.name"),
                host("name"),
            ),
            (
                "Azure App Service",
                azure("azure.app_service"),
                kind(SourceKind::AzureAppService, "app"),
            ),
            (
                "legacy Azure App Service platform",
                azure("azure_app_service"),
                kind(SourceKind::AzureAppService, "app"),
            ),
            (
                "Azure App Service with an empty subscription falls back to the host",
                with(
                    without(azure("azure.app_service"), CLOUD_ACCOUNT_ID),
                    s(CLOUD_ACCOUNT_ID, ""),
                ),
                host("name"),
            ),
            (
                "Azure Container Apps replica",
                with(
                    azure("azure.container_apps"),
                    s("azure.container_app.instance.id", "replica"),
                ),
                kind(SourceKind::AzureContainerApps, "replica"),
            ),
            (
                "Azure Container Apps replica from service.instance.id",
                with(azure("azure_container_apps"), s(SERVICE_INSTANCE_ID, "instance")),
                kind(SourceKind::AzureContainerApps, "instance"),
            ),
            (
                "Azure Container Apps without a replica uses the app name",
                azure("azure.container_apps"),
                kind(SourceKind::AzureContainerApps, "app"),
            ),
            (
                "Azure Container Apps name from the resource ID",
                vec![
                    s(CLOUD_PLATFORM, "azure.container_apps"),
                    s(
                        CLOUD_RESOURCE_ID,
                        "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/containerApps/from-id",
                    ),
                ],
                kind(SourceKind::AzureContainerApps, "from-id"),
            ),
            (
                "Azure Container Apps without an identity still has no host",
                vec![s(CLOUD_PLATFORM, "azure.container_apps"), s(HOST_NAME, "name")],
                kind(SourceKind::AzureContainerApps, ""),
            ),
            (
                "Cloud Run service",
                gcp("gcp_cloud_run"),
                kind(SourceKind::GcpCloudRun, "instance-1"),
            ),
            (
                "Cloud Functions",
                gcp("gcp_cloud_functions"),
                kind(SourceKind::GcpCloudFunctions, "instance-1"),
            ),
            (
                "incomplete Cloud Run identity has no host",
                without(gcp("gcp_cloud_run"), FAAS_INSTANCE),
                None,
            ),
            (
                "non-string Cloud Run region has no host",
                with(
                    without(gcp("gcp_cloud_run"), CLOUD_REGION),
                    attribute_value(CLOUD_REGION, Value::IntValue(1)),
                ),
                None,
            ),
            (
                "Cloud Run job has no host",
                with(gcp("gcp_cloud_run"), s("gcp.cloud_run.job.execution", "")),
                None,
            ),
            (
                "Azure Functions beats Fargate",
                with(
                    with(azure("azure.functions"), s(AWS_ECS_LAUNCHTYPE, "fargate")),
                    s(AWS_ECS_TASK_ARN, "arn"),
                ),
                kind(SourceKind::AzureFunctions, "app"),
            ),
        ];

        for (name, attributes, expected) in cases {
            assert_eq!(resolved_source(attributes), expected, "{name}");
        }
    }

    #[test]
    fn ec2_cluster_name_uses_the_last_cluster_tag() {
        let attributes = vec![
            string_attribute("ec2.tag.kubernetes.io/cluster/first", "owned"),
            string_attribute("ec2.tag.kubernetes.io/cluster/second", "owned"),
        ];

        assert_eq!(ec2_cluster_name(&attributes), Some("second"));
    }

    #[test]
    fn serverless_tags_come_from_app_service_and_gcp_identities() {
        let s = string_attribute;
        let tags = |attributes: Vec<KeyValue>| {
            let mut tags = TagSet::default();
            extract_serverless_tags_from_resource_attributes(&attributes, &mut tags);
            let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
            got.sort();
            got
        };

        assert_eq!(
            tags(vec![
                s(CLOUD_PLATFORM, "azure.app_service"),
                s(SERVICE_NAME, "app"),
                s(CLOUD_ACCOUNT_ID, "sub"),
                s("azure.resource_group.name", "rg"),
            ]),
            vec!["name:app", "resource_group:rg", "subscription_id:sub"]
        );
        assert!(tags(vec![
            s(CLOUD_PLATFORM, "azure.functions"),
            s(SERVICE_NAME, "app"),
            s(CLOUD_ACCOUNT_ID, "sub"),
            s("azure.resource_group.name", "rg"),
        ])
        .is_empty());
        assert_eq!(
            tags(vec![
                s(CLOUD_PLATFORM, "gcp_cloud_run"),
                s(CLOUD_ACCOUNT_ID, "project"),
                s(CLOUD_REGION, "us-central1"),
                s(FAAS_NAME, "svc"),
                s(FAAS_INSTANCE, "instance-1"),
                s(FAAS_VERSION, "rev-1"),
            ]),
            vec![
                "instance:instance-1",
                "location:us-central1",
                "project_id:project",
                "revision_name:rev-1",
                "service_name:svc",
            ]
        );
        assert!(tags(vec![
            s(CLOUD_PLATFORM, "gcp_cloud_run"),
            s(CLOUD_ACCOUNT_ID, "project"),
            s(CLOUD_REGION, "us-central1"),
            s(FAAS_NAME, "svc"),
        ])
        .is_empty());
    }
}

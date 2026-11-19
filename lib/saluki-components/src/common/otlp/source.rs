//! Resolves the source of OTLP telemetry from its resource attributes.
//!
//! A source is the entity that emitted the telemetry: a host, or a serverless resource such as an AWS Lambda function.
//! Detectors run in a fixed order, and the first one that matches wins. Resources that match none of them fall back to
//! the hostname cascade in [`super::util`], except a Cloud Run or Cloud Functions resource with an incomplete
//! identity, which has no source even when it has a hostname.
//!
//! Logs, metrics, and traces all resolve their source here.

use std::sync::LazyLock;

use otlp_protos::opentelemetry::proto::common::v1::KeyValue;
use regex::Regex;
use saluki_core::data_model::tags::TagSet;

use super::util::{first_attribute_str, gcp_hostname, hostname_from_attributes, Source, SourceKind};

const ATTRIBUTE_HOST: &str = "host";
const ATTRIBUTE_DATADOG_HOSTNAME: &str = "datadog.host.name";
const AZURE_CONTAINER_APP_INSTANCE_ID: &str = "azure.container_app.instance.id";
const AZURE_RESOURCE_GROUP_NAME: &str = "azure.resource_group.name";
const AWS_ECS_LAUNCHTYPE: &str = "aws.ecs.launchtype";
const AWS_ECS_TASK_ARN: &str = "aws.ecs.task.arn";
const CLOUD_ACCOUNT_ID: &str = "cloud.account.id";
const CLOUD_PLATFORM: &str = "cloud.platform";
const CLOUD_PROVIDER: &str = "cloud.provider";
const CLOUD_REGION: &str = "cloud.region";
const CLOUD_RESOURCE_ID: &str = "cloud.resource_id";
const FAAS_ID: &str = "faas.id";
const FAAS_INSTANCE: &str = "faas.instance";
const FAAS_NAME: &str = "faas.name";
const FAAS_VERSION: &str = "faas.version";
const K8S_CLUSTER_NAME: &str = "k8s.cluster.name";
const SERVICE_INSTANCE_ID: &str = "service.instance.id";
const SERVICE_NAME: &str = "service.name";

const PLATFORM_AWS_LAMBDA: &str = "aws_lambda";
const PLATFORM_GCP_CLOUD_FUNCTIONS: &str = "gcp_cloud_functions";
const PLATFORM_GCP_CLOUD_RUN: &str = "gcp_cloud_run";
const PLATFORM_GKE: &str = "gcp_kubernetes_engine";

const AZURE_WEB_NAMESPACE: &str = "Microsoft.Web";
const AZURE_WEB_SITES: &str = "sites";
const AZURE_APP_NAMESPACE: &str = "Microsoft.App";
const AZURE_CONTAINER_APPS: &str = "containerApps";

static GCP_CLOUD_RUN_SERVICE_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?://run\.googleapis\.com/)?projects/([^/]+)/locations/([^/]+)/services/([^/]+)(?:/revisions/[^/]+)?$",
    )
    .expect("pattern should be valid")
});
static GCP_CLOUD_RUN_REVISION_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?://run\.googleapis\.com/)?projects/([^/]+)/locations/([^/]+)/revisions/[^/]+$")
        .expect("pattern should be valid")
});
static GCP_CLOUD_FUNCTIONS_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?://cloudfunctions\.googleapis\.com/)?projects/([^/]+)/locations/([^/]+)/(?:cloudfunctions|functions)/([^/]+)$",
    )
    .expect("pattern should be valid")
});
static GCP_CLOUD_RUN_JOB_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?://run\.googleapis\.com/)?projects/([^/]+)/locations/([^/]+)/jobs/([^/]+)(?:/executions/[^/]+(?:/tasks/[^/]+)?)?$",
    )
    .expect("pattern should be valid")
});

/// Resolves the source of telemetry from OTLP resource attributes.
///
/// Returns `None` when no source can be determined, and also for a GCP Cloud Run or Cloud Functions resource whose
/// identity is incomplete. Callers decide whether to fall back to a default hostname; see [`is_gcp_serverless`].
pub fn resolve_source_from_attributes(attributes: &[KeyValue]) -> Option<Source> {
    if let Some(source) = azure_functions_source(attributes) {
        return Some(source);
    }
    if let Some(source) = aws_lambda_source(attributes) {
        return Some(source);
    }
    if let Some(source) = gcp_cloud_run_jobs_source(attributes) {
        return Some(source);
    }
    // An incomplete GCP serverless identity must not fall through to the host cascade, which would report the
    // collector's own host.
    if is_gcp_serverless(attributes) {
        return gcp_serverless_source(attributes);
    }
    if let Some(source) = gke_autopilot_source(attributes) {
        return Some(source);
    }
    if let Some(source) = azure_app_service_source(attributes) {
        return Some(source);
    }
    if let Some(source) = azure_container_apps_source(attributes) {
        return Some(source);
    }
    if let Some(source) = ecs_fargate_source(attributes) {
        return Some(source);
    }

    hostname_from_attributes(attributes).map(|identifier| Source {
        kind: SourceKind::Hostname,
        identifier,
    })
}

/// Returns whether the resource declares a GCP Cloud Run or Cloud Functions platform.
///
/// This holds even when the identity is incomplete, or the resource is a job.
pub fn is_gcp_serverless(attributes: &[KeyValue]) -> bool {
    matches!(
        attr(attributes, CLOUD_PLATFORM),
        PLATFORM_GCP_CLOUD_FUNCTIONS | PLATFORM_GCP_CLOUD_RUN
    )
}

/// Adds the serverless identity tags derived from resource attributes: the Azure App Service app identity, and the
/// dimensions of an AWS Lambda function, a Cloud Run job, or a Cloud Run service or Cloud Functions function.
pub fn extract_serverless_tags_from_resource_attributes(attributes: &[KeyValue], tags: &mut TagSet) {
    if let Some(app) = azure_app_service_identity(attributes) {
        tags.insert_tag(format!("name:{}", app.name));
        tags.insert_tag(format!("subscription_id:{}", app.subscription_id));
        tags.insert_tag(format!("resource_group:{}", app.resource_group));
    }

    if let Some(function) = aws_lambda_function(attributes) {
        tags.insert_tag(format!("function_arn:{}", function.arn));
        tags.insert_tag(format!("function_name:{}", function.name));
        tags.insert_tag(format!("region:{}", function.region));
        tags.insert_tag(format!("aws_account:{}", function.account));
    }

    if let Some(job) = gcp_cloud_run_job(attributes) {
        tags.insert_tag(format!("project_id:{}", job.identity.project_id));
        tags.insert_tag(format!("location:{}", job.identity.location));
        tags.insert_tag(format!("job_name:{}", job.identity.workload_name));
        tags.insert_tag(format!("instance:{}", job.instance));
    }

    if let Some(workload) = gcp_serverless_workload(attributes) {
        tags.insert_tag(format!("project_id:{}", workload.identity.project_id));
        tags.insert_tag(format!("location:{}", workload.identity.location));
        tags.insert_tag(format!("service_name:{}", workload.identity.workload_name));
        tags.insert_tag(format!("instance:{}", workload.instance));
        let revision = attr(attributes, FAAS_VERSION);
        if !revision.is_empty() {
            tags.insert_tag(format!("revision_name:{revision}"));
        }
    }
}

/// Returns the first attribute named `key` as a string, or an empty string if it's missing or not a string.
fn attr<'a>(attributes: &'a [KeyValue], key: &str) -> &'a str {
    first_attribute_str(attributes, key).unwrap_or_default()
}

fn source(kind: SourceKind, identifier: impl Into<String>) -> Option<Source> {
    Some(Source {
        kind,
        identifier: identifier.into(),
    })
}

// ============================================================================
// Azure
// ============================================================================

/// Fields of an Azure Resource Manager ID, `/subscriptions/{id}/resourceGroups/{name}/providers/{namespace}/{type}/{name}`.
struct AzureResourceId<'a> {
    subscription_id: &'a str,
    resource_group: &'a str,
    provider_namespace: &'a str,
    resource_type: &'a str,
    resource_name: &'a str,
}

impl<'a> AzureResourceId<'a> {
    fn parse(resource_id: &'a str) -> Option<Self> {
        let parts: Vec<&str> = resource_id.split('/').collect();
        if parts.len() < 9
            || !parts[0].is_empty()
            || !parts[1].eq_ignore_ascii_case("subscriptions")
            || !parts[3].eq_ignore_ascii_case("resourceGroups")
            || !parts[5].eq_ignore_ascii_case("providers")
            || [parts[2], parts[4], parts[6], parts[7], parts[8]]
                .iter()
                .any(|part| part.is_empty())
        {
            return None;
        }

        Some(Self {
            subscription_id: parts[2],
            resource_group: parts[4],
            provider_namespace: parts[6],
            resource_type: parts[7],
            resource_name: parts[8],
        })
    }

    fn matches(&self, provider_namespace: &str, resource_type: &str) -> bool {
        self.provider_namespace.eq_ignore_ascii_case(provider_namespace)
            && self.resource_type.eq_ignore_ascii_case(resource_type)
    }
}

/// Identity of an Azure app, taken from attributes and filled in from `cloud.resource_id` where absent.
struct AzureIdentity<'a> {
    name: &'a str,
    subscription_id: &'a str,
    resource_group: &'a str,
}

impl<'a> AzureIdentity<'a> {
    fn from_attributes(attributes: &'a [KeyValue], provider_namespace: &str, resource_type: &str) -> Option<Self> {
        let mut identity = Self {
            name: attr(attributes, SERVICE_NAME),
            subscription_id: attr(attributes, CLOUD_ACCOUNT_ID),
            resource_group: attr(attributes, AZURE_RESOURCE_GROUP_NAME),
        };

        if let Some(parsed) = AzureResourceId::parse(attr(attributes, CLOUD_RESOURCE_ID)) {
            if parsed.matches(provider_namespace, resource_type) {
                if identity.name.is_empty() {
                    identity.name = parsed.resource_name;
                }
                if identity.subscription_id.is_empty() {
                    identity.subscription_id = parsed.subscription_id;
                }
                if identity.resource_group.is_empty() {
                    identity.resource_group = parsed.resource_group;
                }
            }
        }

        let complete =
            !identity.name.is_empty() && !identity.subscription_id.is_empty() && !identity.resource_group.is_empty();
        complete.then_some(identity)
    }
}

/// Returns whether `platform` is an Azure platform ending in `suffix`, such as `azure.functions` or `azure_functions`.
///
/// Any run of characters other than lowercase letters and digits separates `azure` from the suffix.
fn is_azure_cloud_platform(platform: &str, suffix: &str) -> bool {
    let platform = platform.to_lowercase();
    if !platform.starts_with("azure") || !platform.ends_with(suffix) {
        return false;
    }

    let separator = platform
        .len()
        .checked_sub(suffix.len())
        .and_then(|end| platform.get("azure".len()..end));
    separator.is_some_and(|separator| {
        !separator.is_empty()
            && separator
                .chars()
                .all(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
    })
}

fn azure_functions_source(attributes: &[KeyValue]) -> Option<Source> {
    if !is_azure_cloud_platform(attr(attributes, CLOUD_PLATFORM), "functions") {
        return None;
    }
    let identity = AzureIdentity::from_attributes(attributes, AZURE_WEB_NAMESPACE, AZURE_WEB_SITES)?;
    source(SourceKind::AzureFunctions, identity.name)
}

/// Returns the identity of an app on the Azure App Service platform.
fn azure_app_service_identity(attributes: &[KeyValue]) -> Option<AzureIdentity<'_>> {
    if !is_azure_cloud_platform(attr(attributes, CLOUD_PLATFORM), "app_service") {
        return None;
    }
    AzureIdentity::from_attributes(attributes, AZURE_WEB_NAMESPACE, AZURE_WEB_SITES)
}

fn azure_app_service_source(attributes: &[KeyValue]) -> Option<Source> {
    source(
        SourceKind::AzureAppService,
        azure_app_service_identity(attributes)?.name,
    )
}

fn azure_container_apps_source(attributes: &[KeyValue]) -> Option<Source> {
    if !is_azure_cloud_platform(attr(attributes, CLOUD_PLATFORM), "container_apps") {
        return None;
    }
    AzureIdentity::from_attributes(attributes, AZURE_APP_NAMESPACE, AZURE_CONTAINER_APPS)?;

    // Some SDKs report the replica in `service.instance.id`; the Azure-specific attribute stays authoritative.
    let replica = Some(attr(attributes, AZURE_CONTAINER_APP_INSTANCE_ID))
        .filter(|replica| !replica.is_empty())
        .unwrap_or_else(|| attr(attributes, SERVICE_INSTANCE_ID));
    if replica.is_empty() {
        return None;
    }
    source(SourceKind::AzureContainerApps, replica)
}

// ============================================================================
// AWS
// ============================================================================

/// An AWS Lambda function, identified by its unqualified ARN.
struct LambdaFunction<'a> {
    arn: String,
    name: &'a str,
    region: &'a str,
    account: &'a str,
}

fn aws_lambda_function(attributes: &[KeyValue]) -> Option<LambdaFunction<'_>> {
    if attr(attributes, CLOUD_PROVIDER) != "aws" {
        return None;
    }

    let platform = attr(attributes, CLOUD_PLATFORM);
    let faas_id = attr(attributes, FAAS_ID);
    let resource_id = attr(attributes, CLOUD_RESOURCE_ID);
    let function_arn = if faas_id.is_empty() { resource_id } else { faas_id };
    let has_faas_attribute = attributes.iter().any(|kv| kv.key.starts_with("faas."));
    if platform != PLATFORM_AWS_LAMBDA && faas_id.is_empty() && (resource_id.is_empty() || !has_faas_attribute) {
        return None;
    }

    let parts: Vec<&str> = function_arn.split(':').collect();
    if parts.len() < 7 || parts[0] != "arn" || parts[2] != "lambda" || parts[5] != "function" {
        return None;
    }
    Some(LambdaFunction {
        arn: parts[..7].join(":"),
        name: parts[6],
        region: parts[3],
        account: parts[4],
    })
}

fn aws_lambda_source(attributes: &[KeyValue]) -> Option<Source> {
    source(SourceKind::AwsLambda, aws_lambda_function(attributes)?.arn)
}

fn ecs_fargate_source(attributes: &[KeyValue]) -> Option<Source> {
    if !attr(attributes, AWS_ECS_LAUNCHTYPE).eq_ignore_ascii_case("fargate") {
        return None;
    }
    let task_arn = first_attribute_str(attributes, AWS_ECS_TASK_ARN)?;
    source(SourceKind::AwsEcsFargate, task_arn)
}

// ============================================================================
// GCP
// ============================================================================

#[derive(Default)]
struct GcpIdentity<'a> {
    project_id: &'a str,
    location: &'a str,
    workload_name: &'a str,
}

impl<'a> GcpIdentity<'a> {
    fn from_pattern(resource_id: &'a str, pattern: &Regex, include_name: bool) -> Self {
        let Some(captures) = pattern.captures(resource_id) else {
            return Self::default();
        };
        Self {
            project_id: captures.get(1).map_or("", |m| m.as_str()),
            location: captures.get(2).map_or("", |m| m.as_str()),
            workload_name: if include_name {
                captures.get(3).map_or("", |m| m.as_str())
            } else {
                ""
            },
        }
    }

    /// Overrides parsed fields with non-empty attributes.
    fn with_attributes(mut self, attributes: &'a [KeyValue]) -> Self {
        for (field, key) in [
            (&mut self.project_id, CLOUD_ACCOUNT_ID),
            (&mut self.location, CLOUD_REGION),
            (&mut self.workload_name, FAAS_NAME),
        ] {
            let value = attr(attributes, key);
            if !value.is_empty() {
                *field = value;
            }
        }
        self
    }

    fn is_complete(&self) -> bool {
        !self.project_id.is_empty() && !self.location.is_empty() && !self.workload_name.is_empty()
    }
}

fn has_gcp_cloud_run_job_evidence(attributes: &[KeyValue]) -> bool {
    attributes.iter().any(|kv| {
        matches!(
            kv.key.as_str(),
            "gcp.cloud_run.job.execution" | "gcp.cloud_run.job.task_index"
        )
    }) || GCP_CLOUD_RUN_JOB_RESOURCE.is_match(attr(attributes, CLOUD_RESOURCE_ID))
}

/// A complete Cloud Run job, Cloud Run service, or Cloud Functions identity, and the instance running it.
struct GcpWorkload<'a> {
    kind: SourceKind,
    identity: GcpIdentity<'a>,
    instance: &'a str,
}

fn gcp_cloud_run_job(attributes: &[KeyValue]) -> Option<GcpWorkload<'_>> {
    if attr(attributes, CLOUD_PLATFORM) != PLATFORM_GCP_CLOUD_RUN || !has_gcp_cloud_run_job_evidence(attributes) {
        return None;
    }

    let identity = GcpIdentity::from_pattern(attr(attributes, CLOUD_RESOURCE_ID), &GCP_CLOUD_RUN_JOB_RESOURCE, true)
        .with_attributes(attributes);
    let instance = attr(attributes, FAAS_INSTANCE);
    if !identity.is_complete() || instance.is_empty() {
        return None;
    }
    Some(GcpWorkload {
        kind: SourceKind::GcpCloudRunJobs,
        identity,
        instance,
    })
}

fn gcp_cloud_run_jobs_source(attributes: &[KeyValue]) -> Option<Source> {
    source(SourceKind::GcpCloudRunJobs, gcp_cloud_run_job(attributes)?.instance)
}

fn gcp_serverless_workload(attributes: &[KeyValue]) -> Option<GcpWorkload<'_>> {
    let resource_id = attr(attributes, CLOUD_RESOURCE_ID);
    let (kind, parsed) = match attr(attributes, CLOUD_PLATFORM) {
        // Functions take precedence over their underlying Cloud Run service.
        PLATFORM_GCP_CLOUD_FUNCTIONS => {
            let parsed = GcpIdentity::from_pattern(resource_id, &GCP_CLOUD_FUNCTIONS_RESOURCE, true);
            let parsed = if parsed.project_id.is_empty() {
                GcpIdentity::from_pattern(resource_id, &GCP_CLOUD_RUN_SERVICE_RESOURCE, true)
            } else {
                parsed
            };
            (SourceKind::GcpCloudFunctions, parsed)
        }
        PLATFORM_GCP_CLOUD_RUN => {
            let parsed = GcpIdentity::from_pattern(resource_id, &GCP_CLOUD_RUN_SERVICE_RESOURCE, true);
            let parsed = if parsed.project_id.is_empty() {
                GcpIdentity::from_pattern(resource_id, &GCP_CLOUD_RUN_REVISION_RESOURCE, false)
            } else {
                parsed
            };
            (SourceKind::GcpCloudRun, parsed)
        }
        _ => return None,
    };

    // A job is out of scope here; `gcp_cloud_run_jobs_source` handles complete ones.
    if has_gcp_cloud_run_job_evidence(attributes) {
        return None;
    }

    let identity = parsed.with_attributes(attributes);
    let instance = attr(attributes, FAAS_INSTANCE);
    if !identity.is_complete() || instance.is_empty() {
        return None;
    }
    Some(GcpWorkload {
        kind,
        identity,
        instance,
    })
}

fn gcp_serverless_source(attributes: &[KeyValue]) -> Option<Source> {
    let workload = gcp_serverless_workload(attributes)?;
    source(workload.kind, workload.instance)
}

fn gke_autopilot_source(attributes: &[KeyValue]) -> Option<Source> {
    if attr(attributes, CLOUD_PROVIDER) != "gcp" || attr(attributes, CLOUD_PLATFORM) != PLATFORM_GKE {
        return None;
    }

    let project_id = attr(attributes, CLOUD_ACCOUNT_ID);
    let cluster_name = attr(attributes, K8S_CLUSTER_NAME);
    let hostname = Some(attr(attributes, ATTRIBUTE_HOST))
        .filter(|host| !host.is_empty())
        .or_else(|| Some(attr(attributes, ATTRIBUTE_DATADOG_HOSTNAME)).filter(|host| !host.is_empty()))
        .map(str::to_owned)
        .or_else(|| gcp_hostname(attributes))
        .unwrap_or_default();
    if project_id.is_empty() || cluster_name.is_empty() || !hostname.to_lowercase().starts_with("gk3") {
        return None;
    }
    source(
        SourceKind::GkeAutopilot,
        format!("{project_id}:{cluster_name}:{hostname}"),
    )
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue};

    use super::*;

    fn attrs(entries: &[(&str, &str)]) -> Vec<KeyValue> {
        entries
            .iter()
            .map(|(key, value)| KeyValue {
                key: (*key).to_string(),
                value: Some(AnyValue {
                    value: Some(Value::StringValue((*value).to_string())),
                }),
            })
            .collect()
    }

    fn resolve(entries: &[(&str, &str)]) -> Option<(SourceKind, String)> {
        resolve_source_from_attributes(&attrs(entries)).map(|source| (source.kind, source.identifier))
    }

    const FUNCTION_ARN: &str = "arn:aws:lambda:us-east-1:123456789012:function:orders";

    #[test]
    fn falls_back_to_the_hostname_cascade() {
        assert_eq!(
            resolve(&[("host.name", "my-host")]),
            Some((SourceKind::Hostname, "my-host".to_string()))
        );
        assert_eq!(
            resolve(&[("datadog.host.name", "custom"), ("host.name", "my-host")]),
            Some((SourceKind::Hostname, "custom".to_string()))
        );
        assert_eq!(resolve(&[("host.name", "localhost")]), None);
        assert_eq!(resolve(&[]), None);
    }

    #[test]
    fn detects_ecs_fargate_with_a_task_arn() {
        let entries = [("aws.ecs.launchtype", "Fargate"), ("aws.ecs.task.arn", "arn:task")];
        assert_eq!(
            resolve(&entries),
            Some((SourceKind::AwsEcsFargate, "arn:task".to_string()))
        );

        // Without a task ARN, Fargate has no hostname either.
        assert_eq!(resolve(&[("aws.ecs.launchtype", "fargate"), ("host.name", "h")]), None);
    }

    #[test]
    fn detects_aws_lambda_from_platform_or_faas_id() {
        let expected = Some((SourceKind::AwsLambda, FUNCTION_ARN.to_string()));
        assert_eq!(
            resolve(&[
                ("cloud.provider", "aws"),
                ("cloud.platform", "aws_lambda"),
                ("faas.id", FUNCTION_ARN)
            ]),
            expected
        );
        assert_eq!(
            resolve(&[("cloud.provider", "aws"), ("faas.id", FUNCTION_ARN)]),
            expected
        );
        // `cloud.resource_id` counts only alongside another `faas.` attribute.
        assert_eq!(
            resolve(&[
                ("cloud.provider", "aws"),
                ("cloud.resource_id", FUNCTION_ARN),
                ("faas.name", "orders")
            ]),
            expected
        );
        assert_eq!(
            resolve(&[("cloud.provider", "aws"), ("cloud.resource_id", FUNCTION_ARN)]),
            None
        );
    }

    #[test]
    fn aws_lambda_keeps_only_the_unqualified_function_arn() {
        let qualified = format!("{FUNCTION_ARN}:live");
        assert_eq!(
            resolve(&[("cloud.provider", "aws"), ("faas.id", &qualified)]),
            Some((SourceKind::AwsLambda, FUNCTION_ARN.to_string()))
        );
        assert_eq!(
            resolve(&[("cloud.provider", "aws"), ("faas.id", "arn:aws:s3:::bucket")]),
            None
        );
    }

    #[test]
    fn aws_lambda_wins_over_host_attributes() {
        let source = resolve(&[
            ("cloud.provider", "aws"),
            ("cloud.platform", "aws_lambda"),
            ("faas.id", FUNCTION_ARN),
            ("host.id", "i-123"),
            ("host.name", "ip-10-0-0-1"),
        ]);
        assert_eq!(source, Some((SourceKind::AwsLambda, FUNCTION_ARN.to_string())));
    }

    #[test]
    fn detects_azure_functions_and_accepts_either_platform_spelling() {
        for platform in ["azure.functions", "azure_functions", "Azure.Functions"] {
            let source = resolve(&[
                ("cloud.platform", platform),
                ("service.name", "func-app"),
                ("cloud.account.id", "sub-1"),
                ("azure.resource_group.name", "rg-1"),
            ]);
            assert_eq!(
                source,
                Some((SourceKind::AzureFunctions, "func-app".to_string())),
                "{platform}"
            );
        }
        for platform in ["azurefunctions", "azure", "azure.functionsx", "aws.functions"] {
            let source = resolve(&[
                ("cloud.platform", platform),
                ("service.name", "func-app"),
                ("cloud.account.id", "sub-1"),
                ("azure.resource_group.name", "rg-1"),
            ]);
            assert_eq!(source, None, "{platform}");
        }
    }

    #[test]
    fn azure_identity_is_filled_in_from_the_resource_id() {
        let resource_id = "/subscriptions/sub-1/resourceGroups/rg-1/providers/Microsoft.Web/sites/site-1";
        assert_eq!(
            resolve(&[
                ("cloud.platform", "azure.app_service"),
                ("cloud.resource_id", resource_id)
            ]),
            Some((SourceKind::AzureAppService, "site-1".to_string()))
        );
        // The resource ID has to describe a site to be used.
        let other = "/subscriptions/sub-1/resourceGroups/rg-1/providers/Microsoft.Storage/accounts/acct";
        assert_eq!(
            resolve(&[("cloud.platform", "azure.app_service"), ("cloud.resource_id", other)]),
            None
        );
        // Attributes take precedence over the resource ID.
        assert_eq!(
            resolve(&[
                ("cloud.platform", "azure.app_service"),
                ("cloud.resource_id", resource_id),
                ("service.name", "named")
            ]),
            Some((SourceKind::AzureAppService, "named".to_string()))
        );
    }

    #[test]
    fn azure_container_apps_identifies_the_replica() {
        let resource_id = "/subscriptions/sub-1/resourceGroups/rg-1/providers/Microsoft.App/containerApps/app-1";
        let base = [
            ("cloud.platform", "azure.container_apps"),
            ("cloud.resource_id", resource_id),
        ];

        assert_eq!(resolve(&base), None);

        let mut with_instance = base.to_vec();
        with_instance.push(("service.instance.id", "replica-b"));
        assert_eq!(
            resolve(&with_instance),
            Some((SourceKind::AzureContainerApps, "replica-b".to_string()))
        );

        with_instance.push(("azure.container_app.instance.id", "replica-a"));
        assert_eq!(
            resolve(&with_instance),
            Some((SourceKind::AzureContainerApps, "replica-a".to_string()))
        );
    }

    #[test]
    fn detects_gcp_cloud_run_and_cloud_functions() {
        let service = [
            ("cloud.platform", "gcp_cloud_run"),
            (
                "cloud.resource_id",
                "//run.googleapis.com/projects/p/locations/us-central1/services/svc",
            ),
            ("faas.instance", "instance-1"),
        ];
        assert_eq!(
            resolve(&service),
            Some((SourceKind::GcpCloudRun, "instance-1".to_string()))
        );

        let function = [
            ("cloud.platform", "gcp_cloud_functions"),
            ("cloud.account.id", "p"),
            ("cloud.region", "us-central1"),
            ("faas.name", "fn"),
            ("faas.instance", "instance-2"),
        ];
        assert_eq!(
            resolve(&function),
            Some((SourceKind::GcpCloudFunctions, "instance-2".to_string()))
        );
    }

    #[test]
    fn incomplete_gcp_serverless_does_not_fall_back_to_the_host() {
        let source = resolve(&[
            ("cloud.platform", "gcp_cloud_run"),
            ("cloud.account.id", "p"),
            ("host.name", "collector-host"),
        ]);
        assert_eq!(source, None);
        assert!(is_gcp_serverless(&attrs(&[("cloud.platform", "gcp_cloud_run")])));
        assert!(!is_gcp_serverless(&attrs(&[("cloud.platform", "gcp_compute_engine")])));
    }

    #[test]
    fn detects_gcp_cloud_run_jobs() {
        let job = [
            ("cloud.platform", "gcp_cloud_run"),
            (
                "cloud.resource_id",
                "projects/p/locations/us-central1/jobs/job-1/executions/e-1/tasks/0",
            ),
            ("faas.instance", "instance-3"),
        ];
        assert_eq!(
            resolve(&job),
            Some((SourceKind::GcpCloudRunJobs, "instance-3".to_string()))
        );

        // A job never resolves as a Cloud Run service, even when its identity is incomplete.
        let mut incomplete = job.to_vec();
        incomplete.retain(|(key, _)| *key != "faas.instance");
        assert_eq!(resolve(&incomplete), None);
    }

    #[test]
    fn detects_gke_autopilot_by_node_name() {
        let autopilot = [
            ("cloud.provider", "gcp"),
            ("cloud.platform", "gcp_kubernetes_engine"),
            ("cloud.account.id", "p"),
            ("k8s.cluster.name", "cluster"),
            ("host.name", "GK3-cluster-node"),
        ];
        assert_eq!(
            resolve(&autopilot),
            Some((SourceKind::GkeAutopilot, "p:cluster:GK3-cluster-node.p".to_string()))
        );

        let mut standard = autopilot.to_vec();
        standard.retain(|(key, _)| *key != "host.name");
        standard.push(("host.name", "gke-cluster-node"));
        assert_eq!(
            resolve(&standard),
            Some((SourceKind::Hostname, "gke-cluster-node.p".to_string()))
        );
    }

    #[test]
    fn serverless_kinds_are_not_hostname_sources() {
        for entries in [
            vec![("cloud.provider", "aws"), ("faas.id", FUNCTION_ARN), ("host.name", "h")],
            vec![
                ("cloud.platform", "azure.functions"),
                ("service.name", "f"),
                ("cloud.account.id", "s"),
                ("azure.resource_group.name", "r"),
                ("host.name", "h"),
            ],
        ] {
            let source = resolve(&entries).expect("serverless source should resolve");
            assert_ne!(source.0, SourceKind::Hostname);
        }
    }

    /// A named source resolution case: the attributes and the expected kind and identifier.
    type SourceCase = (
        &'static str,
        Vec<(&'static str, &'static str)>,
        Option<(SourceKind, String)>,
    );

    #[test]
    fn incomplete_identities_and_precedence() {
        let azure = |platform: &'static str| {
            vec![
                ("cloud.platform", platform),
                ("service.name", "app"),
                ("cloud.account.id", "sub"),
                ("azure.resource_group.name", "rg"),
                ("host.name", "name"),
            ]
        };
        let without = |mut entries: Vec<(&'static str, &'static str)>, key: &str| {
            entries.retain(|(k, _)| *k != key);
            entries
        };
        let host = |identifier: &str| Some((SourceKind::Hostname, identifier.to_string()));

        let cases: Vec<SourceCase> = vec![
            (
                "incomplete Azure Functions falls back to the host",
                without(azure("azure.functions"), "azure.resource_group.name"),
                host("name"),
            ),
            (
                "Azure App Service with an empty subscription falls back to the host",
                {
                    let mut entries = without(azure("azure.app_service"), "cloud.account.id");
                    entries.push(("cloud.account.id", ""));
                    entries
                },
                host("name"),
            ),
            (
                "Azure Container Apps without a replica falls back to the host",
                azure("azure.container_apps"),
                host("name"),
            ),
            (
                "Azure Functions beats ECS Fargate",
                {
                    let mut entries = azure("azure.functions");
                    entries.extend([("aws.ecs.launchtype", "fargate"), ("aws.ecs.task.arn", "arn")]);
                    entries
                },
                Some((SourceKind::AzureFunctions, "app".to_string())),
            ),
        ];

        for (name, entries, expected) in cases {
            assert_eq!(resolve(&entries), expected, "{name}");
        }
    }

    #[test]
    fn reads_the_first_attribute_even_when_it_is_not_a_string() {
        let mut attributes = vec![KeyValue {
            key: "cloud.platform".to_string(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(1)),
            }),
        }];
        attributes.extend(attrs(&[
            ("cloud.platform", "azure.functions"),
            ("service.name", "app"),
            ("cloud.account.id", "sub"),
            ("azure.resource_group.name", "rg"),
            ("host.name", "name"),
        ]));

        assert_eq!(
            resolve_source_from_attributes(&attributes).map(|source| (source.kind, source.identifier)),
            Some((SourceKind::Hostname, "name".to_string()))
        );
    }

    #[test]
    fn serverless_tags_come_from_the_source_identities() {
        let tags = |entries: &[(&str, &str)]| {
            let mut tags = TagSet::default();
            extract_serverless_tags_from_resource_attributes(&attrs(entries), &mut tags);
            let mut got: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
            got.sort();
            got
        };

        assert_eq!(
            tags(&[
                ("cloud.platform", "azure.app_service"),
                ("service.name", "app"),
                ("cloud.account.id", "sub"),
                ("azure.resource_group.name", "rg"),
            ]),
            vec!["name:app", "resource_group:rg", "subscription_id:sub"]
        );
        assert!(tags(&[
            ("cloud.platform", "azure.functions"),
            ("service.name", "app"),
            ("cloud.account.id", "sub"),
            ("azure.resource_group.name", "rg"),
        ])
        .is_empty());
        assert_eq!(
            tags(&[("cloud.provider", "aws"), ("faas.id", FUNCTION_ARN)]),
            vec![
                "aws_account:123456789012",
                "function_arn:arn:aws:lambda:us-east-1:123456789012:function:orders",
                "function_name:orders",
                "region:us-east-1",
            ]
        );
        let cloud_run = [
            ("cloud.platform", "gcp_cloud_run"),
            ("cloud.account.id", "project"),
            ("cloud.region", "us-central1"),
            ("faas.name", "svc"),
            ("faas.instance", "instance-1"),
        ];
        let mut service = cloud_run.to_vec();
        service.push(("faas.version", "rev-1"));
        assert_eq!(
            tags(&service),
            vec![
                "instance:instance-1",
                "location:us-central1",
                "project_id:project",
                "revision_name:rev-1",
                "service_name:svc",
            ]
        );
        let mut job = cloud_run.to_vec();
        job.push(("gcp.cloud_run.job.execution", "job-1"));
        assert_eq!(
            tags(&job),
            vec![
                "instance:instance-1",
                "job_name:svc",
                "location:us-central1",
                "project_id:project",
            ]
        );
        assert!(tags(&cloud_run[..4]).is_empty());
    }
}

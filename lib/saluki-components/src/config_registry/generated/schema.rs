// @generated — DO NOT EDIT
// Regenerate with: cargo xtask gen-config-schema
// Source: lib/saluki-components/vendor/core_schema.yaml

#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(non_upper_case_globals)]

use crate::config_registry::{SchemaEntry, ValueType};

pub const GUI_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "GUI_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost\""),
};

pub const GUI_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "GUI_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: None,
};

pub const GUI_SESSION_EXPIRATION: SchemaEntry = SchemaEntry {
    yaml_path: "GUI_session_expiration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const AC_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "ac_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AC_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "ac_include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AD_ALLOWED_ENV_VARS: SchemaEntry = SchemaEntry {
    yaml_path: "ad_allowed_env_vars",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AD_CONFIG_POLL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "ad_config_poll_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const AD_DISABLE_ENV_VAR_RESOLUTION: SchemaEntry = SchemaEntry {
    yaml_path: "ad_disable_env_var_resolution",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AD_TAG_COMPLETENESS_MAX_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "ad_tag_completeness_max_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const ADDITIONAL_CHECKSD: SchemaEntry = SchemaEntry {
    yaml_path: "additional_checksd",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${conf_path}/checks.d\""),
};

// TODO: value_type unknown for 'additional_endpoints' — set an override in the annotation
pub const ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const ADMISSION_CONTROLLER_ADD_AKS_SELECTORS: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.add_aks_selectors",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_CLUSTER_AGENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.cluster_agent.enabled",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"true\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_CLUSTER_AGENT_TLS_VERIFICATION_COPY_CA_CONFIGMAP: SchemaEntry =
    SchemaEntry {
        yaml_path: "admission_controller.agent_sidecar.cluster_agent.tls_verification.copy_ca_configmap",
        env_vars: &[],
        value_type: ValueType::Bool,
        default: Some("false"),
    };

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_CLUSTER_AGENT_TLS_VERIFICATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.cluster_agent.tls_verification.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_CONTAINER_REGISTRY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.container_registry",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/agentsidecar\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_IMAGE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.image_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"agent\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_IMAGE_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.image_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"latest\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_KUBELET_API_LOGGING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.kubelet_api_logging.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_PROFILES: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.profiles",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"[]\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AGENT_SIDECAR_SELECTORS: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.agent_sidecar.selectors",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"[]\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_BODY_PARSING_SIZE_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.body_parsing_size_limit",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_HEALTH_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.health_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8081"),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_IMAGE: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.image",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"ghcr.io/datadog/dd-trace-go/service-extensions-callout\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_IMAGE_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.image_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"latest\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8080"),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_RESOURCES_LIMITS_CPU: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.resources.limits.cpu",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_RESOURCES_LIMITS_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.resources.limits.memory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_RESOURCES_REQUESTS_CPU: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.resources.requests.cpu",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10m\""),
};

pub const ADMISSION_CONTROLLER_APPSEC_SIDECAR_RESOURCES_REQUESTS_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.appsec.sidecar.resources.requests.memory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"128Mi\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_ASM_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.asm.enabled",
    env_vars: &["DD_ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_APPSEC_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_ASM_SCA_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.asm_sca.enabled",
    env_vars: &["DD_ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_APPSEC_SCA_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_CONTAINER_REGISTRY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.container_registry",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_DEFAULT_DD_REGISTRIES: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.default_dd_registries",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/injectlib\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_GRADUAL_ROLLOUT_CACHE_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.gradual_rollout.cache_ttl",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_GRADUAL_ROLLOUT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.gradual_rollout.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_IAST_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.iast.enabled",
    env_vars: &["DD_ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_IAST_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_INIT_RESOURCES_CPU: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.init_resources.cpu",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_INIT_RESOURCES_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.init_resources.memory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_INIT_SECURITY_CONTEXT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.init_security_context",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_INJECT_AUTO_DETECTED_LIBRARIES: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.inject_auto_detected_libraries",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_PATCHER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.patcher.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_PATCHER_FALLBACK_TO_FILE_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.patcher.fallback_to_file_provider",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_PATCHER_FILE_PROVIDER_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.patcher.file_provider_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/etc/datadog-agent/patch/auto-instru.json\""),
};

pub const ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_PROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.auto_instrumentation.profiling.enabled",
    env_vars: &["DD_ADMISSION_CONTROLLER_AUTO_INSTRUMENTATION_PROFILING_ENABLED"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_CERTIFICATE_EXPIRATION_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.certificate.expiration_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("720"),
};

pub const ADMISSION_CONTROLLER_CERTIFICATE_SECRET_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.certificate.secret_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"webhook-certificate\""),
};

pub const ADMISSION_CONTROLLER_CERTIFICATE_VALIDITY_BOUND: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.certificate.validity_bound",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8760"),
};

pub const ADMISSION_CONTROLLER_CONTAINER_REGISTRY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.container_registry",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"gcr.io/datadoghq\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_COMMAND_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.command_endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/inject-command-cws\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_CONTAINER_REGISTRY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.container_registry",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_IMAGE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.image_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"cws-instrumentation\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_IMAGE_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.image_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"latest\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_INIT_RESOURCES_CPU: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.init_resources.cpu",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_INIT_RESOURCES_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.init_resources.memory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"remote_copy\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_MUTATE_UNLABELLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.mutate_unlabelled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_POD_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.pod_endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/inject-pod-cws\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_REMOTE_COPY_DIRECTORY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.remote_copy.directory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/tmp\""),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_REMOTE_COPY_MOUNT_VOLUME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.remote_copy.mount_volume",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_CWS_INSTRUMENTATION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.cws_instrumentation.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const ADMISSION_CONTROLLER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_FAILURE_POLICY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.failure_policy",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"Ignore\""),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/injectconfig\""),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_LOCAL_SERVICE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.local_service_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog\""),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"hostip\""),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_SOCKET_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.socket_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/var/run/datadog\""),
};

pub const ADMISSION_CONTROLLER_INJECT_CONFIG_TYPE_SOCKET_VOLUMES: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_config.type_socket_volumes",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_INJECT_TAGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_tags.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_INJECT_TAGS_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_tags.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/injecttags\""),
};

pub const ADMISSION_CONTROLLER_INJECT_TAGS_POD_OWNERS_CACHE_VALIDITY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.inject_tags.pod_owners_cache_validity",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const ADMISSION_CONTROLLER_KUBERNETES_ADMISSION_EVENTS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.kubernetes_admission_events.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_MUTATE_UNLABELLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.mutate_unlabelled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_MUTATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.mutation.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_NAMESPACE_SELECTOR_FALLBACK: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.namespace_selector_fallback",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_POD_OWNERS_CACHE_VALIDITY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.pod_owners_cache_validity",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const ADMISSION_CONTROLLER_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8000"),
};

pub const ADMISSION_CONTROLLER_PROBE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.probe.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ADMISSION_CONTROLLER_PROBE_GRACE_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.probe.grace_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const ADMISSION_CONTROLLER_PROBE_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.probe.interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const ADMISSION_CONTROLLER_REINVOCATION_POLICY: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.reinvocation_policy",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"IfNeeded\""),
};

pub const ADMISSION_CONTROLLER_SERVICE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.service_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog-admission-controller\""),
};

pub const ADMISSION_CONTROLLER_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const ADMISSION_CONTROLLER_VALIDATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.validation.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ADMISSION_CONTROLLER_WEBHOOK_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "admission_controller.webhook_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog-webhook\""),
};

pub const AGENT_IPC_CONFIG_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_ipc.config_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const AGENT_IPC_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "agent_ipc.host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost\""),
};

pub const AGENT_IPC_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "agent_ipc.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const AGENT_IPC_SOCKET_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "agent_ipc.socket_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${run_path}/agent_ipc.socket\""),
};

pub const AGENT_IPC_USE_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "agent_ipc.use_socket",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'agent_telemetry.additional_endpoints' — set an override in the annotation
pub const AGENT_TELEMETRY_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const AGENT_TELEMETRY_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const AGENT_TELEMETRY_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const AGENT_TELEMETRY_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const AGENT_TELEMETRY_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const AGENT_TELEMETRY_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const AGENT_TELEMETRY_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const AGENT_TELEMETRY_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'agent_telemetry.dd_url' — set an override in the annotation
pub const AGENT_TELEMETRY_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const AGENT_TELEMETRY_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AGENT_TELEMETRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const AGENT_TELEMETRY_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'agent_telemetry.logs_dd_url' — set an override in the annotation
pub const AGENT_TELEMETRY_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const AGENT_TELEMETRY_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AGENT_TELEMETRY_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const AGENT_TELEMETRY_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const AGENT_TELEMETRY_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const AGENT_TELEMETRY_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const AGENT_TELEMETRY_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AGENT_TELEMETRY_STARTUP_TRACE_SAMPLING: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.startup_trace_sampling",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const AGENT_TELEMETRY_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const AGENT_TELEMETRY_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const AGENT_TELEMETRY_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "agent_telemetry.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const AGGREGATOR_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "aggregator_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "aggregator_flush_metrics_and_serialize_in_parallel_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4000"),
};

pub const AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "aggregator_flush_metrics_and_serialize_in_parallel_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("200"),
};

pub const AGGREGATOR_STOP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "aggregator_stop_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const AGGREGATOR_USE_TAGS_STORE: SchemaEntry = SchemaEntry {
    yaml_path: "aggregator_use_tags_store",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ALLOW_ARBITRARY_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "allow_arbitrary_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ALLOW_PYTHON_PATH_HEURISTICS_FAILURE: SchemaEntry = SchemaEntry {
    yaml_path: "allow_python_path_heuristics_failure",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ALLOWED_ADDITIONAL_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "allowed_additional_checks",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'apm_config.additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.additional_endpoints",
    env_vars: &["DD_APM_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.additional_profile_tags' — set an override in the annotation
pub const APM_CONFIG_ADDITIONAL_PROFILE_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.additional_profile_tags",
    env_vars: &["DD_APM_ADDITIONAL_PROFILE_TAGS"],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'apm_config.analyzed_rate_by_service' — set an override in the annotation
pub const APM_CONFIG_ANALYZED_RATE_BY_SERVICE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.analyzed_rate_by_service",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'apm_config.analyzed_spans' — set an override in the annotation
pub const APM_CONFIG_ANALYZED_SPANS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.analyzed_spans",
    env_vars: &["DD_APM_ANALYZED_SPANS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.apm_dd_url' — set an override in the annotation
pub const APM_CONFIG_APM_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.apm_dd_url",
    env_vars: &["DD_APM_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.apm_non_local_traffic' — set an override in the annotation
pub const APM_CONFIG_APM_NON_LOCAL_TRAFFIC: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.apm_non_local_traffic",
    env_vars: &["DD_APM_NON_LOCAL_TRAFFIC"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_BUCKET_SIZE_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.bucket_size_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const APM_CONFIG_CLIENT_STATS_FLUSH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.client_stats_flush_interval",
    env_vars: &["DD_APM_CLIENT_STATS_FLUSH_INTERVAL"],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const APM_CONFIG_COMPUTE_STATS_BY_SPAN_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.compute_stats_by_span_kind",
    env_vars: &["DD_APM_COMPUTE_STATS_BY_SPAN_KIND"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.connection_limit' — set an override in the annotation
pub const APM_CONFIG_CONNECTION_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.connection_limit",
    env_vars: &["DD_APM_CONNECTION_LIMIT", "DD_CONNECTION_LIMIT"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.connection_reset_interval' — set an override in the annotation
pub const APM_CONFIG_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.connection_reset_interval",
    env_vars: &["DD_APM_CONNECTION_RESET_INTERVAL"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_DD_AGENT_BIN: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.dd_agent_bin",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const APM_CONFIG_DEBUG_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debug.port",
    env_vars: &["DD_APM_DEBUG_PORT"],
    value_type: ValueType::Float,
    default: Some("5012"),
};

pub const APM_CONFIG_DEBUG_V1_PAYLOADS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debug_v1_payloads",
    env_vars: &["DD_APM_DEBUG_V1_PAYLOADS"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'apm_config.debugger_additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_additional_endpoints",
    env_vars: &["DD_APM_DEBUGGER_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.debugger_api_key' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_api_key",
    env_vars: &["DD_APM_DEBUGGER_API_KEY"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.debugger_dd_url' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_dd_url",
    env_vars: &["DD_APM_DEBUGGER_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.debugger_diagnostics_additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_DIAGNOSTICS_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_diagnostics_additional_endpoints",
    env_vars: &["DD_APM_DEBUGGER_DIAGNOSTICS_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.debugger_diagnostics_api_key' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_DIAGNOSTICS_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_diagnostics_api_key",
    env_vars: &["DD_APM_DEBUGGER_DIAGNOSTICS_API_KEY"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.debugger_diagnostics_dd_url' — set an override in the annotation
pub const APM_CONFIG_DEBUGGER_DIAGNOSTICS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.debugger_diagnostics_dd_url",
    env_vars: &["DD_APM_DEBUGGER_DIAGNOSTICS_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.decoder_timeout' — set an override in the annotation
pub const APM_CONFIG_DECODER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.decoder_timeout",
    env_vars: &["DD_APM_DECODER_TIMEOUT"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.decoders' — set an override in the annotation
pub const APM_CONFIG_DECODERS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.decoders",
    env_vars: &["DD_APM_DECODERS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.disable_rare_sampler' — set an override in the annotation
pub const APM_CONFIG_DISABLE_RARE_SAMPLER: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.disable_rare_sampler",
    env_vars: &["DD_APM_DISABLE_RARE_SAMPLER"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_ENABLE_CONTAINER_TAGS_BUFFER: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.enable_container_tags_buffer",
    env_vars: &["DD_APM_ENABLE_CONTAINER_TAGS_BUFFER"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.enable_rare_sampler' — set an override in the annotation
pub const APM_CONFIG_ENABLE_RARE_SAMPLER: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.enable_rare_sampler",
    env_vars: &["DD_APM_ENABLE_RARE_SAMPLER"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.enabled",
    env_vars: &["DD_APM_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.env' — set an override in the annotation
pub const APM_CONFIG_ENV: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.env",
    env_vars: &["DD_APM_ENV"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_ERROR_TRACKING_STANDALONE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.error_tracking_standalone.enabled",
    env_vars: &["DD_APM_ERROR_TRACKING_STANDALONE_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'apm_config.errors_per_second' — set an override in the annotation
pub const APM_CONFIG_ERRORS_PER_SECOND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.errors_per_second",
    env_vars: &["DD_APM_ERROR_TPS"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_EXTRA_SAMPLE_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.extra_sample_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const APM_CONFIG_FEATURES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.features",
    env_vars: &["DD_APM_FEATURES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_FILTER_TAGS_REJECT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.filter_tags.reject",
    env_vars: &["DD_APM_FILTER_TAGS_REJECT"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_FILTER_TAGS_REQUIRE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.filter_tags.require",
    env_vars: &["DD_APM_FILTER_TAGS_REQUIRE"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_FILTER_TAGS_REGEX_REJECT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.filter_tags_regex.reject",
    env_vars: &["DD_APM_FILTER_TAGS_REGEX_REJECT"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_FILTER_TAGS_REGEX_REQUIRE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.filter_tags_regex.require",
    env_vars: &["DD_APM_FILTER_TAGS_REGEX_REQUIRE"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

// TODO: value_type unknown for 'apm_config.ignore_resources' — set an override in the annotation
pub const APM_CONFIG_IGNORE_RESOURCES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.ignore_resources",
    env_vars: &["DD_APM_IGNORE_RESOURCES", "DD_IGNORE_RESOURCE"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.install_id' — set an override in the annotation
pub const APM_CONFIG_INSTALL_ID: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.install_id",
    env_vars: &["DD_INSTRUMENTATION_INSTALL_ID"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.install_time' — set an override in the annotation
pub const APM_CONFIG_INSTALL_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.install_time",
    env_vars: &["DD_INSTRUMENTATION_INSTALL_TIME"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.install_type' — set an override in the annotation
pub const APM_CONFIG_INSTALL_TYPE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.install_type",
    env_vars: &["DD_INSTRUMENTATION_INSTALL_TYPE"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_INSTRUMENTATION_DISABLED_NAMESPACES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.disabled_namespaces",
    env_vars: &["DD_APM_INSTRUMENTATION_DISABLED_NAMESPACES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_INSTRUMENTATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.enabled",
    env_vars: &["DD_APM_INSTRUMENTATION_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_INSTRUMENTATION_ENABLED_NAMESPACES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.enabled_namespaces",
    env_vars: &["DD_APM_INSTRUMENTATION_ENABLED_NAMESPACES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_INSTRUMENTATION_INJECTION_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.injection_mode",
    env_vars: &["DD_APM_INSTRUMENTATION_INJECTION_MODE"],
    value_type: ValueType::String,
    default: Some("\"auto\""),
};

pub const APM_CONFIG_INSTRUMENTATION_INJECTOR_IMAGE_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.injector_image_tag",
    env_vars: &["DD_APM_INSTRUMENTATION_INJECTOR_IMAGE_TAG"],
    value_type: ValueType::String,
    default: Some("\"0\""),
};

// TODO: value_type unknown for 'apm_config.instrumentation.lib_versions' — set an override in the annotation
pub const APM_CONFIG_INSTRUMENTATION_LIB_VERSIONS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.lib_versions",
    env_vars: &["DD_APM_INSTRUMENTATION_LIB_VERSIONS"],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'apm_config.instrumentation.targets' — set an override in the annotation
pub const APM_CONFIG_INSTRUMENTATION_TARGETS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.instrumentation.targets",
    env_vars: &["DD_APM_INSTRUMENTATION_TARGETS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.internal_profiling.enabled' — set an override in the annotation
pub const APM_CONFIG_INTERNAL_PROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.internal_profiling.enabled",
    env_vars: &["DD_APM_INTERNAL_PROFILING_ENABLED"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.log_file' — set an override in the annotation
pub const APM_CONFIG_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.log_file",
    env_vars: &["DD_APM_LOG_FILE"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_MAX_CATALOG_ENTRIES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_catalog_entries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'apm_config.max_catalog_services' — set an override in the annotation
pub const APM_CONFIG_MAX_CATALOG_SERVICES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_catalog_services",
    env_vars: &["DD_APM_MAX_CATALOG_SERVICES"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_connections' — set an override in the annotation
pub const APM_CONFIG_MAX_CONNECTIONS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_connections",
    env_vars: &["DD_APM_MAX_CONNECTIONS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_cpu_percent' — set an override in the annotation
pub const APM_CONFIG_MAX_CPU_PERCENT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_cpu_percent",
    env_vars: &["DD_APM_MAX_CPU_PERCENT"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_events_per_second' — set an override in the annotation
pub const APM_CONFIG_MAX_EVENTS_PER_SECOND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_events_per_second",
    env_vars: &["DD_APM_MAX_EPS", "DD_MAX_EPS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_memory' — set an override in the annotation
pub const APM_CONFIG_MAX_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_memory",
    env_vars: &["DD_APM_MAX_MEMORY"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_payload_size' — set an override in the annotation
pub const APM_CONFIG_MAX_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_payload_size",
    env_vars: &["DD_APM_MAX_PAYLOAD_SIZE"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_remote_traces_per_second' — set an override in the annotation
pub const APM_CONFIG_MAX_REMOTE_TRACES_PER_SECOND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_remote_traces_per_second",
    env_vars: &["DD_APM_MAX_REMOTE_TPS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_sender_retries' — set an override in the annotation
pub const APM_CONFIG_MAX_SENDER_RETRIES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_sender_retries",
    env_vars: &["DD_APM_MAX_SENDER_RETRIES"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.max_traces_per_second' — set an override in the annotation
pub const APM_CONFIG_MAX_TRACES_PER_SECOND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.max_traces_per_second",
    env_vars: &["DD_APM_MAX_TPS", "DD_MAX_TPS"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.mode",
    env_vars: &["DD_APM_MODE"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const APM_CONFIG_OBFUSCATION_CACHE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.cache.enabled",
    env_vars: &["DD_APM_OBFUSCATION_CACHE_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_CACHE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.cache.max_size",
    env_vars: &["DD_APM_OBFUSCATION_CACHE_MAX_SIZE"],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const APM_CONFIG_OBFUSCATION_CREDIT_CARDS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.credit_cards.enabled",
    env_vars: &["DD_APM_OBFUSCATION_CREDIT_CARDS_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_CREDIT_CARDS_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.credit_cards.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_CREDIT_CARDS_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_CREDIT_CARDS_LUHN: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.credit_cards.luhn",
    env_vars: &["DD_APM_OBFUSCATION_CREDIT_CARDS_LUHN"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_ELASTICSEARCH_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.elasticsearch.enabled",
    env_vars: &["DD_APM_OBFUSCATION_ELASTICSEARCH_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_ELASTICSEARCH_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.elasticsearch.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_ELASTICSEARCH_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_ELASTICSEARCH_OBFUSCATE_SQL_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.elasticsearch.obfuscate_sql_values",
    env_vars: &["DD_APM_OBFUSCATION_ELASTICSEARCH_OBFUSCATE_SQL_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_HTTP_REMOVE_PATHS_WITH_DIGITS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.http.remove_paths_with_digits",
    env_vars: &["DD_APM_OBFUSCATION_HTTP_REMOVE_PATHS_WITH_DIGITS"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_HTTP_REMOVE_QUERY_STRING: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.http.remove_query_string",
    env_vars: &["DD_APM_OBFUSCATION_HTTP_REMOVE_QUERY_STRING"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_MEMCACHED_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.memcached.enabled",
    env_vars: &["DD_APM_OBFUSCATION_MEMCACHED_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_MEMCACHED_KEEP_COMMAND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.memcached.keep_command",
    env_vars: &["DD_APM_OBFUSCATION_MEMCACHED_KEEP_COMMAND"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_MONGODB_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.mongodb.enabled",
    env_vars: &["DD_APM_OBFUSCATION_MONGODB_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_MONGODB_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.mongodb.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_MONGODB_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_MONGODB_OBFUSCATE_SQL_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.mongodb.obfuscate_sql_values",
    env_vars: &["DD_APM_OBFUSCATION_MONGODB_OBFUSCATE_SQL_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_OPENSEARCH_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.opensearch.enabled",
    env_vars: &["DD_APM_OBFUSCATION_OPENSEARCH_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_OPENSEARCH_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.opensearch.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_OPENSEARCH_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_OPENSEARCH_OBFUSCATE_SQL_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.opensearch.obfuscate_sql_values",
    env_vars: &["DD_APM_OBFUSCATION_OPENSEARCH_OBFUSCATE_SQL_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_REDIS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.redis.enabled",
    env_vars: &["DD_APM_OBFUSCATION_REDIS_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_REDIS_REMOVE_ALL_ARGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.redis.remove_all_args",
    env_vars: &["DD_APM_OBFUSCATION_REDIS_REMOVE_ALL_ARGS"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_REMOVE_STACK_TRACES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.remove_stack_traces",
    env_vars: &["DD_APM_OBFUSCATION_REMOVE_STACK_TRACES"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan.enabled",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_OBFUSCATE_SQL_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan.obfuscate_sql_values",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_OBFUSCATE_SQL_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan_normalize.enabled",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan_normalize.keep_values",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_KEEP_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_OBFUSCATE_SQL_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql_exec_plan_normalize.obfuscate_sql_values",
    env_vars: &["DD_APM_OBFUSCATION_SQL_EXEC_PLAN_NORMALIZE_OBFUSCATE_SQL_VALUES"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_OBFUSCATION_VALKEY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.valkey.enabled",
    env_vars: &["DD_APM_OBFUSCATION_VALKEY_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_OBFUSCATION_VALKEY_REMOVE_ALL_ARGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.valkey.remove_all_args",
    env_vars: &["DD_APM_OBFUSCATION_VALKEY_REMOVE_ALL_ARGS"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_PEER_SERVICE_AGGREGATION: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.peer_service_aggregation",
    env_vars: &["DD_APM_PEER_SERVICE_AGGREGATION"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.peer_tags' — set an override in the annotation
pub const APM_CONFIG_PEER_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.peer_tags",
    env_vars: &["DD_APM_PEER_TAGS"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_PEER_TAGS_AGGREGATION: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.peer_tags_aggregation",
    env_vars: &["DD_APM_PEER_TAGS_AGGREGATION"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.probabilistic_sampler.enabled' — set an override in the annotation
pub const APM_CONFIG_PROBABILISTIC_SAMPLER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.probabilistic_sampler.enabled",
    env_vars: &["DD_APM_PROBABILISTIC_SAMPLER_ENABLED"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.probabilistic_sampler.hash_seed' — set an override in the annotation
pub const APM_CONFIG_PROBABILISTIC_SAMPLER_HASH_SEED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.probabilistic_sampler.hash_seed",
    env_vars: &["DD_APM_PROBABILISTIC_SAMPLER_HASH_SEED"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.probabilistic_sampler.sampling_percentage' — set an override in the annotation
pub const APM_CONFIG_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.probabilistic_sampler.sampling_percentage",
    env_vars: &["DD_APM_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.profiling_additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_PROFILING_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.profiling_additional_endpoints",
    env_vars: &["DD_APM_PROFILING_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.profiling_dd_url' — set an override in the annotation
pub const APM_CONFIG_PROFILING_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.profiling_dd_url",
    env_vars: &["DD_APM_PROFILING_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.profiling_receiver_timeout' — set an override in the annotation
pub const APM_CONFIG_PROFILING_RECEIVER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.profiling_receiver_timeout",
    env_vars: &["DD_APM_PROFILING_RECEIVER_TIMEOUT"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_RECEIVER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.receiver_enabled",
    env_vars: &["DD_APM_RECEIVER_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APM_CONFIG_RECEIVER_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.receiver_port",
    env_vars: &["DD_APM_RECEIVER_PORT", "DD_RECEIVER_PORT"],
    value_type: ValueType::Float,
    default: Some("8126"),
};

pub const APM_CONFIG_RECEIVER_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.receiver_socket",
    env_vars: &["DD_APM_RECEIVER_SOCKET"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.receiver_timeout' — set an override in the annotation
pub const APM_CONFIG_RECEIVER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.receiver_timeout",
    env_vars: &["DD_APM_RECEIVER_TIMEOUT"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.replace_tags' — set an override in the annotation
pub const APM_CONFIG_REPLACE_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.replace_tags",
    env_vars: &["DD_APM_REPLACE_TAGS"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_SEND_ALL_INTERNAL_STATS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.send_all_internal_stats",
    env_vars: &["DD_APM_SEND_ALL_INTERNAL_STATS"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_SOCKET_ACTIVATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.socket_activation.enabled",
    env_vars: &["DD_APM_SOCKET_ACTIVATION_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_SOCKET_ACTIVATION_HANDLE_TCP_PROBE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.socket_activation.handle_tcp_probe",
    env_vars: &["DD_APM_SOCKET_ACTIVATION_HANDLE_TCP_PROBE"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APM_CONFIG_SPAN_DERIVED_PRIMARY_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.span_derived_primary_tags",
    env_vars: &["DD_APM_SPAN_DERIVED_PRIMARY_TAGS"],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const APM_CONFIG_SQL_OBFUSCATION_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.sql_obfuscation_mode",
    env_vars: &["DD_APM_SQL_OBFUSCATION_MODE"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const APM_CONFIG_STATS_WRITER_CONNECTION_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.stats_writer.connection_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const APM_CONFIG_STATS_WRITER_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.stats_writer.queue_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'apm_config.symdb_additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_SYMDB_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.symdb_additional_endpoints",
    env_vars: &["DD_APM_SYMDB_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.symdb_api_key' — set an override in the annotation
pub const APM_CONFIG_SYMDB_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.symdb_api_key",
    env_vars: &["DD_APM_SYMDB_API_KEY"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.symdb_dd_url' — set an override in the annotation
pub const APM_CONFIG_SYMDB_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.symdb_dd_url",
    env_vars: &["DD_APM_SYMDB_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_SYNC_FLUSHING: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.sync_flushing",
    env_vars: &["DD_APM_SYNC_FLUSHING"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'apm_config.target_traces_per_second' — set an override in the annotation
pub const APM_CONFIG_TARGET_TRACES_PER_SECOND: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.target_traces_per_second",
    env_vars: &["DD_APM_TARGET_TPS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.telemetry.additional_endpoints' — set an override in the annotation
pub const APM_CONFIG_TELEMETRY_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.telemetry.additional_endpoints",
    env_vars: &["DD_APM_TELEMETRY_ADDITIONAL_ENDPOINTS"],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'apm_config.telemetry.dd_url' — set an override in the annotation
pub const APM_CONFIG_TELEMETRY_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.telemetry.dd_url",
    env_vars: &["DD_APM_TELEMETRY_DD_URL"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_TELEMETRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.telemetry.enabled",
    env_vars: &["DD_APM_TELEMETRY_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'apm_config.trace_buffer' — set an override in the annotation
pub const APM_CONFIG_TRACE_BUFFER: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.trace_buffer",
    env_vars: &["DD_APM_TRACE_BUFFER"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_TRACE_WRITER_CONNECTION_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.trace_writer.connection_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const APM_CONFIG_TRACE_WRITER_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.trace_writer.queue_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const APM_CONFIG_WATCHDOG_CHECK_DELAY: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.watchdog_check_delay",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const APM_CONFIG_WINDOWS_PIPE_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.windows_pipe_buffer_size",
    env_vars: &["DD_APM_WINDOWS_PIPE_BUFFER_SIZE"],
    value_type: ValueType::Float,
    default: Some("1000000"),
};

// TODO: value_type unknown for 'apm_config.windows_pipe_name' — set an override in the annotation
pub const APM_CONFIG_WINDOWS_PIPE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.windows_pipe_name",
    env_vars: &["DD_APM_WINDOWS_PIPE_NAME"],
    value_type: ValueType::String,
    default: None,
};

pub const APM_CONFIG_WINDOWS_PIPE_SECURITY_DESCRIPTOR: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.windows_pipe_security_descriptor",
    env_vars: &["DD_APM_WINDOWS_PIPE_SECURITY_DESCRIPTOR"],
    value_type: ValueType::String,
    default: Some("\"D:AI(A;;GA;;;WD)\""),
};

pub const APM_CONFIG_WORKLOAD_SELECTION: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.workload_selection",
    env_vars: &["DD_APM_WORKLOAD_SELECTION"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APP_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "app_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const APPSEC_PROXY_AUTO_DETECT: SchemaEntry = SchemaEntry {
    yaml_path: "appsec.proxy.auto_detect",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const APPSEC_PROXY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "appsec.proxy.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const APPSEC_PROXY_PROCESSOR_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "appsec.proxy.processor.address",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const APPSEC_PROXY_PROCESSOR_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "appsec.proxy.processor.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("443"),
};

pub const APPSEC_PROXY_PROXIES: SchemaEntry = SchemaEntry {
    yaml_path: "appsec.proxy.proxies",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AUTH_INIT_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "auth_init_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

pub const AUTH_TOKEN_FILE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "auth_token_file_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const AUTO_EXIT_NOPROCESS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "auto_exit.noprocess.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTO_EXIT_NOPROCESS_EXCLUDED_PROCESSES: SchemaEntry = SchemaEntry {
    yaml_path: "auto_exit.noprocess.excluded_processes",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AUTO_EXIT_VALIDATION_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "auto_exit.validation_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const AUTO_TEAM_TAG_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "auto_team_tag_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const AUTOCONF_CONFIG_FILES_POLL: SchemaEntry = SchemaEntry {
    yaml_path: "autoconf_config_files_poll",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTOCONF_CONFIG_FILES_POLL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "autoconf_config_files_poll_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const AUTOCONF_TEMPLATE_DIR: SchemaEntry = SchemaEntry {
    yaml_path: "autoconf_template_dir",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/datadog/check_configs\""),
};

pub const AUTOCONFIG_EXCLUDE_FEATURES: SchemaEntry = SchemaEntry {
    yaml_path: "autoconfig_exclude_features",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AUTOCONFIG_FROM_ENVIRONMENT: SchemaEntry = SchemaEntry {
    yaml_path: "autoconfig_from_environment",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const AUTOCONFIG_INCLUDE_FEATURES: SchemaEntry = SchemaEntry {
    yaml_path: "autoconfig_include_features",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const AUTOSCALING_CLUSTER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.cluster.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTOSCALING_FAILOVER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.failover.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTOSCALING_FAILOVER_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.failover.metrics",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const AUTOSCALING_WORKLOAD_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTOSCALING_WORKLOAD_EXTERNAL_RECOMMENDER_TLS_CA_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.external_recommender.tls.ca_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const AUTOSCALING_WORKLOAD_EXTERNAL_RECOMMENDER_TLS_CERT_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.external_recommender.tls.cert_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const AUTOSCALING_WORKLOAD_EXTERNAL_RECOMMENDER_TLS_KEY_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.external_recommender.tls.key_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const AUTOSCALING_WORKLOAD_IN_PLACE_VERTICAL_SCALING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.in_place_vertical_scaling.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const AUTOSCALING_WORKLOAD_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const AUTOSCALING_WORKLOAD_NUM_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "autoscaling.workload.num_workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const AZURE_HOSTNAME_STYLE: SchemaEntry = SchemaEntry {
    yaml_path: "azure_hostname_style",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"os\""),
};

pub const AZURE_METADATA_API_VERSION: SchemaEntry = SchemaEntry {
    yaml_path: "azure_metadata_api_version",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"2021-02-01\""),
};

pub const AZURE_METADATA_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "azure_metadata_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const BASIC_TELEMETRY_ADD_CONTAINER_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "basic_telemetry_add_container_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'bind_host' — set an override in the annotation
pub const BIND_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "bind_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const BOSH_ID: SchemaEntry = SchemaEntry {
    yaml_path: "bosh_id",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const C_CORE_DUMP: SchemaEntry = SchemaEntry {
    yaml_path: "c_core_dump",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const C_STACKTRACE_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "c_stacktrace_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'cel_workload_exclude' — set an override in the annotation
pub const CEL_WORKLOAD_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cel_workload_exclude",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const CF_OS_HOSTNAME_ALIASING: SchemaEntry = SchemaEntry {
    yaml_path: "cf_os_hostname_aliasing",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CHECK_CANCEL_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "check_cancel_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"500ms\""),
};

pub const CHECK_RUNNER_UTILIZATION_MONITOR_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "check_runner_utilization_monitor_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const CHECK_RUNNER_UTILIZATION_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "check_runner_utilization_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.95"),
};

pub const CHECK_RUNNER_UTILIZATION_WARNING_COOLDOWN: SchemaEntry = SchemaEntry {
    yaml_path: "check_runner_utilization_warning_cooldown",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10m0s\""),
};

pub const CHECK_RUNNERS: SchemaEntry = SchemaEntry {
    yaml_path: "check_runners",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4"),
};

pub const CHECK_SAMPLER_ALLOW_SKETCH_BUCKET_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "check_sampler_allow_sketch_bucket_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CHECK_SAMPLER_BUCKET_COMMITS_COUNT_EXPIRY: SchemaEntry = SchemaEntry {
    yaml_path: "check_sampler_bucket_commits_count_expiry",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CHECK_SAMPLER_CONTEXT_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "check_sampler_context_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CHECK_SAMPLER_EXPIRE_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "check_sampler_expire_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CHECK_SAMPLER_STATEFUL_METRIC_EXPIRATION_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "check_sampler_stateful_metric_expiration_time",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"25h0m0s\""),
};

pub const CHECK_SYSTEM_PROBE_STARTUP_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "check_system_probe_startup_time",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5m0s\""),
};

pub const CHECK_SYSTEM_PROBE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "check_system_probe_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const CHECK_WATCHDOG_WARNING_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "check_watchdog_warning_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"0s\""),
};

pub const CHECKS_TAG_CARDINALITY: SchemaEntry = SchemaEntry {
    yaml_path: "checks_tag_cardinality",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"low\""),
};

pub const CLC_RUNNER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLC_RUNNER_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLC_RUNNER_ID: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_id",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLC_RUNNER_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5005"),
};

pub const CLC_RUNNER_REMOTE_TAGGER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_remote_tagger_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CLC_RUNNER_SERVER_READHEADER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_server_readheader_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const CLC_RUNNER_SERVER_WRITE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "clc_runner_server_write_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15"),
};

pub const CLOUD_FOUNDRY: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLOUD_FOUNDRY_BBS_CA_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.ca_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLOUD_FOUNDRY_BBS_CERT_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.cert_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLOUD_FOUNDRY_BBS_ENV_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.env_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLOUD_FOUNDRY_BBS_ENV_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.env_include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLOUD_FOUNDRY_BBS_KEY_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.key_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLOUD_FOUNDRY_BBS_POLL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.poll_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15"),
};

pub const CLOUD_FOUNDRY_BBS_URL: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_bbs.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"https://bbs.service.cf.internal:8889\""),
};

pub const CLOUD_FOUNDRY_BUILDPACK: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_buildpack",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLOUD_FOUNDRY_CC_APPS_BATCH_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.apps_batch_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

pub const CLOUD_FOUNDRY_CC_CLIENT_ID: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.client_id",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLOUD_FOUNDRY_CC_CLIENT_SECRET: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.client_secret",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLOUD_FOUNDRY_CC_POLL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.poll_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const CLOUD_FOUNDRY_CC_SKIP_SSL_VALIDATION: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.skip_ssl_validation",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLOUD_FOUNDRY_CC_URL: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_cc.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"https://cloud-controller-ng.service.cf.internal:9024\""),
};

pub const CLOUD_FOUNDRY_CONTAINER_TAGGER_RETRY_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_container_tagger.retry_count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const CLOUD_FOUNDRY_CONTAINER_TAGGER_RETRY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_container_tagger.retry_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const CLOUD_FOUNDRY_CONTAINER_TAGGER_SHELL_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_container_tagger.shell_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/bin/sh\""),
};

pub const CLOUD_FOUNDRY_GARDEN_LISTEN_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_garden.listen_address",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/var/vcap/data/garden/garden.sock\""),
};

pub const CLOUD_FOUNDRY_GARDEN_LISTEN_NETWORK: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_foundry_garden.listen_network",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"unix\""),
};

pub const CLOUD_PROVIDER_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "cloud_provider_metadata",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const CLUSTER_AGENT_ALLOW_LEGACY_TLS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.allow_legacy_tls",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'cluster_agent.appsec.injector.annotations' — set an override in the annotation
pub const CLUSTER_AGENT_APPSEC_INJECTOR_ANNOTATIONS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.annotations",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_BASE_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.base_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"5m\""),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_ISTIO_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.istio.namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"istio-system\""),
};

// TODO: value_type unknown for 'cluster_agent.appsec.injector.labels' — set an override in the annotation
pub const CLUSTER_AGENT_APPSEC_INJECTOR_LABELS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.labels",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_MAX_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.max_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"sidecar\""),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_PROCESSOR_SERVICE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.processor.service.name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_AGENT_APPSEC_INJECTOR_PROCESSOR_SERVICE_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.appsec.injector.processor.service.namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_AGENT_AUTH_TOKEN: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.auth_token",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_AGENT_CLIENT_RECONNECT_PERIOD_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.client_reconnect_period_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1200"),
};

pub const CLUSTER_AGENT_CLUSTER_TAGGER_GRPC_MAX_MESSAGE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.cluster_tagger.grpc_max_message_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4194304"),
};

pub const CLUSTER_AGENT_CMD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.cmd_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5005"),
};

pub const CLUSTER_AGENT_COLLECT_KUBERNETES_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.collect_kubernetes_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_ISOLATION_SEGMENTS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.isolation_segments_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_KUBE_METADATA_COLLECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kube_metadata_collection.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_KUBE_METADATA_COLLECTION_RESOURCE_ANNOTATIONS_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kube_metadata_collection.resource_annotations_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLUSTER_AGENT_KUBE_METADATA_COLLECTION_RESOURCES: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kube_metadata_collection.resources",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLUSTER_AGENT_KUBERNETES_RESOURCES_COLLECTION_DEPLOYMENT_ANNOTATIONS_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kubernetes_resources_collection.deployment_annotations_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const CLUSTER_AGENT_KUBERNETES_RESOURCES_COLLECTION_POD_ANNOTATIONS_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kubernetes_resources_collection.pod_annotations_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const CLUSTER_AGENT_KUBERNETES_SERVICE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.kubernetes_service_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog-cluster-agent\""),
};

pub const CLUSTER_AGENT_LANGUAGE_DETECTION_CLEANUP_LANGUAGE_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.language_detection.cleanup.language_ttl",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"30m\""),
};

pub const CLUSTER_AGENT_LANGUAGE_DETECTION_CLEANUP_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.language_detection.cleanup.period",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10m\""),
};

pub const CLUSTER_AGENT_LANGUAGE_DETECTION_PATCHER_BASE_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.language_detection.patcher.base_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"5m\""),
};

pub const CLUSTER_AGENT_LANGUAGE_DETECTION_PATCHER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.language_detection.patcher.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CLUSTER_AGENT_LANGUAGE_DETECTION_PATCHER_MAX_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.language_detection.patcher.max_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const CLUSTER_AGENT_MAX_LEADER_CONNECTIONS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.max_leader_connections",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const CLUSTER_AGENT_MCP_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.mcp.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_MCP_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.mcp.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/mcp\""),
};

pub const CLUSTER_AGENT_REFRESH_ON_CACHE_MISS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.refresh_on_cache_miss",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CLUSTER_AGENT_SERVE_NOZZLE_DATA: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.serve_nozzle_data",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_SERVER_IDLE_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.server.idle_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const CLUSTER_AGENT_SERVER_READ_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.server.read_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CLUSTER_AGENT_SERVER_WRITE_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.server.write_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CLUSTER_AGENT_SERVICE_ACCOUNT_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.service_account_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_AGENT_SIDECARS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.sidecars_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_TAGGING_FALLBACK: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.tagging_fallback",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_TOKEN_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.token_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadogtoken\""),
};

pub const CLUSTER_AGENT_TRACING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.tracing.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_AGENT_TRACING_ENV: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.tracing.env",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_AGENT_TRACING_SAMPLE_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.tracing.sample_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.1"),
};

pub const CLUSTER_AGENT_URL: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_agent.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_CHECKS_ADVANCED_DISPATCHING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.advanced_dispatching_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CLUSTER_CHECKS_CLC_RUNNERS_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.clc_runners_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5005"),
};

pub const CLUSTER_CHECKS_CLUSTER_TAG_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.cluster_tag_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"cluster_name\""),
};

pub const CLUSTER_CHECKS_CRD_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.crd_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_CHECKS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_CHECKS_EXCLUDE_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.exclude_checks",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLUSTER_CHECKS_EXCLUDE_CHECKS_FROM_DISPATCHING: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.exclude_checks_from_dispatching",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLUSTER_CHECKS_EXTRA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.extra_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CLUSTER_CHECKS_KSM_SHARDING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.ksm_sharding_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_CHECKS_NODE_EXPIRATION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.node_expiration_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const CLUSTER_CHECKS_REBALANCE_MIN_PERCENTAGE_IMPROVEMENT: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.rebalance_min_percentage_improvement",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const CLUSTER_CHECKS_REBALANCE_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.rebalance_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10m0s\""),
};

pub const CLUSTER_CHECKS_REBALANCE_WITH_UTILIZATION: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.rebalance_with_utilization",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CLUSTER_CHECKS_SUPPORT_HYBRID_IGNORE_AD_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.support_hybrid_ignore_ad_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CLUSTER_CHECKS_UNSCHEDULED_CHECK_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.unscheduled_check_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const CLUSTER_CHECKS_WARMUP_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_checks.warmup_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const CLUSTER_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_TRUST_CHAIN_CA_CERT_FILE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_trust_chain.ca_cert_file_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_TRUST_CHAIN_CA_KEY_FILE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_trust_chain.ca_key_file_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CLUSTER_TRUST_CHAIN_ENABLE_TLS_VERIFICATION: SchemaEntry = SchemaEntry {
    yaml_path: "cluster_trust_chain.enable_tls_verification",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CMD_CHECK_FULLSKETCHES: SchemaEntry = SchemaEntry {
    yaml_path: "cmd.check.fullsketches",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CMD_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "cmd_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost\""),
};

pub const CMD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "cmd_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5001"),
};

pub const COLLECT_CCRID: SchemaEntry = SchemaEntry {
    yaml_path: "collect_ccrid",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COLLECT_EC2_INSTANCE_INFO: SchemaEntry = SchemaEntry {
    yaml_path: "collect_ec2_instance_info",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COLLECT_EC2_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "collect_ec2_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COLLECT_EC2_TAGS_USE_IMDS: SchemaEntry = SchemaEntry {
    yaml_path: "collect_ec2_tags_use_imds",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COLLECT_GCE_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "collect_gce_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COLLECT_GPU_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "collect_gpu_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COLLECT_KUBERNETES_EVENTS: SchemaEntry = SchemaEntry {
    yaml_path: "collect_kubernetes_events",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMMON_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "common_root",
    env_vars: &["DD_COMMON_ROOT"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const COMPLIANCE_CONFIG_CHECK_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.check_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"20m0s\""),
};

pub const COMPLIANCE_CONFIG_CHECK_MAX_EVENTS_PER_RUN: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.check_max_events_per_run",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const COMPLIANCE_CONFIG_CONTAINER_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.container_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const COMPLIANCE_CONFIG_CONTAINER_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.container_include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const COMPLIANCE_CONFIG_DATABASE_BENCHMARKS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.database_benchmarks.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_DIR: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.dir",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/etc/datadog-agent/compliance.d\""),
};

pub const COMPLIANCE_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'compliance_config.endpoints.additional_endpoints' — set an override in the annotation
pub const COMPLIANCE_CONFIG_ENDPOINTS_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'compliance_config.endpoints.dd_url' — set an override in the annotation
pub const COMPLIANCE_CONFIG_ENDPOINTS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'compliance_config.endpoints.logs_dd_url' — set an override in the annotation
pub const COMPLIANCE_CONFIG_ENDPOINTS_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COMPLIANCE_CONFIG_ENDPOINTS_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.endpoints.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const COMPLIANCE_CONFIG_EXCLUDE_PAUSE_CONTAINER: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.exclude_pause_container",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COMPLIANCE_CONFIG_HOST_BENCHMARKS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.host_benchmarks.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const COMPLIANCE_CONFIG_METRICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.metrics.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_OPA_METRICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.opa.metrics.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'compliance_config.run_commands_as' — set an override in the annotation
pub const COMPLIANCE_CONFIG_RUN_COMMANDS_AS: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.run_commands_as",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const COMPLIANCE_CONFIG_RUN_IN_SYSTEM_PROBE: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.run_in_system_probe",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const COMPLIANCE_CONFIG_XCCDF_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "compliance_config.xccdf.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONF_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "conf_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\".\""),
};

pub const CONFD_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "confd_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${conf_path}/conf.d\""),
};

pub const CONFIG_ID: SchemaEntry = SchemaEntry {
    yaml_path: "config_id",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'config_providers' — set an override in the annotation
pub const CONFIG_PROVIDERS: SchemaEntry = SchemaEntry {
    yaml_path: "config_providers",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const CONTAINER_CGROUP_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "container_cgroup_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/host/sys/fs/cgroup/\""),
};

// TODO: value_type unknown for 'container_env_as_tags' — set an override in the annotation
pub const CONTAINER_ENV_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "container_env_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const CONTAINER_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "container_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINER_EXCLUDE_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "container_exclude_logs",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINER_EXCLUDE_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "container_exclude_metrics",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINER_EXCLUDE_STOPPED_AGE: SchemaEntry = SchemaEntry {
    yaml_path: "container_exclude_stopped_age",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("22"),
};

// TODO: value_type unknown for 'container_image.additional_endpoints' — set an override in the annotation
pub const CONTAINER_IMAGE_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_IMAGE_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const CONTAINER_IMAGE_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const CONTAINER_IMAGE_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const CONTAINER_IMAGE_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const CONTAINER_IMAGE_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const CONTAINER_IMAGE_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const CONTAINER_IMAGE_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'container_image.dd_url' — set an override in the annotation
pub const CONTAINER_IMAGE_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_IMAGE_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_IMAGE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_IMAGE_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'container_image.logs_dd_url' — set an override in the annotation
pub const CONTAINER_IMAGE_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_IMAGE_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_IMAGE_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const CONTAINER_IMAGE_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CONTAINER_IMAGE_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const CONTAINER_IMAGE_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CONTAINER_IMAGE_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_IMAGE_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_IMAGE_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_IMAGE_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "container_image.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const CONTAINER_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "container_include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINER_INCLUDE_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "container_include_logs",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINER_INCLUDE_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "container_include_metrics",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

// TODO: value_type unknown for 'container_labels_as_tags' — set an override in the annotation
pub const CONTAINER_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "container_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'container_lifecycle.additional_endpoints' — set an override in the annotation
pub const CONTAINER_LIFECYCLE_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_LIFECYCLE_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const CONTAINER_LIFECYCLE_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const CONTAINER_LIFECYCLE_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const CONTAINER_LIFECYCLE_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const CONTAINER_LIFECYCLE_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const CONTAINER_LIFECYCLE_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const CONTAINER_LIFECYCLE_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'container_lifecycle.dd_url' — set an override in the annotation
pub const CONTAINER_LIFECYCLE_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_LIFECYCLE_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_LIFECYCLE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_LIFECYCLE_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'container_lifecycle.logs_dd_url' — set an override in the annotation
pub const CONTAINER_LIFECYCLE_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const CONTAINER_LIFECYCLE_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_LIFECYCLE_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const CONTAINER_LIFECYCLE_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CONTAINER_LIFECYCLE_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const CONTAINER_LIFECYCLE_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const CONTAINER_LIFECYCLE_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const CONTAINER_LIFECYCLE_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_LIFECYCLE_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CONTAINER_LIFECYCLE_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "container_lifecycle.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const CONTAINER_PID_MAPPER: SchemaEntry = SchemaEntry {
    yaml_path: "container_pid_mapper",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CONTAINER_PROC_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "container_proc_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/host/proc\""),
};

pub const CONTAINERD_EXCLUDE_NAMESPACES: SchemaEntry = SchemaEntry {
    yaml_path: "containerd_exclude_namespaces",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const CONTAINERD_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "containerd_namespace",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONTAINERD_NAMESPACES: SchemaEntry = SchemaEntry {
    yaml_path: "containerd_namespaces",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const CONVERT_DD_SITE_FQDN_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "convert_dd_site_fqdn.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CORE_AGENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "core_agent.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const CRI_CONNECTION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "cri_connection_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const CRI_QUERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "cri_query_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const CRI_SOCKET_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "cri_socket_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const CSI_DRIVER: SchemaEntry = SchemaEntry {
    yaml_path: "csi.driver",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"k8s.csi.datadoghq.com\""),
};

pub const CSI_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "csi.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'data_observability.forwarder.additional_endpoints' — set an override in the annotation
pub const DATA_OBSERVABILITY_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_OBSERVABILITY_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DATA_OBSERVABILITY_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const DATA_OBSERVABILITY_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const DATA_OBSERVABILITY_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const DATA_OBSERVABILITY_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const DATA_OBSERVABILITY_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const DATA_OBSERVABILITY_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'data_observability.forwarder.dd_url' — set an override in the annotation
pub const DATA_OBSERVABILITY_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_OBSERVABILITY_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_OBSERVABILITY_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'data_observability.forwarder.logs_dd_url' — set an override in the annotation
pub const DATA_OBSERVABILITY_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_OBSERVABILITY_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_OBSERVABILITY_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATA_OBSERVABILITY_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATA_OBSERVABILITY_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const DATA_OBSERVABILITY_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATA_OBSERVABILITY_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_OBSERVABILITY_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATA_OBSERVABILITY_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATA_OBSERVABILITY_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATA_OBSERVABILITY_QUERY_ACTIONS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "data_observability.query_actions.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_PLANE_DOGSTATSD_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "data_plane.dogstatsd.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_PLANE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "data_plane.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_PLANE_OTLP_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "data_plane.otlp.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_PLANE_OTLP_PROXY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "data_plane.otlp.proxy.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_PLANE_OTLP_PROXY_RECEIVER_PROTOCOLS_GRPC_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "data_plane.otlp.proxy.receiver.protocols.grpc.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"127.0.0.1:4319\""),
};

// TODO: value_type unknown for 'data_streams.forwarder.additional_endpoints' — set an override in the annotation
pub const DATA_STREAMS_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_STREAMS_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DATA_STREAMS_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const DATA_STREAMS_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const DATA_STREAMS_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.1"),
};

pub const DATA_STREAMS_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const DATA_STREAMS_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const DATA_STREAMS_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'data_streams.forwarder.dd_url' — set an override in the annotation
pub const DATA_STREAMS_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_STREAMS_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_STREAMS_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'data_streams.forwarder.logs_dd_url' — set an override in the annotation
pub const DATA_STREAMS_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATA_STREAMS_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_STREAMS_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATA_STREAMS_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATA_STREAMS_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const DATA_STREAMS_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATA_STREAMS_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATA_STREAMS_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATA_STREAMS_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATA_STREAMS_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "data_streams.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'database_monitoring.activity.additional_endpoints' — set an override in the annotation
pub const DATABASE_MONITORING_ACTIVITY_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_ACTIVITY_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DATABASE_MONITORING_ACTIVITY_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const DATABASE_MONITORING_ACTIVITY_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const DATABASE_MONITORING_ACTIVITY_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const DATABASE_MONITORING_ACTIVITY_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const DATABASE_MONITORING_ACTIVITY_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const DATABASE_MONITORING_ACTIVITY_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'database_monitoring.activity.dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_ACTIVITY_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_ACTIVITY_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_ACTIVITY_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'database_monitoring.activity.logs_dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_ACTIVITY_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_ACTIVITY_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_ACTIVITY_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATABASE_MONITORING_ACTIVITY_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_ACTIVITY_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const DATABASE_MONITORING_ACTIVITY_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_ACTIVITY_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_ACTIVITY_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_ACTIVITY_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_ACTIVITY_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.activity.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_DBM_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.dbm_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadoghq.com/dbm:true\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_DISCOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.discovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_GLOBAL_VIEW_DB_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.global_view_db_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadoghq.com/global_view_db\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_QUERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.query_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_AURORA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.aurora.tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_DBM_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.dbm_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadoghq.com/dbm:true\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_DISCOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.discovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_GLOBAL_VIEW_DB_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.global_view_db_tag",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadoghq.com/global_view_db\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_QUERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.query_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DATABASE_MONITORING_AUTODISCOVERY_RDS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.autodiscovery.rds.tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

// TODO: value_type unknown for 'database_monitoring.metrics.additional_endpoints' — set an override in the annotation
pub const DATABASE_MONITORING_METRICS_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_METRICS_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DATABASE_MONITORING_METRICS_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const DATABASE_MONITORING_METRICS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const DATABASE_MONITORING_METRICS_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const DATABASE_MONITORING_METRICS_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const DATABASE_MONITORING_METRICS_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const DATABASE_MONITORING_METRICS_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'database_monitoring.metrics.dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_METRICS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_METRICS_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_METRICS_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'database_monitoring.metrics.logs_dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_METRICS_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_METRICS_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_METRICS_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATABASE_MONITORING_METRICS_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_METRICS_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const DATABASE_MONITORING_METRICS_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_METRICS_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_METRICS_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_METRICS_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_METRICS_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.metrics.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'database_monitoring.samples.additional_endpoints' — set an override in the annotation
pub const DATABASE_MONITORING_SAMPLES_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_SAMPLES_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DATABASE_MONITORING_SAMPLES_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const DATABASE_MONITORING_SAMPLES_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const DATABASE_MONITORING_SAMPLES_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const DATABASE_MONITORING_SAMPLES_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const DATABASE_MONITORING_SAMPLES_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const DATABASE_MONITORING_SAMPLES_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'database_monitoring.samples.dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_SAMPLES_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_SAMPLES_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_SAMPLES_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'database_monitoring.samples.logs_dd_url' — set an override in the annotation
pub const DATABASE_MONITORING_SAMPLES_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DATABASE_MONITORING_SAMPLES_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_SAMPLES_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DATABASE_MONITORING_SAMPLES_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_SAMPLES_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const DATABASE_MONITORING_SAMPLES_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DATABASE_MONITORING_SAMPLES_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DATABASE_MONITORING_SAMPLES_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_SAMPLES_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DATABASE_MONITORING_SAMPLES_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "database_monitoring.samples.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'dd_url' — set an override in the annotation
pub const DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "dd_url",
    env_vars: &["DD_DD_URL", "DD_URL"],
    value_type: ValueType::String,
    default: None,
};

pub const DEFAULT_INTEGRATION_HTTP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "default_integration_http_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("9"),
};

pub const DELEGATED_AUTH_AWS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "delegated_auth.aws.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DELEGATED_AUTH_ORG_UUID: SchemaEntry = SchemaEntry {
    yaml_path: "delegated_auth.org_uuid",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DELEGATED_AUTH_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "delegated_auth.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DELEGATED_AUTH_REFRESH_INTERVAL_MINS: SchemaEntry = SchemaEntry {
    yaml_path: "delegated_auth.refresh_interval_mins",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const DISABLE_CLUSTER_NAME_TAG_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "disable_cluster_name_tag_key",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DISABLE_FILE_LOGGING: SchemaEntry = SchemaEntry {
    yaml_path: "disable_file_logging",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DISABLE_PY3_VALIDATION: SchemaEntry = SchemaEntry {
    yaml_path: "disable_py3_validation",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DISABLE_UNSAFE_YAML: SchemaEntry = SchemaEntry {
    yaml_path: "disable_unsafe_yaml",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DISK_CHECK_USE_CORE_LOADER: SchemaEntry = SchemaEntry {
    yaml_path: "disk_check.use_core_loader",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DJM_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "djm_config.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'docker_env_as_tags' — set an override in the annotation
pub const DOCKER_ENV_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "docker_env_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'docker_labels_as_tags' — set an override in the annotation
pub const DOCKER_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "docker_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const DOCKER_QUERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "docker_query_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const DOGSTATSD_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8192"),
};

pub const DOGSTATSD_CAPTURE_DEPTH: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_capture_depth",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DOGSTATSD_CAPTURE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_capture_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DOGSTATSD_CONTEXT_EXPIRY_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_context_expiry_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("20"),
};

pub const DOGSTATSD_DISABLE_VERBOSE_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_disable_verbose_logs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_ENTITY_ID_PRECEDENCE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_entity_id_precedence",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_EOL_REQUIRED: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_eol_required",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const DOGSTATSD_EXPERIMENTAL_HTTP_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_experimental_http.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_EXPERIMENTAL_HTTP_LISTEN_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_experimental_http.listen_address",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"127.0.0.1:8125\""),
};

pub const DOGSTATSD_EXPIRY_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_expiry_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const DOGSTATSD_FLUSH_INCOMPLETE_BUCKETS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_flush_incomplete_buckets",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_HOST_SOCKET_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_host_socket_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/var/run/datadog\""),
};

pub const DOGSTATSD_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DOGSTATSD_LOG_FILE_MAX_ROLLS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_log_file_max_rolls",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const DOGSTATSD_LOG_FILE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_log_file_max_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10Mb\""),
};

pub const DOGSTATSD_LOGGING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_logging_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DOGSTATSD_MAPPER_CACHE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mapper_cache_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

// TODO: value_type unknown for 'dogstatsd_mapper_profiles' — set an override in the annotation
pub const DOGSTATSD_MAPPER_PROFILES: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mapper_profiles",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_GO_GC: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.go_gc",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_HIGH_SOFT_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.high_soft_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.8"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_LOW_SOFT_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.low_soft_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.7"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_MEMORY_BALLAST: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.memory_ballast",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8589934592"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_RATE_CHECK_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.rate_check.factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_RATE_CHECK_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.rate_check.max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_RATE_CHECK_MIN: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.rate_check.min",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.01"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_SOFT_LIMIT_FREEOS_CHECK_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.soft_limit_freeos_check.factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1.5"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_SOFT_LIMIT_FREEOS_CHECK_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.soft_limit_freeos_check.max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.1"),
};

pub const DOGSTATSD_MEM_BASED_RATE_LIMITER_SOFT_LIMIT_FREEOS_CHECK_MIN: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mem_based_rate_limiter.soft_limit_freeos_check.min",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.01"),
};

pub const DOGSTATSD_METRICS_STATS_ENABLE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_metrics_stats_enable",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_NO_AGGREGATION_PIPELINE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_no_aggregation_pipeline",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DOGSTATSD_NO_AGGREGATION_PIPELINE_BATCH_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_no_aggregation_pipeline_batch_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2048"),
};

pub const DOGSTATSD_NON_LOCAL_TRAFFIC: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_non_local_traffic",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_ORIGIN_DETECTION: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_origin_detection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_ORIGIN_DETECTION_CLIENT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_origin_detection_client",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_ORIGIN_OPTOUT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_origin_optout_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const DOGSTATSD_PACKET_BUFFER_FLUSH_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_packet_buffer_flush_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"100ms\""),
};

pub const DOGSTATSD_PACKET_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_packet_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("32"),
};

pub const DOGSTATSD_PIPE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_pipe_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DOGSTATSD_PIPELINE_AUTOADJUST: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_pipeline_autoadjust",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_PIPELINE_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_pipeline_count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const DOGSTATSD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8125"),
};

pub const DOGSTATSD_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_queue_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1024"),
};

pub const DOGSTATSD_SO_RCVBUF: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_so_rcvbuf",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const DOGSTATSD_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const DOGSTATSD_STATS_BUFFER: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_stats_buffer",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const DOGSTATSD_STATS_ENABLE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_stats_enable",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_STATS_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_stats_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

pub const DOGSTATSD_STREAM_LOG_TOO_BIG: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_stream_log_too_big",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_STREAM_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_stream_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const DOGSTATSD_STRING_INTERNER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_string_interner_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4096"),
};

pub const DOGSTATSD_TAG_CARDINALITY: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_tag_cardinality",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"low\""),
};

pub const DOGSTATSD_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const DOGSTATSD_TELEMETRY_ENABLED_LISTENER_ID: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_telemetry_enabled_listener_id",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const DOGSTATSD_WINDOWS_PIPE_SECURITY_DESCRIPTOR: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_windows_pipe_security_descriptor",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"D:AI(A;;GA;;;WD)\""),
};

pub const DOGSTATSD_WORKERS_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_workers_count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const EC2_IMDSV2_TRANSITION_PAYLOAD_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_imdsv2_transition_payload_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EC2_METADATA_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_metadata_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const EC2_METADATA_TOKEN_LIFETIME: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_metadata_token_lifetime",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("21600"),
};

pub const EC2_PREFER_IMDSV2: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_prefer_imdsv2",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EC2_PRIORITIZE_INSTANCE_ID_AS_HOSTNAME: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_prioritize_instance_id_as_hostname",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EC2_USE_DMI: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_use_dmi",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EC2_USE_WINDOWS_PREFIX_DETECTION: SchemaEntry = SchemaEntry {
    yaml_path: "ec2_use_windows_prefix_detection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ECS_AGENT_CONTAINER_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_agent_container_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"ecs-agent\""),
};

pub const ECS_AGENT_URL: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_agent_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ECS_COLLECT_RESOURCE_TAGS_EC2: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_collect_resource_tags_ec2",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ECS_DEPLOYMENT_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_deployment_mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"auto\""),
};

pub const ECS_METADATA_RETRY_INITIAL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_metadata_retry_initial_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"100ms\""),
};

pub const ECS_METADATA_RETRY_MAX_ELAPSED_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_metadata_retry_max_elapsed_time",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"3s\""),
};

pub const ECS_METADATA_RETRY_TIMEOUT_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_metadata_retry_timeout_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const ECS_METADATA_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_metadata_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const ECS_RESOURCE_TAGS_REPLACE_COLON: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_resource_tags_replace_colon",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ECS_TASK_CACHE_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_task_cache_ttl",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"3m0s\""),
};

pub const ECS_TASK_COLLECTION_BURST: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_task_collection_burst",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const ECS_TASK_COLLECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_task_collection_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ECS_TASK_COLLECTION_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "ecs_task_collection_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("35"),
};

pub const EKS_FARGATE: SchemaEntry = SchemaEntry {
    yaml_path: "eks_fargate",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ENABLE_CLUSTER_AGENT_METADATA_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "enable_cluster_agent_metadata_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_GOHAI: SchemaEntry = SchemaEntry {
    yaml_path: "enable_gohai",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_JSON_STREAM_SHARED_COMPRESSOR_BUFFERS: SchemaEntry = SchemaEntry {
    yaml_path: "enable_json_stream_shared_compressor_buffers",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_METADATA_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "enable_metadata_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_PAYLOADS_EVENTS: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads.events",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_PAYLOADS_JSON_TO_V1_INTAKE: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads.json_to_v1_intake",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_PAYLOADS_SERIES: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads.series",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_PAYLOADS_SERVICE_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads.service_checks",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_PAYLOADS_SKETCHES: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads.sketches",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLE_SIGNING_METADATA_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "enable_signing_metadata_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENABLED_RFC1123_COMPLIANT_CLUSTER_NAME_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "enabled_rfc1123_compliant_cluster_name_tag",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENHANCED_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "enhanced_metrics",
    env_vars: &["DD_ENHANCED_METRICS_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ENTITY_ID: SchemaEntry = SchemaEntry {
    yaml_path: "entity_id",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const ENV: SchemaEntry = SchemaEntry {
    yaml_path: "env",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'event_management.forwarder.additional_endpoints' — set an override in the annotation
pub const EVENT_MANAGEMENT_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const EVENT_MANAGEMENT_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const EVENT_MANAGEMENT_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const EVENT_MANAGEMENT_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const EVENT_MANAGEMENT_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const EVENT_MANAGEMENT_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const EVENT_MANAGEMENT_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const EVENT_MANAGEMENT_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'event_management.forwarder.dd_url' — set an override in the annotation
pub const EVENT_MANAGEMENT_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const EVENT_MANAGEMENT_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EVENT_MANAGEMENT_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'event_management.forwarder.logs_dd_url' — set an override in the annotation
pub const EVENT_MANAGEMENT_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const EVENT_MANAGEMENT_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EVENT_MANAGEMENT_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const EVENT_MANAGEMENT_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const EVENT_MANAGEMENT_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const EVENT_MANAGEMENT_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const EVENT_MANAGEMENT_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EVENT_MANAGEMENT_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EVENT_MANAGEMENT_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EVENT_MANAGEMENT_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "event_management.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'evp_proxy_config.additional_endpoints' — set an override in the annotation
pub const EVP_PROXY_CONFIG_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'evp_proxy_config.api_key' — set an override in the annotation
pub const EVP_PROXY_CONFIG_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'evp_proxy_config.dd_url' — set an override in the annotation
pub const EVP_PROXY_CONFIG_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const EVP_PROXY_CONFIG_DELEGATED_AUTH_AWS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.delegated_auth.aws.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const EVP_PROXY_CONFIG_DELEGATED_AUTH_ORG_UUID: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.delegated_auth.org_uuid",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const EVP_PROXY_CONFIG_DELEGATED_AUTH_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.delegated_auth.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const EVP_PROXY_CONFIG_DELEGATED_AUTH_REFRESH_INTERVAL_MINS: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.delegated_auth.refresh_interval_mins",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

// TODO: value_type unknown for 'evp_proxy_config.enabled' — set an override in the annotation
pub const EVP_PROXY_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.enabled",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'evp_proxy_config.max_payload_size' — set an override in the annotation
pub const EVP_PROXY_CONFIG_MAX_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.max_payload_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'evp_proxy_config.receiver_timeout' — set an override in the annotation
pub const EVP_PROXY_CONFIG_RECEIVER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "evp_proxy_config.receiver_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const EXCLUDE_EC2_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "exclude_ec2_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const EXCLUDE_GCE_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "exclude_gce_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const EXCLUDE_PAUSE_CONTAINER: SchemaEntry = SchemaEntry {
    yaml_path: "exclude_pause_container",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EXPECTED_TAGS_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "expected_tags_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"0s\""),
};

pub const EXPVAR_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "expvar_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

pub const EXTERNAL_METRICS_AGGREGATOR: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics.aggregator",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"avg\""),
};

pub const EXTERNAL_METRICS_PROVIDER_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const EXTERNAL_METRICS_PROVIDER_APP_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.app_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const EXTERNAL_METRICS_PROVIDER_BATCH_WINDOW: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.batch_window",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const EXTERNAL_METRICS_PROVIDER_BUCKET_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.bucket_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const EXTERNAL_METRICS_PROVIDER_CHUNK_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.chunk_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("35"),
};

// TODO: value_type unknown for 'external_metrics_provider.config' — set an override in the annotation
pub const EXTERNAL_METRICS_PROVIDER_CONFIG: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.config",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const EXTERNAL_METRICS_PROVIDER_ENABLE_DATADOGMETRIC_AUTOGEN: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.enable_datadogmetric_autogen",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const EXTERNAL_METRICS_PROVIDER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EXTERNAL_METRICS_PROVIDER_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'external_metrics_provider.endpoints' — set an override in the annotation
pub const EXTERNAL_METRICS_PROVIDER_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const EXTERNAL_METRICS_PROVIDER_LOCAL_COPY_REFRESH_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.local_copy_refresh_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const EXTERNAL_METRICS_PROVIDER_MAX_AGE: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.max_age",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const EXTERNAL_METRICS_PROVIDER_MAX_PARALLEL_QUERIES: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.max_parallel_queries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const EXTERNAL_METRICS_PROVIDER_MAX_TIME_WINDOW: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.max_time_window",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("86400"),
};

pub const EXTERNAL_METRICS_PROVIDER_NUM_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.num_workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const EXTERNAL_METRICS_PROVIDER_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8443"),
};

pub const EXTERNAL_METRICS_PROVIDER_QUERY_VALIDITY_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.query_validity_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const EXTERNAL_METRICS_PROVIDER_REFRESH_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.refresh_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const EXTERNAL_METRICS_PROVIDER_ROLLUP: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.rollup",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const EXTERNAL_METRICS_PROVIDER_SPLIT_BATCHES_WITH_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.split_batches_with_backoff",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EXTERNAL_METRICS_PROVIDER_USE_DATADOGMETRIC_CRD: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.use_datadogmetric_crd",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EXTERNAL_METRICS_PROVIDER_WPA_CONTROLLER: SchemaEntry = SchemaEntry {
    yaml_path: "external_metrics_provider.wpa_controller",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const EXTRA_CONFIG_PROVIDERS: SchemaEntry = SchemaEntry {
    yaml_path: "extra_config_providers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const EXTRA_LISTENERS: SchemaEntry = SchemaEntry {
    yaml_path: "extra_listeners",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const EXTRA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "extra_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const FIPS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "fips.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const FIPS_HTTPS: SchemaEntry = SchemaEntry {
    yaml_path: "fips.https",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const FIPS_LOCAL_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "fips.local_address",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost\""),
};

pub const FIPS_PORT_RANGE_START: SchemaEntry = SchemaEntry {
    yaml_path: "fips.port_range_start",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("9803"),
};

pub const FIPS_TLS_VERIFY: SchemaEntry = SchemaEntry {
    yaml_path: "fips.tls_verify",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const FLARE_PROFILE_OVERHEAD_RUNTIME: SchemaEntry = SchemaEntry {
    yaml_path: "flare.profile_overhead_runtime",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10s\""),
};

pub const FLARE_RC_PROFILING_BLOCKING_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "flare.rc_profiling.blocking_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const FLARE_RC_PROFILING_MUTEX_FRACTION: SchemaEntry = SchemaEntry {
    yaml_path: "flare.rc_profiling.mutex_fraction",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const FLARE_RC_PROFILING_PROFILE_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "flare.rc_profiling.profile_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

pub const FLARE_RC_STREAMLOGS_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "flare.rc_streamlogs.duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const FLARE_PROVIDER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "flare_provider_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10s\""),
};

pub const FLARE_STRIPPED_KEYS: SchemaEntry = SchemaEntry {
    yaml_path: "flare_stripped_keys",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const FLEET_LAYERS: SchemaEntry = SchemaEntry {
    yaml_path: "fleet_layers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

// TODO: value_type unknown for 'fleet_policies_dir' — set an override in the annotation
pub const FLEET_POLICIES_DIR: SchemaEntry = SchemaEntry {
    yaml_path: "fleet_policies_dir",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const FORWARDER_APIKEY_VALIDATION_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_apikey_validation_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const FORWARDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const FORWARDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const FORWARDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("64"),
};

pub const FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const FORWARDER_FLUSH_TO_DISK_MEM_RATIO: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_flush_to_disk_mem_ratio",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.5"),
};

pub const FORWARDER_HIGH_PRIO_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_high_prio_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const FORWARDER_HTTP_PROTOCOL: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_http_protocol",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"auto\""),
};

pub const FORWARDER_LOW_PRIO_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_low_prio_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const FORWARDER_MAX_CONCURRENT_REQUESTS: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_max_concurrent_requests",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const FORWARDER_NUM_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_num_workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const FORWARDER_OUTDATED_FILE_IN_DAYS: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_outdated_file_in_days",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const FORWARDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const FORWARDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const FORWARDER_REQUEUE_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_requeue_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const FORWARDER_RETRY_QUEUE_CAPACITY_TIME_INTERVAL_SEC: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_retry_queue_capacity_time_interval_sec",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("900"),
};

pub const FORWARDER_RETRY_QUEUE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_retry_queue_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_retry_queue_payloads_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15728640"),
};

pub const FORWARDER_STOP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_stop_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const FORWARDER_STORAGE_MAX_DISK_RATIO: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_storage_max_disk_ratio",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.8"),
};

pub const FORWARDER_STORAGE_MAX_SIZE_IN_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_storage_max_size_in_bytes",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const FORWARDER_STORAGE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_storage_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const FORWARDER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "forwarder_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("20"),
};

pub const GCE_METADATA_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "gce_metadata_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const GCE_SEND_PROJECT_ID_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "gce_send_project_id_tag",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const GO_CORE_DUMP: SchemaEntry = SchemaEntry {
    yaml_path: "go_core_dump",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const GPU_DISABLED_COLLECTORS: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.disabled_collectors",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const GPU_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const GPU_INTEGRATE_WITH_WORKLOADMETA_PROCESSES: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.integrate_with_workloadmeta_processes",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const GPU_NVML_LIB_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.nvml_lib_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const GPU_SP_PROCESS_METRICS_REQUEST_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.sp_process_metrics_request_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"3s\""),
};

pub const GPU_USE_SP_PROCESS_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.use_sp_process_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const GPU_WORKLOAD_TAG_CACHE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "gpu.workload_tag_cache_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1024"),
};

pub const HA_AGENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "ha_agent.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'ha_agent.group' — set an override in the annotation
pub const HA_AGENT_GROUP: SchemaEntry = SchemaEntry {
    yaml_path: "ha_agent.group",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const HEALTH_PLATFORM_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "health_platform.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HEALTH_PLATFORM_FORWARDER_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "health_platform.forwarder.interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const HEALTH_PLATFORM_PERSIST_ON_KUBERNETES: SchemaEntry = SchemaEntry {
    yaml_path: "health_platform.persist_on_kubernetes",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HEALTH_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "health_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const HEROKU_DYNO: SchemaEntry = SchemaEntry {
    yaml_path: "heroku_dyno",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HISTOGRAM_AGGREGATES: SchemaEntry = SchemaEntry {
    yaml_path: "histogram_aggregates",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const HISTOGRAM_COPY_TO_DISTRIBUTION: SchemaEntry = SchemaEntry {
    yaml_path: "histogram_copy_to_distribution",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HISTOGRAM_COPY_TO_DISTRIBUTION_PREFIX: SchemaEntry = SchemaEntry {
    yaml_path: "histogram_copy_to_distribution_prefix",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const HISTOGRAM_PERCENTILES: SchemaEntry = SchemaEntry {
    yaml_path: "histogram_percentiles",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const HOST_ALIASES: SchemaEntry = SchemaEntry {
    yaml_path: "host_aliases",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const HOSTNAME: SchemaEntry = SchemaEntry {
    yaml_path: "hostname",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const HOSTNAME_DRIFT_INITIAL_DELAY: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_drift_initial_delay",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"20m0s\""),
};

pub const HOSTNAME_DRIFT_RECURRING_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_drift_recurring_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"6h0m0s\""),
};

pub const HOSTNAME_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const HOSTNAME_FORCE_CONFIG_AS_CANONICAL: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_force_config_as_canonical",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HOSTNAME_FQDN: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_fqdn",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HOSTNAME_TRUST_UTS_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "hostname_trust_uts_namespace",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'hostprofiler.additional_http_headers' — set an override in the annotation
pub const HOSTPROFILER_ADDITIONAL_HTTP_HEADERS: SchemaEntry = SchemaEntry {
    yaml_path: "hostprofiler.additional_http_headers",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const HOSTPROFILER_DDPROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "hostprofiler.ddprofiling.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const HOSTPROFILER_DDPROFILING_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "hostprofiler.ddprofiling.period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const HOSTPROFILER_DEBUG_VERBOSITY: SchemaEntry = SchemaEntry {
    yaml_path: "hostprofiler.debug.verbosity",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const HPA_CONFIGMAP_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "hpa_configmap_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog-custom-metrics\""),
};

pub const HPA_WATCHER_GC_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "hpa_watcher_gc_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const HPA_WATCHER_POLLING_FREQ: SchemaEntry = SchemaEntry {
    yaml_path: "hpa_watcher_polling_freq",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

// TODO: value_type unknown for 'http_dial_fallback_delay' — set an override in the annotation
pub const HTTP_DIAL_FALLBACK_DELAY: SchemaEntry = SchemaEntry {
    yaml_path: "http_dial_fallback_delay",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const IBM_METADATA_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "ibm_metadata_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const IGNORE_AUTOCONF: SchemaEntry = SchemaEntry {
    yaml_path: "ignore_autoconf",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const IGNORE_HOST_ETC: SchemaEntry = SchemaEntry {
    yaml_path: "ignore_host_etc",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INCLUDE_EPHEMERAL_CONTAINERS: SchemaEntry = SchemaEntry {
    yaml_path: "include_ephemeral_containers",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INFRASTRUCTURE_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "infrastructure_mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"full\""),
};

pub const INSTALLER_GC_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "installer.gc_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1h0m0s\""),
};

pub const INSTALLER_MIRROR: SchemaEntry = SchemaEntry {
    yaml_path: "installer.mirror",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INSTALLER_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "installer.refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

pub const INSTALLER_REGISTRY_AUTH: SchemaEntry = SchemaEntry {
    yaml_path: "installer.registry.auth",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INSTALLER_REGISTRY_PASSWORD: SchemaEntry = SchemaEntry {
    yaml_path: "installer.registry.password",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INSTALLER_REGISTRY_URL: SchemaEntry = SchemaEntry {
    yaml_path: "installer.registry.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INSTALLER_REGISTRY_USERNAME: SchemaEntry = SchemaEntry {
    yaml_path: "installer.registry.username",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INTEGRATION_ADDITIONAL: SchemaEntry = SchemaEntry {
    yaml_path: "integration.additional",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_BASIC_ALLOWED: SchemaEntry = SchemaEntry {
    yaml_path: "integration.basic.allowed",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const INTEGRATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "integration.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INTEGRATION_END_USER_DEVICE_ALLOWED: SchemaEntry = SchemaEntry {
    yaml_path: "integration.end_user_device.allowed",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_EXCLUDED: SchemaEntry = SchemaEntry {
    yaml_path: "integration.excluded",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_FULL_ALLOWED: SchemaEntry = SchemaEntry {
    yaml_path: "integration.full.allowed",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_CHECK_STATUS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "integration_check_status_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTEGRATION_FILE_PATHS_ALLOWLIST: SchemaEntry = SchemaEntry {
    yaml_path: "integration_file_paths_allowlist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_IGNORE_UNTRUSTED_FILE_PARAMS: SchemaEntry = SchemaEntry {
    yaml_path: "integration_ignore_untrusted_file_params",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTEGRATION_PROFILING: SchemaEntry = SchemaEntry {
    yaml_path: "integration_profiling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTEGRATION_SECURITY_EXCLUDED_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "integration_security_excluded_checks",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTEGRATION_TRACING: SchemaEntry = SchemaEntry {
    yaml_path: "integration_tracing",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTEGRATION_TRACING_EXHAUSTIVE: SchemaEntry = SchemaEntry {
    yaml_path: "integration_tracing_exhaustive",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTEGRATION_TRUSTED_PROVIDERS: SchemaEntry = SchemaEntry {
    yaml_path: "integration_trusted_providers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const INTERNAL_PROFILING_BLOCK_PROFILE_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.block_profile_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const INTERNAL_PROFILING_CAPTURE_ALL_ALLOCATIONS: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.capture_all_allocations",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTERNAL_PROFILING_CPU_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.cpu_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const INTERNAL_PROFILING_CUSTOM_ATTRIBUTES: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.custom_attributes",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const INTERNAL_PROFILING_DELTA_PROFILES: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.delta_profiles",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INTERNAL_PROFILING_ENABLE_BLOCK_PROFILING: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.enable_block_profiling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTERNAL_PROFILING_ENABLE_GOROUTINE_STACKTRACES: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.enable_goroutine_stacktraces",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTERNAL_PROFILING_ENABLE_MUTEX_PROFILING: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.enable_mutex_profiling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTERNAL_PROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const INTERNAL_PROFILING_EXTRA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.extra_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const INTERNAL_PROFILING_MUTEX_PROFILE_FRACTION: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.mutex_profile_fraction",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const INTERNAL_PROFILING_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5m0s\""),
};

// TODO: value_type unknown for 'internal_profiling.profile_dd_url' — set an override in the annotation
pub const INTERNAL_PROFILING_PROFILE_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.profile_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const INTERNAL_PROFILING_UNIX_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "internal_profiling.unix_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const INVENTORIES_CHECKS_CONFIGURATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_checks_configuration_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INVENTORIES_COLLECT_CLOUD_PROVIDER_ACCOUNT_ID: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_collect_cloud_provider_account_id",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INVENTORIES_CONFIGURATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_configuration_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INVENTORIES_DIAGNOSTICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_diagnostics_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INVENTORIES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const INVENTORIES_FIRST_RUN_DELAY: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_first_run_delay",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const INVENTORIES_MAX_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_max_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const INVENTORIES_MIN_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "inventories_min_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const IOT_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "iot_host",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'ipc_address' — set an override in the annotation
pub const IPC_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "ipc_address",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const IPC_CERT_FILE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "ipc_cert_file_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const JMX_CHECK_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_check_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15000"),
};

pub const JMX_COLLECTION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_collection_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const JMX_CUSTOM_JARS: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_custom_jars",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const JMX_JAVA_TOOL_OPTIONS: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_java_tool_options",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const JMX_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const JMX_MAX_RAM_PERCENTAGE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_max_ram_percentage",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("25"),
};

pub const JMX_MAX_RESTARTS: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_max_restarts",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const JMX_RECONNECTION_THREAD_POOL_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_reconnection_thread_pool_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const JMX_RECONNECTION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_reconnection_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const JMX_RESTART_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_restart_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const JMX_STATSD_CLIENT_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_statsd_client_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const JMX_STATSD_CLIENT_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_statsd_client_queue_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4096"),
};

pub const JMX_STATSD_CLIENT_SOCKET_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_statsd_client_socket_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const JMX_STATSD_CLIENT_USE_NON_BLOCKING: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_statsd_client_use_non_blocking",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const JMX_STATSD_TELEMETRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_statsd_telemetry_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const JMX_TELEMETRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_telemetry_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const JMX_THREAD_POOL_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_thread_pool_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const JMX_USE_CGROUP_MEMORY_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_use_cgroup_memory_limit",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const JMX_USE_CONTAINER_SUPPORT: SchemaEntry = SchemaEntry {
    yaml_path: "jmx_use_container_support",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBE_CACHE_SYNC_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "kube_cache_sync_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const KUBE_RESOURCES_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "kube_resources_namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBEACTIONS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "kubeactions.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBELET_AUTH_TOKEN_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_auth_token_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBELET_CACHE_PODS_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_cache_pods_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const KUBELET_CLIENT_CA: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_client_ca",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBELET_CLIENT_CRT: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_client_crt",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBELET_CLIENT_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_client_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBELET_COLLECTOR_PULL_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_collector_pull_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const KUBELET_CORE_CHECK_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_core_check_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const KUBELET_TLS_VERIFY: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_tls_verify",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const KUBELET_USE_API_SERVER: SchemaEntry = SchemaEntry {
    yaml_path: "kubelet_use_api_server",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_AD_TAGS_DISABLED: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_ad_tags_disabled",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const KUBERNETES_APISERVER_CA_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_apiserver_ca_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBERNETES_APISERVER_CLIENT_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_apiserver_client_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const KUBERNETES_APISERVER_INFORMER_CLIENT_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_apiserver_informer_client_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const KUBERNETES_APISERVER_TLS_VERIFY: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_apiserver_tls_verify",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const KUBERNETES_APISERVER_USE_PROTOBUF: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_apiserver_use_protobuf",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_COLLECT_METADATA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_collect_metadata_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const KUBERNETES_EVENT_COLLECTION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_event_collection_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const KUBERNETES_EVENTS_SOURCE_DETECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_events_source_detection.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_HTTP_KUBELET_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_http_kubelet_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10255"),
};

pub const KUBERNETES_HTTPS_KUBELET_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_https_kubelet_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10250"),
};

pub const KUBERNETES_INFORMERS_RESYNC_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_informers_resync_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("300"),
};

pub const KUBERNETES_KUBE_SERVICE_IGNORE_READINESS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kube_service_ignore_readiness",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_KUBECONFIG_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubeconfig_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBERNETES_KUBELET_DEVICEPLUGINS_CACHE_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubelet_deviceplugins_cache_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5s\""),
};

pub const KUBERNETES_KUBELET_DEVICEPLUGINS_SOCKETDIR: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubelet_deviceplugins_socketdir",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const KUBERNETES_KUBELET_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubelet_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBERNETES_KUBELET_NODENAME: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubelet_nodename",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const KUBERNETES_KUBELET_PODRESOURCES_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_kubelet_podresources_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const KUBERNETES_MAP_SERVICES_ON_IP: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_map_services_on_ip",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_METADATA_STREAMING: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_metadata_streaming",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const KUBERNETES_METADATA_TAG_UPDATE_FREQ: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_metadata_tag_update_freq",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

// TODO: value_type unknown for 'kubernetes_namespace_annotations_as_tags' — set an override in the annotation
pub const KUBERNETES_NAMESPACE_ANNOTATIONS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_namespace_annotations_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'kubernetes_namespace_labels_as_tags' — set an override in the annotation
pub const KUBERNETES_NAMESPACE_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_namespace_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const KUBERNETES_NODE_ANNOTATIONS_AS_HOST_ALIASES: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_node_annotations_as_host_aliases",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

// TODO: value_type unknown for 'kubernetes_node_annotations_as_tags' — set an override in the annotation
pub const KUBERNETES_NODE_ANNOTATIONS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_node_annotations_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const KUBERNETES_NODE_LABEL_AS_CLUSTER_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_node_label_as_cluster_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'kubernetes_node_labels_as_tags' — set an override in the annotation
pub const KUBERNETES_NODE_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_node_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const KUBERNETES_PERSISTENT_VOLUME_CLAIMS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_persistent_volume_claims_as_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'kubernetes_pod_annotations_as_tags' — set an override in the annotation
pub const KUBERNETES_POD_ANNOTATIONS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_pod_annotations_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const KUBERNETES_POD_EXPIRATION_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_pod_expiration_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("900"),
};

// TODO: value_type unknown for 'kubernetes_pod_labels_as_tags' — set an override in the annotation
pub const KUBERNETES_POD_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_pod_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const KUBERNETES_RESOURCES_ANNOTATIONS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_resources_annotations_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"{}\""),
};

pub const KUBERNETES_RESOURCES_LABELS_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_resources_labels_as_tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"{}\""),
};

pub const KUBERNETES_USE_ENDPOINT_SLICES: SchemaEntry = SchemaEntry {
    yaml_path: "kubernetes_use_endpoint_slices",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LANGUAGE_DETECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "language_detection.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LANGUAGE_DETECTION_REPORTING_BUFFER_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "language_detection.reporting.buffer_period",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10s\""),
};

pub const LANGUAGE_DETECTION_REPORTING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "language_detection.reporting.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LANGUAGE_DETECTION_REPORTING_REFRESH_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "language_detection.reporting.refresh_period",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"20m\""),
};

pub const LEADER_ELECTION: SchemaEntry = SchemaEntry {
    yaml_path: "leader_election",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LEADER_ELECTION_DEFAULT_RESOURCE: SchemaEntry = SchemaEntry {
    yaml_path: "leader_election_default_resource",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"configmap\""),
};

pub const LEADER_ELECTION_RELEASE_ON_SHUTDOWN: SchemaEntry = SchemaEntry {
    yaml_path: "leader_election_release_on_shutdown",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LEADER_LEASE_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "leader_lease_duration",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"60\""),
};

pub const LEADER_LEASE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "leader_lease_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"datadog-leader-election\""),
};

// TODO: value_type unknown for 'listeners' — set an override in the annotation
pub const LISTENERS: SchemaEntry = SchemaEntry {
    yaml_path: "listeners",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const LOG_ALL_GOROUTINES_WHEN_UNHEALTHY: SchemaEntry = SchemaEntry {
    yaml_path: "log_all_goroutines_when_unhealthy",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "log_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOG_FILE_MAX_ROLLS: SchemaEntry = SchemaEntry {
    yaml_path: "log_file_max_rolls",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const LOG_FILE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "log_file_max_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10Mb\""),
};

pub const LOG_FORMAT_JSON: SchemaEntry = SchemaEntry {
    yaml_path: "log_format_json",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOG_FORMAT_RFC3339: SchemaEntry = SchemaEntry {
    yaml_path: "log_format_rfc3339",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOG_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "log_level",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"info\""),
};

pub const LOG_PAYLOADS: SchemaEntry = SchemaEntry {
    yaml_path: "log_payloads",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOG_TO_CONSOLE: SchemaEntry = SchemaEntry {
    yaml_path: "log_to_console",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOG_TO_SYSLOG: SchemaEntry = SchemaEntry {
    yaml_path: "log_to_syslog",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGGING_FREQUENCY: SchemaEntry = SchemaEntry {
    yaml_path: "logging_frequency",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("500"),
};

pub const LOGON_DURATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "logon_duration.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_ADD_LOGSOURCE_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.add_logsource_tag",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'logs_config.additional_endpoints' — set an override in the annotation
pub const LOGS_CONFIG_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const LOGS_CONFIG_AGGREGATION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.aggregation_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

// TODO: value_type unknown for 'logs_config.api_key' — set an override in the annotation
pub const LOGS_CONFIG_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const LOGS_CONFIG_ATOMIC_REGISTRY_WRITE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.atomic_registry_write",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_AUDITOR_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auditor_ttl",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("23"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_ENABLE_DATETIME_DETECTION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.enable_datetime_detection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_ENABLE_JSON_AGGREGATION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.enable_json_aggregation",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_ENABLE_JSON_DETECTION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.enable_json_detection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_PATTERN_TABLE_MATCH_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.pattern_table_match_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.75"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_PATTERN_TABLE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.pattern_table_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("20"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_TAG_AGGREGATED_JSON: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.tag_aggregated_json",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_TIMESTAMP_DETECTOR_MATCH_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.timestamp_detector_match_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.5"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_TOKENIZER_MAX_INPUT_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line.tokenizer_max_input_bytes",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_DEFAULT_MATCH_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_default_match_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.48"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_DEFAULT_MATCH_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_default_match_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_DEFAULT_SAMPLE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_default_sample_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("500"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_DETECTION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_detection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'logs_config.auto_multi_line_detection_custom_samples' — set an override in the annotation
pub const LOGS_CONFIG_AUTO_MULTI_LINE_DETECTION_CUSTOM_SAMPLES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_detection_custom_samples",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_DETECTION_TAGGING: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_detection_tagging",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_AUTO_MULTI_LINE_EXTRA_PATTERNS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.auto_multi_line_extra_patterns",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const LOGS_CONFIG_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const LOGS_CONFIG_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const LOGS_CONFIG_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const LOGS_CONFIG_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const LOGS_CONFIG_CLOSE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.close_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const LOGS_CONFIG_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const LOGS_CONFIG_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const LOGS_CONFIG_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const LOGS_CONFIG_CONTAINER_COLLECT_ALL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.container_collect_all",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_CONTAINER_RUNTIME_WAITING_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.container_runtime_waiting_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"3s\""),
};

pub const LOGS_CONFIG_DD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.dd_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10516"),
};

// TODO: value_type unknown for 'logs_config.dd_url' — set an override in the annotation
pub const LOGS_CONFIG_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const LOGS_CONFIG_DD_URL_443: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.dd_url_443",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"agent-443-intake.logs.datadoghq.com\""),
};

pub const LOGS_CONFIG_DELEGATED_AUTH_AWS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.delegated_auth.aws.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOGS_CONFIG_DELEGATED_AUTH_ORG_UUID: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.delegated_auth.org_uuid",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOGS_CONFIG_DELEGATED_AUTH_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.delegated_auth.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOGS_CONFIG_DELEGATED_AUTH_REFRESH_INTERVAL_MINS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.delegated_auth.refresh_interval_mins",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const LOGS_CONFIG_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_DEV_MODE_USE_PROTO: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.dev_mode_use_proto",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_DISABLE_DISTRIBUTED_SENDERS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.disable_distributed_senders",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_DOCKER_CLIENT_READ_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.docker_client_read_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const LOGS_CONFIG_DOCKER_CONTAINER_FORCE_USE_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.docker_container_force_use_file",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_DOCKER_CONTAINER_USE_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.docker_container_use_file",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_DOCKER_PATH_OVERRIDE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.docker_path_override",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOGS_CONFIG_ENABLE_RECURSIVE_GLOB: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.enable_recursive_glob",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_EXPECTED_TAGS_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.expected_tags_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"0s\""),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_BURST_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.burst_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_MATCH_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.match_threshold",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.9"),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_MAX_PATTERNS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.max_patterns",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_RATE_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.rate_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const LOGS_CONFIG_EXPERIMENTAL_ADAPTIVE_SAMPLING_TOKENIZER_MAX_INPUT_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.experimental_adaptive_sampling.tokenizer_max_input_bytes",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2048"),
};

pub const LOGS_CONFIG_FILE_SCAN_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.file_scan_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const LOGS_CONFIG_FILE_WILDCARD_SELECTION_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.file_wildcard_selection_mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"by_name\""),
};

pub const LOGS_CONFIG_FINGERPRINT_CONFIG_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.fingerprint_config.count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const LOGS_CONFIG_FINGERPRINT_CONFIG_COUNT_TO_SKIP: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.fingerprint_config.count_to_skip",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const LOGS_CONFIG_FINGERPRINT_CONFIG_FINGERPRINT_STRATEGY: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.fingerprint_config.fingerprint_strategy",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"disabled\""),
};

pub const LOGS_CONFIG_FINGERPRINT_CONFIG_MAX_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.fingerprint_config.max_bytes",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100000"),
};

pub const LOGS_CONFIG_FORCE_AUTO_MULTI_LINE_DETECTION_V1: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.force_auto_multi_line_detection_v1",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_FORCE_USE_HTTP: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.force_use_http",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_FORCE_USE_TCP: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.force_use_tcp",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_FRAME_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.frame_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("9000"),
};

pub const LOGS_CONFIG_HTTP_CONNECTIVITY_RETRY_INTERVAL_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.http_connectivity_retry_interval_max",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const LOGS_CONFIG_HTTP_PROTOCOL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.http_protocol",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"auto\""),
};

pub const LOGS_CONFIG_HTTP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.http_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const LOGS_CONFIG_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const LOGS_CONFIG_INTEGRATIONS_LOGS_DISK_RATIO: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.integrations_logs_disk_ratio",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0.8"),
};

pub const LOGS_CONFIG_INTEGRATIONS_LOGS_FILES_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.integrations_logs_files_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const LOGS_CONFIG_INTEGRATIONS_LOGS_TOTAL_USAGE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.integrations_logs_total_usage",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const LOGS_CONFIG_K8S_CONTAINER_USE_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.k8s_container_use_file",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_K8S_CONTAINER_USE_KUBELET_API: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.k8s_container_use_kubelet_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_KUBELET_API_CLIENT_READ_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.kubelet_api_client_read_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"30s\""),
};

// TODO: value_type unknown for 'logs_config.logs_dd_url' — set an override in the annotation
pub const LOGS_CONFIG_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const LOGS_CONFIG_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_MAX_MESSAGE_SIZE_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.max_message_size_bytes",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("900000"),
};

pub const LOGS_CONFIG_MESSAGE_CHANNEL_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.message_channel_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const LOGS_CONFIG_OPEN_FILES_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.open_files_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: None,
};

pub const LOGS_CONFIG_PAYLOAD_CHANNEL_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.payload_channel_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const LOGS_CONFIG_PIPELINE_FAILOVER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.pipeline_failover.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_PIPELINE_FAILOVER_ROUTER_CHANNEL_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.pipeline_failover.router_channel_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const LOGS_CONFIG_PIPELINES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.pipelines",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4"),
};

pub const LOGS_CONFIG_PROCESS_EXCLUDE_AGENT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.process_exclude_agent",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'logs_config.processing_rules' — set an override in the annotation
pub const LOGS_CONFIG_PROCESSING_RULES: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.processing_rules",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const LOGS_CONFIG_RUN_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.run_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${run_path}\""),
};

pub const LOGS_CONFIG_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const LOGS_CONFIG_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const LOGS_CONFIG_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const LOGS_CONFIG_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const LOGS_CONFIG_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_SOCKS5_PROXY_ADDRESS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.socks5_proxy_address",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const LOGS_CONFIG_STOP_GRACE_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.stop_grace_period",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const LOGS_CONFIG_STREAMING_STREAMLOGS_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.streaming.streamlogs_log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${log_path}/streamlogs_info/streamlogs.log\""),
};

pub const LOGS_CONFIG_TAG_MULTI_LINE_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.tag_multi_line_logs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_TAG_TRUNCATED_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.tag_truncated_logs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_TAGGER_WARMUP_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.tagger_warmup_duration",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const LOGS_CONFIG_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_USE_HTTP: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_http",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_USE_PODMAN_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_podman_logs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_USE_PORT_443: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_port_443",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_USE_SOURCEHOST_TAG: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_sourcehost_tag",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_USE_TCP: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_tcp",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const LOGS_CONFIG_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_VALIDATE_POD_CONTAINER_ID: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.validate_pod_container_id",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const LOGS_CONFIG_WINDOWS_OPEN_FILE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.windows_open_file_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const LOGS_CONFIG_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "logs_config.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const LOGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "logs_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const MEMTRACK_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "memtrack_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const METADATA_ENDPOINTS_MAX_HOSTNAME_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "metadata_endpoints_max_hostname_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("255"),
};

pub const METADATA_IP_RESOLUTION_FROM_HOSTNAME: SchemaEntry = SchemaEntry {
    yaml_path: "metadata_ip_resolution_from_hostname",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const METADATA_PROVIDER_STOP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "metadata_provider_stop_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

// TODO: value_type unknown for 'metadata_providers' — set an override in the annotation
pub const METADATA_PROVIDERS: SchemaEntry = SchemaEntry {
    yaml_path: "metadata_providers",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const METRIC_FILTERLIST: SchemaEntry = SchemaEntry {
    yaml_path: "metric_filterlist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const METRIC_FILTERLIST_MATCH_PREFIX: SchemaEntry = SchemaEntry {
    yaml_path: "metric_filterlist_match_prefix",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'metric_tag_filterlist' — set an override in the annotation
pub const METRIC_TAG_FILTERLIST: SchemaEntry = SchemaEntry {
    yaml_path: "metric_tag_filterlist",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const METRICS_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "metrics_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

pub const MIN_TLS_VERSION: SchemaEntry = SchemaEntry {
    yaml_path: "min_tls_version",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"tlsv1.2\""),
};

// TODO: value_type unknown for 'multi_region_failover.api_key' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'multi_region_failover.dd_url' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const MULTI_REGION_FAILOVER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const MULTI_REGION_FAILOVER_FAILOVER_APM: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.failover_apm",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const MULTI_REGION_FAILOVER_FAILOVER_LOGS: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.failover_logs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const MULTI_REGION_FAILOVER_FAILOVER_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.failover_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'multi_region_failover.logs_service_allowlist' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_LOGS_SERVICE_ALLOWLIST: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.logs_service_allowlist",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'multi_region_failover.metric_allowlist' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_METRIC_ALLOWLIST: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.metric_allowlist",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_CLIENTS_CACHE_BYPASS_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.clients.cache_bypass_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_CLIENTS_TTL_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.clients.ttl_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

// TODO: value_type unknown for 'multi_region_failover.remote_configuration.config_root' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_CONFIG_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.config_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'multi_region_failover.remote_configuration.director_root' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_DIRECTOR_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.director_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'multi_region_failover.remote_configuration.key' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_MAX_BACKOFF_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.max_backoff_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5m0s\""),
};

// TODO: value_type unknown for 'multi_region_failover.remote_configuration.max_backoff_time' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_MAX_BACKOFF_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.max_backoff_time",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_ORG_STATUS_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.org_status_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

// TODO: value_type unknown for 'multi_region_failover.remote_configuration.refresh_interval' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_REMOTE_CONFIGURATION_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.remote_configuration.refresh_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'multi_region_failover.site' — set an override in the annotation
pub const MULTI_REGION_FAILOVER_SITE: SchemaEntry = SchemaEntry {
    yaml_path: "multi_region_failover.site",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'network.id' — set an override in the annotation
pub const NETWORK_ID: SchemaEntry = SchemaEntry {
    yaml_path: "network.id",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_CHECK_USE_CORE_LOADER: SchemaEntry = SchemaEntry {
    yaml_path: "network_check.use_core_loader",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

// TODO: value_type unknown for 'network_config_management.forwarder.additional_endpoints' — set an override in the annotation
pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'network_config_management.forwarder.dd_url' — set an override in the annotation
pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'network_config_management.forwarder.logs_dd_url' — set an override in the annotation
pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_CONFIG_MANAGEMENT_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_config_management.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_ALLOWED_FAILURES: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.allowed_failures",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_COLLECT_DEVICE_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.collect_device_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_COLLECT_TOPOLOGY: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.collect_topology",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_COLLECT_VPN: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.collect_vpn",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'network_devices.autodiscovery.configs' — set an override in the annotation
pub const NETWORK_DEVICES_AUTODISCOVERY_CONFIGS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.configs",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_DISCOVERY_ALLOWED_FAILURES: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.discovery_allowed_failures",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_DISCOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.discovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3600"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_LOADER: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.loader",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"core\""),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_MIN_COLLECTION_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.min_collection_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"default\""),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_OID_BATCH_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.oid_batch_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_PING_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.ping.count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_PING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.ping.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_PING_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.ping.interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_PING_LINUX_USE_RAW_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.ping.linux.use_raw_socket",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_PING_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.ping.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3000"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_RETRIES: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.retries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_USE_DEDUPLICATION: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.use_deduplication",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_USE_DEVICE_ID_AS_HOSTNAME: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.use_device_id_as_hostname",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_AUTODISCOVERY_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.autodiscovery.workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_DEFAULT_SCAN_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.default_scan.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_DEFAULT_SCAN_EXCLUDED_IPS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.default_scan.excluded_ips",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

// TODO: value_type unknown for 'network_devices.metadata.additional_endpoints' — set an override in the annotation
pub const NETWORK_DEVICES_METADATA_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_METADATA_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const NETWORK_DEVICES_METADATA_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const NETWORK_DEVICES_METADATA_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_DEVICES_METADATA_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_DEVICES_METADATA_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const NETWORK_DEVICES_METADATA_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const NETWORK_DEVICES_METADATA_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'network_devices.metadata.dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_METADATA_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_METADATA_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_METADATA_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'network_devices.metadata.logs_dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_METADATA_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_METADATA_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_METADATA_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_METADATA_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_METADATA_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const NETWORK_DEVICES_METADATA_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_METADATA_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_METADATA_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_METADATA_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_METADATA_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.metadata.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"default\""),
};

// TODO: value_type unknown for 'network_devices.netflow.aggregator_buffer_size' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_AGGREGATOR_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.aggregator_buffer_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'network_devices.netflow.aggregator_flow_context_ttl' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_AGGREGATOR_FLOW_CONTEXT_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.aggregator_flow_context_ttl",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'network_devices.netflow.aggregator_flush_interval' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_AGGREGATOR_FLUSH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.aggregator_flush_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'network_devices.netflow.aggregator_port_rollup_threshold' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_AGGREGATOR_PORT_ROLLUP_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.aggregator_port_rollup_threshold",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'network_devices.netflow.aggregator_rollup_tracker_refresh_interval' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_AGGREGATOR_ROLLUP_TRACKER_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.aggregator_rollup_tracker_refresh_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_NETFLOW_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'network_devices.netflow.forwarder.additional_endpoints' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'network_devices.netflow.forwarder.dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'network_devices.netflow.forwarder.logs_dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_NETFLOW_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'network_devices.netflow.listeners' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_LISTENERS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.listeners",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_NETFLOW_REVERSE_DNS_ENRICHMENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.reverse_dns_enrichment_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'network_devices.netflow.stop_timeout' — set an override in the annotation
pub const NETWORK_DEVICES_NETFLOW_STOP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.netflow.stop_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_SNMP_TRAPS_BIND_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.bind_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"0.0.0.0\""),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_COMMUNITY_STRINGS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.community_strings",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'network_devices.snmp_traps.forwarder.additional_endpoints' — set an override in the annotation
pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'network_devices.snmp_traps.forwarder.dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'network_devices.snmp_traps.forwarder.logs_dd_url' — set an override in the annotation
pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("9162"),
};

pub const NETWORK_DEVICES_SNMP_TRAPS_STOP_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.stop_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

// TODO: value_type unknown for 'network_devices.snmp_traps.users' — set an override in the annotation
pub const NETWORK_DEVICES_SNMP_TRAPS_USERS: SchemaEntry = SchemaEntry {
    yaml_path: "network_devices.snmp_traps.users",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

// TODO: value_type unknown for 'network_path.collector.dest_excludes' — set an override in the annotation
pub const NETWORK_PATH_COLLECTOR_DEST_EXCLUDES: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.dest_excludes",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const NETWORK_PATH_COLLECTOR_DISABLE_INTRA_VPC_COLLECTION: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.disable_intra_vpc_collection",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_COLLECTOR_DISABLE_WINDOWS_DRIVER: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.disable_windows_driver",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_COLLECTOR_E2E_QUERIES: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.e2e_queries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("50"),
};

// TODO: value_type unknown for 'network_path.collector.filters' — set an override in the annotation
pub const NETWORK_PATH_COLLECTOR_FILTERS: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.filters",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const NETWORK_PATH_COLLECTOR_FLUSH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.flush_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10s\""),
};

pub const NETWORK_PATH_COLLECTOR_ICMP_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.icmp_mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const NETWORK_PATH_COLLECTOR_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_PATH_COLLECTOR_MAX_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.max_ttl",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const NETWORK_PATH_COLLECTOR_MONITOR_IP_WITHOUT_DOMAIN: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.monitor_ip_without_domain",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_COLLECTOR_PATHTEST_CONTEXTS_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.pathtest_contexts_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_PATH_COLLECTOR_PATHTEST_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.pathtest_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"30m\""),
};

pub const NETWORK_PATH_COLLECTOR_PATHTEST_MAX_BURST_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.pathtest_max_burst_duration",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"30s\""),
};

pub const NETWORK_PATH_COLLECTOR_PATHTEST_MAX_PER_MINUTE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.pathtest_max_per_minute",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("150"),
};

pub const NETWORK_PATH_COLLECTOR_PATHTEST_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.pathtest_ttl",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"70m\""),
};

pub const NETWORK_PATH_COLLECTOR_PROCESSING_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.processing_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_PATH_COLLECTOR_REVERSE_DNS_ENRICHMENT_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.reverse_dns_enrichment.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_PATH_COLLECTOR_REVERSE_DNS_ENRICHMENT_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.reverse_dns_enrichment.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

// TODO: value_type unknown for 'network_path.collector.source_excludes' — set an override in the annotation
pub const NETWORK_PATH_COLLECTOR_SOURCE_EXCLUDES: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.source_excludes",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const NETWORK_PATH_COLLECTOR_TCP_METHOD: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.tcp_method",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const NETWORK_PATH_COLLECTOR_TCP_SYN_PARIS_TRACEROUTE_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.tcp_syn_paris_traceroute_mode",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_COLLECTOR_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_PATH_COLLECTOR_TRACEROUTE_QUERIES: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.traceroute_queries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const NETWORK_PATH_COLLECTOR_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.collector.workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4"),
};

pub const NETWORK_PATH_CONNECTIONS_MONITORING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.connections_monitoring.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'network_path.forwarder.additional_endpoints' — set an override in the annotation
pub const NETWORK_PATH_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_PATH_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const NETWORK_PATH_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const NETWORK_PATH_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const NETWORK_PATH_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const NETWORK_PATH_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const NETWORK_PATH_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const NETWORK_PATH_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'network_path.forwarder.dd_url' — set an override in the annotation
pub const NETWORK_PATH_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_PATH_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'network_path.forwarder.logs_dd_url' — set an override in the annotation
pub const NETWORK_PATH_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const NETWORK_PATH_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NETWORK_PATH_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_PATH_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const NETWORK_PATH_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const NETWORK_PATH_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NETWORK_PATH_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_PATH_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const NETWORK_PATH_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "network_path.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const NO_PROXY_NONEXACT_MATCH: SchemaEntry = SchemaEntry {
    yaml_path: "no_proxy_nonexact_match",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const NOTABLE_EVENTS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "notable_events.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.logs.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OBSERVABILITY_PIPELINES_WORKER_LOGS_URL: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.logs.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OBSERVABILITY_PIPELINES_WORKER_METRICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.metrics.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OBSERVABILITY_PIPELINES_WORKER_METRICS_URL: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.metrics.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OBSERVABILITY_PIPELINES_WORKER_TRACES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.traces.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OBSERVABILITY_PIPELINES_WORKER_TRACES_URL: SchemaEntry = SchemaEntry {
    yaml_path: "observability_pipelines_worker.traces.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'ol_proxy_config.additional_endpoints' — set an override in the annotation
pub const OL_PROXY_CONFIG_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'ol_proxy_config.api_key' — set an override in the annotation
pub const OL_PROXY_CONFIG_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const OL_PROXY_CONFIG_API_VERSION: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.api_version",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

// TODO: value_type unknown for 'ol_proxy_config.dd_url' — set an override in the annotation
pub const OL_PROXY_CONFIG_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const OL_PROXY_CONFIG_DELEGATED_AUTH_AWS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.delegated_auth.aws.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OL_PROXY_CONFIG_DELEGATED_AUTH_ORG_UUID: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.delegated_auth.org_uuid",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OL_PROXY_CONFIG_DELEGATED_AUTH_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.delegated_auth.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OL_PROXY_CONFIG_DELEGATED_AUTH_REFRESH_INTERVAL_MINS: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.delegated_auth.refresh_interval_mins",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const OL_PROXY_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "ol_proxy_config.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_COLLECTOR_DISCOVERY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.collector_discovery.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_CONTAINER_SCRUBBING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.container_scrubbing.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_RESOURCES_MAX_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_resources.max_count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_RESOURCES_OOTB_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_resources.ootb.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_RESOURCES_OOTB_GATEWAY_API: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_resources.ootb.gateway_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_RESOURCES_OOTB_INGRESS_CONTROLLERS: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_resources.ootb.ingress_controllers",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_RESOURCES_OOTB_SERVICE_MESH: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_resources.ootb.service_mesh",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_SENSITIVE_ANNOTATIONS_LABELS: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_sensitive_annotations_labels",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const ORCHESTRATOR_EXPLORER_CUSTOM_SENSITIVE_WORDS: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.custom_sensitive_words",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const ORCHESTRATOR_EXPLORER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_EXTRA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.extra_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const ORCHESTRATOR_EXPLORER_KUBELET_CONFIG_CHECK_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.kubelet_config_check.enabled",
    env_vars: &["DD_ORCHESTRATOR_EXPLORER_KUBELET_CONFIG_CHECK_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_MANIFEST_COLLECTION_BUFFER_FLUSH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.manifest_collection.buffer_flush_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"20s\""),
};

pub const ORCHESTRATOR_EXPLORER_MANIFEST_COLLECTION_BUFFER_MANIFEST: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.manifest_collection.buffer_manifest",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_MANIFEST_COLLECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.manifest_collection.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'orchestrator_explorer.max_message_bytes' — set an override in the annotation
pub const ORCHESTRATOR_EXPLORER_MAX_MESSAGE_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.max_message_bytes",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'orchestrator_explorer.max_per_message' — set an override in the annotation
pub const ORCHESTRATOR_EXPLORER_MAX_PER_MESSAGE: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.max_per_message",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'orchestrator_explorer.orchestrator_additional_endpoints' — set an override in the annotation
pub const ORCHESTRATOR_EXPLORER_ORCHESTRATOR_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.orchestrator_additional_endpoints",
    env_vars: &[
        "DD_ORCHESTRATOR_EXPLORER_ORCHESTRATOR_ADDITIONAL_ENDPOINTS",
        "DD_ORCHESTRATOR_ADDITIONAL_ENDPOINTS",
    ],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'orchestrator_explorer.orchestrator_dd_url' — set an override in the annotation
pub const ORCHESTRATOR_EXPLORER_ORCHESTRATOR_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.orchestrator_dd_url",
    env_vars: &["DD_ORCHESTRATOR_EXPLORER_ORCHESTRATOR_DD_URL", "DD_ORCHESTRATOR_URL"],
    value_type: ValueType::String,
    default: None,
};

pub const ORCHESTRATOR_EXPLORER_TERMINATED_PODS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.terminated_pods.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const ORCHESTRATOR_EXPLORER_TERMINATED_PODS_IMPROVED_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.terminated_pods_improved.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const ORCHESTRATOR_EXPLORER_TERMINATED_RESOURCES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.terminated_resources.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'orchestrator_explorer.use_legacy_endpoint' — set an override in the annotation
pub const ORCHESTRATOR_EXPLORER_USE_LEGACY_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "orchestrator_explorer.use_legacy_endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const ORIGIN_DETECTION_UNIFIED: SchemaEntry = SchemaEntry {
    yaml_path: "origin_detection_unified",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTEL_STANDALONE: SchemaEntry = SchemaEntry {
    yaml_path: "otel_standalone",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTELCOLLECTOR_CONVERTER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.converter.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const OTELCOLLECTOR_CONVERTER_FEATURES: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.converter.features",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const OTELCOLLECTOR_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTELCOLLECTOR_EXTENSION_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.extension_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTELCOLLECTOR_EXTENSION_URL: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.extension_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"https://localhost:7777\""),
};

pub const OTELCOLLECTOR_FLARE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.flare.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const OTELCOLLECTOR_GATEWAY_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.gateway.mode",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTELCOLLECTOR_INSTALLATION_METHOD: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.installation_method",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OTELCOLLECTOR_SUBMIT_DUMMY_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "otelcollector.submit_dummy_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_DEBUG_VERBOSITY: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.debug.verbosity",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"basic\""),
};

pub const OTLP_CONFIG_GRPC_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.grpc_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_HTTP_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.http_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_LOGS_BATCH_FLUSH_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.logs.batch.flush_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"200ms\""),
};

pub const OTLP_CONFIG_LOGS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.logs.batch.max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_LOGS_BATCH_MIN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.logs.batch.min_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8192"),
};

pub const OTLP_CONFIG_LOGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.logs.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_METRICS_BATCH_FLUSH_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.batch.flush_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"200ms\""),
};

pub const OTLP_CONFIG_METRICS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.batch.max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_METRICS_BATCH_MIN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.batch.min_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("8192"),
};

pub const OTLP_CONFIG_METRICS_DELTA_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.delta_ttl",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3600"),
};

pub const OTLP_CONFIG_METRICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const OTLP_CONFIG_METRICS_HISTOGRAMS_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.histograms.mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"distributions\""),
};

pub const OTLP_CONFIG_METRICS_HISTOGRAMS_SEND_AGGREGATION_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.histograms.send_aggregation_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_METRICS_HISTOGRAMS_SEND_COUNT_SUM_METRICS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.histograms.send_count_sum_metrics",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_METRICS_INSTRUMENTATION_SCOPE_METADATA_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.instrumentation_scope_metadata_as_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const OTLP_CONFIG_METRICS_RESOURCE_ATTRIBUTES_AS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.resource_attributes_as_tags",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_METRICS_SUMMARIES_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.summaries.mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"gauges\""),
};

pub const OTLP_CONFIG_METRICS_SUMS_CUMULATIVE_MONOTONIC_MODE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.sums.cumulative_monotonic_mode",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"to_delta\""),
};

pub const OTLP_CONFIG_METRICS_SUMS_INITIAL_CUMULATIVE_MONOTONIC_VALUE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.sums.initial_cumulative_monotonic_value",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"auto\""),
};

pub const OTLP_CONFIG_METRICS_TAG_CARDINALITY: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.tag_cardinality",
    env_vars: &["DD_OTLP_CONFIG_METRICS_TAG_CARDINALITY", "DD_OTLP_TAG_CARDINALITY"],
    value_type: ValueType::String,
    default: Some("\"low\""),
};

pub const OTLP_CONFIG_METRICS_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.metrics.tags",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost:4317\""),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_INCLUDE_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.include_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_KEEPALIVE_ENFORCEMENT_POLICY_MIN_TIME: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.keepalive.enforcement_policy.min_time",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"5m\""),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_MAX_CONCURRENT_STREAMS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.max_concurrent_streams",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_MAX_RECV_MSG_SIZE_MIB: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_READ_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.read_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("524288"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TRANSPORT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.transport",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"tcp\""),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_WRITE_BUFFER_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.grpc.write_buffer_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_CORS_ALLOWED_HEADERS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.cors.allowed_headers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_CORS_ALLOWED_ORIGINS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.cors.allowed_origins",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.endpoint",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"localhost:4318\""),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_INCLUDE_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.include_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_MAX_REQUEST_BODY_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.max_request_body_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const OTLP_CONFIG_TRACES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const OTLP_CONFIG_TRACES_INFRA_ATTRIBUTES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.infra_attributes.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const OTLP_CONFIG_TRACES_INTERNAL_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.internal_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5003"),
};

pub const OTLP_CONFIG_TRACES_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.probabilistic_sampler.sampling_percentage",
    env_vars: &["DD_OTLP_CONFIG_TRACES_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE"],
    value_type: ValueType::Float,
    default: Some("100"),
};

pub const OTLP_CONFIG_TRACES_SPAN_NAME_AS_RESOURCE_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.span_name_as_resource_name",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'otlp_config.traces.span_name_remappings' — set an override in the annotation
pub const OTLP_CONFIG_TRACES_SPAN_NAME_REMAPPINGS: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.span_name_remappings",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const PODMAN_DB_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "podman_db_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PRIORITIZE_GO_CHECK_LOADER: SchemaEntry = SchemaEntry {
    yaml_path: "prioritize_go_check_loader",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const PRIVATE_ACTION_RUNNER_ACTIONS_ALLOWLIST: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.actions_allowlist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PRIVATE_ACTION_RUNNER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PRIVATE_ACTION_RUNNER_HTTP_ALLOW_IMDS_ENDPOINT: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.http_allow_imds_endpoint",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PRIVATE_ACTION_RUNNER_HTTP_ALLOWLIST: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.http_allowlist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PRIVATE_ACTION_RUNNER_HTTP_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.http_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const PRIVATE_ACTION_RUNNER_IDENTITY_FILE_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.identity_file_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PRIVATE_ACTION_RUNNER_IDENTITY_SECRET_NAME: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.identity_secret_name",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"private-action-runner-identity\""),
};

pub const PRIVATE_ACTION_RUNNER_IDENTITY_USE_K8S_SECRET: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.identity_use_k8s_secret",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const PRIVATE_ACTION_RUNNER_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${log_path}/private-action-runner.log\""),
};

pub const PRIVATE_ACTION_RUNNER_PRIVATE_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.private_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PRIVATE_ACTION_RUNNER_RESTRICTED_SHELL_ALLOWED_PATHS: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.restricted_shell_allowed_paths",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const PRIVATE_ACTION_RUNNER_SELF_ENROLL: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.self_enroll",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const PRIVATE_ACTION_RUNNER_TASK_CONCURRENCY: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.task_concurrency",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const PRIVATE_ACTION_RUNNER_TASK_TIMEOUT_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.task_timeout_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const PRIVATE_ACTION_RUNNER_URN: SchemaEntry = SchemaEntry {
    yaml_path: "private_action_runner.urn",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROC_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "proc_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/proc\""),
};

// TODO: value_type unknown for 'process_config.additional_endpoints' — set an override in the annotation
pub const PROCESS_CONFIG_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.additional_endpoints",
    env_vars: &[
        "DD_PROCESS_CONFIG_ADDITIONAL_ENDPOINTS",
        "DD_PROCESS_AGENT_ADDITIONAL_ENDPOINTS",
        "DD_PROCESS_ADDITIONAL_ENDPOINTS",
    ],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const PROCESS_CONFIG_BLACKLIST_PATTERNS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.blacklist_patterns",
    env_vars: &[
        "DD_PROCESS_CONFIG_BLACKLIST_PATTERNS",
        "DD_PROCESS_AGENT_BLACKLIST_PATTERNS",
    ],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PROCESS_CONFIG_CACHE_LOOKUPID: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.cache_lookupid",
    env_vars: &["DD_PROCESS_CONFIG_CACHE_LOOKUPID", "DD_PROCESS_AGENT_CACHE_LOOKUPID"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROCESS_CONFIG_CMD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.cmd_port",
    env_vars: &["DD_PROCESS_CONFIG_CMD_PORT", "DD_PROCESS_AGENT_CMD_PORT"],
    value_type: ValueType::Float,
    default: Some("6162"),
};

pub const PROCESS_CONFIG_CONTAINER_COLLECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.container_collection.enabled",
    env_vars: &[
        "DD_PROCESS_CONFIG_CONTAINER_COLLECTION_ENABLED",
        "DD_PROCESS_AGENT_CONTAINER_COLLECTION_ENABLED",
    ],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const PROCESS_CONFIG_CUSTOM_SENSITIVE_WORDS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.custom_sensitive_words",
    env_vars: &[
        "DD_CUSTOM_SENSITIVE_WORDS",
        "DD_PROCESS_CONFIG_CUSTOM_SENSITIVE_WORDS",
        "DD_PROCESS_AGENT_CUSTOM_SENSITIVE_WORDS",
    ],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PROCESS_CONFIG_DD_AGENT_BIN: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.dd_agent_bin",
    env_vars: &["DD_PROCESS_CONFIG_DD_AGENT_BIN", "DD_PROCESS_AGENT_DD_AGENT_BIN"],
    value_type: ValueType::String,
    default: None,
};

pub const PROCESS_CONFIG_DD_AGENT_ENV: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.dd_agent_env",
    env_vars: &["DD_PROCESS_CONFIG_DD_AGENT_ENV", "DD_PROCESS_AGENT_DD_AGENT_ENV"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROCESS_CONFIG_DISABLE_REALTIME_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.disable_realtime_checks",
    env_vars: &[
        "DD_PROCESS_CONFIG_DISABLE_REALTIME_CHECKS",
        "DD_PROCESS_AGENT_DISABLE_REALTIME_CHECKS",
    ],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROCESS_CONFIG_DROP_CHECK_PAYLOADS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.drop_check_payloads",
    env_vars: &[
        "DD_PROCESS_CONFIG_DROP_CHECK_PAYLOADS",
        "DD_PROCESS_AGENT_DROP_CHECK_PAYLOADS",
    ],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PROCESS_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.enabled",
    env_vars: &["DD_PROCESS_CONFIG_ENABLED", "DD_PROCESS_AGENT_ENABLED"],
    value_type: ValueType::String,
    default: Some("\"false\""),
};

pub const PROCESS_CONFIG_EXPVAR_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.expvar_port",
    env_vars: &["DD_PROCESS_CONFIG_EXPVAR_PORT", "DD_PROCESS_AGENT_EXPVAR_PORT"],
    value_type: ValueType::Float,
    default: Some("6062"),
};

pub const PROCESS_CONFIG_GRPC_CONNECTION_TIMEOUT_SECS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.grpc_connection_timeout_secs",
    env_vars: &[
        "DD_PROCESS_CONFIG_GRPC_CONNECTION_TIMEOUT_SECS",
        "DD_PROCESS_AGENT_GRPC_CONNECTION_TIMEOUT_SECS",
    ],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const PROCESS_CONFIG_IGNORE_ZOMBIE_PROCESSES: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.ignore_zombie_processes",
    env_vars: &[
        "DD_PROCESS_CONFIG_IGNORE_ZOMBIE_PROCESSES",
        "DD_PROCESS_AGENT_IGNORE_ZOMBIE_PROCESSES",
    ],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROCESS_CONFIG_INTERNAL_PROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.internal_profiling.enabled",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERNAL_PROFILING_ENABLED",
        "DD_PROCESS_AGENT_INTERNAL_PROFILING_ENABLED",
    ],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROCESS_CONFIG_INTERVALS_CONNECTIONS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.intervals.connections",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERVALS_CONNECTIONS",
        "DD_PROCESS_AGENT_INTERVALS_CONNECTIONS",
    ],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const PROCESS_CONFIG_INTERVALS_CONTAINER: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.intervals.container",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERVALS_CONTAINER",
        "DD_PROCESS_AGENT_INTERVALS_CONTAINER",
    ],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const PROCESS_CONFIG_INTERVALS_CONTAINER_REALTIME: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.intervals.container_realtime",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERVALS_CONTAINER_REALTIME",
        "DD_PROCESS_AGENT_INTERVALS_CONTAINER_REALTIME",
    ],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const PROCESS_CONFIG_INTERVALS_PROCESS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.intervals.process",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERVALS_PROCESS",
        "DD_PROCESS_AGENT_INTERVALS_PROCESS",
    ],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const PROCESS_CONFIG_INTERVALS_PROCESS_REALTIME: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.intervals.process_realtime",
    env_vars: &[
        "DD_PROCESS_CONFIG_INTERVALS_PROCESS_REALTIME",
        "DD_PROCESS_AGENT_INTERVALS_PROCESS_REALTIME",
    ],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const PROCESS_CONFIG_LANGUAGE_DETECTION_GRPC_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.language_detection.grpc_port",
    env_vars: &[
        "DD_PROCESS_CONFIG_LANGUAGE_DETECTION_GRPC_PORT",
        "DD_PROCESS_AGENT_LANGUAGE_DETECTION_GRPC_PORT",
    ],
    value_type: ValueType::Float,
    default: Some("6262"),
};

pub const PROCESS_CONFIG_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.log_file",
    env_vars: &["DD_PROCESS_CONFIG_LOG_FILE", "DD_PROCESS_AGENT_LOG_FILE"],
    value_type: ValueType::String,
    default: Some("\"${log_path}/process-agent.log\""),
};

pub const PROCESS_CONFIG_MAX_MESSAGE_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.max_message_bytes",
    env_vars: &[
        "DD_PROCESS_CONFIG_MAX_MESSAGE_BYTES",
        "DD_PROCESS_AGENT_MAX_MESSAGE_BYTES",
    ],
    value_type: ValueType::Float,
    default: Some("1000000"),
};

pub const PROCESS_CONFIG_MAX_PER_MESSAGE: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.max_per_message",
    env_vars: &["DD_PROCESS_CONFIG_MAX_PER_MESSAGE", "DD_PROCESS_AGENT_MAX_PER_MESSAGE"],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'process_config.orchestrator_additional_endpoints' — set an override in the annotation
pub const PROCESS_CONFIG_ORCHESTRATOR_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.orchestrator_additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

// TODO: value_type unknown for 'process_config.orchestrator_dd_url' — set an override in the annotation
pub const PROCESS_CONFIG_ORCHESTRATOR_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.orchestrator_dd_url",
    env_vars: &[
        "DD_PROCESS_CONFIG_ORCHESTRATOR_DD_URL",
        "DD_PROCESS_AGENT_ORCHESTRATOR_DD_URL",
    ],
    value_type: ValueType::String,
    default: None,
};

pub const PROCESS_CONFIG_PROCESS_COLLECTION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_collection.enabled",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_COLLECTION_ENABLED",
        "DD_PROCESS_AGENT_PROCESS_COLLECTION_ENABLED",
    ],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'process_config.process_dd_url' — set an override in the annotation
pub const PROCESS_CONFIG_PROCESS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_dd_url",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_DD_URL",
        "DD_PROCESS_AGENT_PROCESS_DD_URL",
        "DD_PROCESS_AGENT_URL",
        "DD_PROCESS_CONFIG_URL",
    ],
    value_type: ValueType::String,
    default: None,
};

pub const PROCESS_CONFIG_PROCESS_DISCOVERY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_discovery.enabled",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_DISCOVERY_ENABLED",
        "DD_PROCESS_AGENT_PROCESS_DISCOVERY_ENABLED",
        "DD_PROCESS_CONFIG_DISCOVERY_ENABLED",
        "DD_PROCESS_AGENT_DISCOVERY_ENABLED",
    ],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const PROCESS_CONFIG_PROCESS_DISCOVERY_HINT_FREQUENCY: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_discovery.hint_frequency",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_DISCOVERY_HINT_FREQUENCY",
        "DD_PROCESS_AGENT_PROCESS_DISCOVERY_HINT_FREQUENCY",
    ],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const PROCESS_CONFIG_PROCESS_DISCOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_discovery.interval",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_DISCOVERY_INTERVAL",
        "DD_PROCESS_AGENT_PROCESS_DISCOVERY_INTERVAL",
    ],
    value_type: ValueType::Float,
    default: Some("\"4h0m0s\""),
};

pub const PROCESS_CONFIG_PROCESS_QUEUE_BYTES: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.process_queue_bytes",
    env_vars: &[
        "DD_PROCESS_CONFIG_PROCESS_QUEUE_BYTES",
        "DD_PROCESS_AGENT_PROCESS_QUEUE_BYTES",
    ],
    value_type: ValueType::Float,
    default: Some("60000000"),
};

pub const PROCESS_CONFIG_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.queue_size",
    env_vars: &["DD_PROCESS_CONFIG_QUEUE_SIZE", "DD_PROCESS_AGENT_QUEUE_SIZE"],
    value_type: ValueType::Float,
    default: Some("256"),
};

pub const PROCESS_CONFIG_RT_QUEUE_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.rt_queue_size",
    env_vars: &["DD_PROCESS_CONFIG_RT_QUEUE_SIZE", "DD_PROCESS_AGENT_RT_QUEUE_SIZE"],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const PROCESS_CONFIG_SCRUB_ARGS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.scrub_args",
    env_vars: &[
        "DD_SCRUB_ARGS",
        "DD_PROCESS_CONFIG_SCRUB_ARGS",
        "DD_PROCESS_AGENT_SCRUB_ARGS",
    ],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'process_config.strip_proc_arguments' — set an override in the annotation
pub const PROCESS_CONFIG_STRIP_PROC_ARGUMENTS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.strip_proc_arguments",
    env_vars: &[
        "DD_STRIP_PROCESS_ARGS",
        "DD_PROCESS_CONFIG_STRIP_PROC_ARGUMENTS",
        "DD_PROCESS_AGENT_STRIP_PROC_ARGUMENTS",
    ],
    value_type: ValueType::String,
    default: None,
};

pub const PROCESS_CONFIG_WINDOWS_USE_PERF_COUNTERS: SchemaEntry = SchemaEntry {
    yaml_path: "process_config.windows.use_perf_counters",
    env_vars: &[
        "DD_PROCESS_CONFIG_WINDOWS_USE_PERF_COUNTERS",
        "DD_PROCESS_AGENT_WINDOWS_USE_PERF_COUNTERS",
    ],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROCFS_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "procfs_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROMETHEUS_SCRAPE_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "prometheus_scrape.checks",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROMETHEUS_SCRAPE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "prometheus_scrape.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROMETHEUS_SCRAPE_SERVICE_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "prometheus_scrape.service_endpoints",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const PROMETHEUS_SCRAPE_VERSION: SchemaEntry = SchemaEntry {
    yaml_path: "prometheus_scrape.version",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

// TODO: value_type unknown for 'provider_kind' — set an override in the annotation
pub const PROVIDER_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "provider_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const PROXY_HTTP: SchemaEntry = SchemaEntry {
    yaml_path: "proxy.http",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROXY_HTTPS: SchemaEntry = SchemaEntry {
    yaml_path: "proxy.https",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const PROXY_NO_PROXY: SchemaEntry = SchemaEntry {
    yaml_path: "proxy.no_proxy",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const PYTHON3_LINTER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "python3_linter_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const PYTHON_LAZY_LOADING: SchemaEntry = SchemaEntry {
    yaml_path: "python_lazy_loading",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REMOTE_AGENT_CONFIGSTREAM_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.configstream.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_AGENT_CONFIGSTREAM_SLEEP_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.configstream.sleep_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10s\""),
};

pub const REMOTE_AGENT_REGISTRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.registry.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REMOTE_AGENT_REGISTRY_IDLE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.registry.idle_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

pub const REMOTE_AGENT_REGISTRY_QUERY_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.registry.query_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"3s\""),
};

pub const REMOTE_AGENT_REGISTRY_RECOMMENDED_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_agent.registry.recommended_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"10s\""),
};

pub const REMOTE_CONFIGURATION_AGENT_INTEGRATIONS_ALLOW_LIST: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.agent_integrations.allow_list",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const REMOTE_CONFIGURATION_AGENT_INTEGRATIONS_ALLOW_LOG_CONFIG_SCHEDULING: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.agent_integrations.allow_log_config_scheduling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_CONFIGURATION_AGENT_INTEGRATIONS_BLOCK_LIST: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.agent_integrations.block_list",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const REMOTE_CONFIGURATION_AGENT_INTEGRATIONS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.agent_integrations.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'remote_configuration.api_key' — set an override in the annotation
pub const REMOTE_CONFIGURATION_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.api_key",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const REMOTE_CONFIGURATION_APM_SAMPLING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.apm_sampling.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REMOTE_CONFIGURATION_CLIENTS_CACHE_BYPASS_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.clients.cache_bypass_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const REMOTE_CONFIGURATION_CLIENTS_TTL_SECONDS: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.clients.ttl_seconds",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"30s\""),
};

pub const REMOTE_CONFIGURATION_CONFIG_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.config_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_DELEGATED_AUTH_AWS_REGION: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.delegated_auth.aws.region",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_DELEGATED_AUTH_ORG_UUID: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.delegated_auth.org_uuid",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_DELEGATED_AUTH_PROVIDER: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.delegated_auth.provider",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_DELEGATED_AUTH_REFRESH_INTERVAL_MINS: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.delegated_auth.refresh_interval_mins",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const REMOTE_CONFIGURATION_DIRECTOR_ROOT: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.director_root",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REMOTE_CONFIGURATION_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.key",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const REMOTE_CONFIGURATION_MAX_BACKOFF_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.max_backoff_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5m0s\""),
};

pub const REMOTE_CONFIGURATION_NO_TLS: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.no_tls",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_CONFIGURATION_NO_TLS_VALIDATION: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.no_tls_validation",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_CONFIGURATION_NO_WEBSOCKET_ECHO: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.no_websocket_echo",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_CONFIGURATION_ORG_STATUS_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.org_status_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

// TODO: value_type unknown for 'remote_configuration.rc_dd_url' — set an override in the annotation
pub const REMOTE_CONFIGURATION_RC_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.rc_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'remote_configuration.refresh_interval' — set an override in the annotation
pub const REMOTE_CONFIGURATION_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "remote_configuration.refresh_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const REMOTE_POLICIES: SchemaEntry = SchemaEntry {
    yaml_path: "remote_policies",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const REMOTE_TAGGER_MAX_CONCURRENT_SYNC: SchemaEntry = SchemaEntry {
    yaml_path: "remote_tagger.max_concurrent_sync",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const REMOTE_UPDATES: SchemaEntry = SchemaEntry {
    yaml_path: "remote_updates",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REVERSE_DNS_ENRICHMENT_CACHE_CLEAN_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.clean_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"2h0m0s\""),
};

pub const REVERSE_DNS_ENRICHMENT_CACHE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const REVERSE_DNS_ENRICHMENT_CACHE_ENTRY_TTL: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.entry_ttl",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"24h0m0s\""),
};

pub const REVERSE_DNS_ENRICHMENT_CACHE_MAX_RETRIES: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.max_retries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

// TODO: value_type unknown for 'reverse_dns_enrichment.cache.max_size' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_CACHE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.max_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const REVERSE_DNS_ENRICHMENT_CACHE_PERSIST_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.cache.persist_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"2h0m0s\""),
};

// TODO: value_type unknown for 'reverse_dns_enrichment.chan_size' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.chan_size",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'reverse_dns_enrichment.rate_limiter.limit_per_sec' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_LIMIT_PER_SEC: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.limit_per_sec",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'reverse_dns_enrichment.rate_limiter.limit_throttled_per_sec' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_LIMIT_THROTTLED_PER_SEC: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.limit_throttled_per_sec",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"5s\""),
};

// TODO: value_type unknown for 'reverse_dns_enrichment.rate_limiter.recovery_intervals' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_RECOVERY_INTERVALS: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.recovery_intervals",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'reverse_dns_enrichment.rate_limiter.throttle_error_threshold' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_RATE_LIMITER_THROTTLE_ERROR_THRESHOLD: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.rate_limiter.throttle_error_threshold",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

// TODO: value_type unknown for 'reverse_dns_enrichment.workers' — set an override in the annotation
pub const REVERSE_DNS_ENRICHMENT_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "reverse_dns_enrichment.workers",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUN_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "run_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${run_path}\""),
};

// TODO: value_type unknown for 'runtime_security_config.activity_dump.remote_storage.endpoints.additional_endpoints' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_ADDITIONAL_ENDPOINTS: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.additional_endpoints",
        env_vars: &[],
        value_type: ValueType::String,
        default: None,
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_BATCH_MAX_CONCURRENT_SEND: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.batch_max_concurrent_send",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("0"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_BATCH_MAX_CONTENT_SIZE: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.batch_max_content_size",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("5000000"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_CONNECTION_RESET_INTERVAL: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.connection_reset_interval",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("0"),
    };

// TODO: value_type unknown for 'runtime_security_config.activity_dump.remote_storage.endpoints.dd_url' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'runtime_security_config.activity_dump.remote_storage.endpoints.logs_dd_url' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_SENDER_BACKOFF_BASE: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.sender_backoff_base",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("1"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_SENDER_BACKOFF_FACTOR: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.sender_backoff_factor",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("2"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_SENDER_BACKOFF_MAX: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.sender_backoff_max",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("120"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_SENDER_RECOVERY_INTERVAL: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.sender_recovery_interval",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("2"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_SENDER_RECOVERY_RESET: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.sender_recovery_reset",
        env_vars: &[],
        value_type: ValueType::Bool,
        default: Some("false"),
    };

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const RUNTIME_SECURITY_CONFIG_ACTIVITY_DUMP_REMOTE_STORAGE_ENDPOINTS_ZSTD_COMPRESSION_LEVEL: SchemaEntry =
    SchemaEntry {
        yaml_path: "runtime_security_config.activity_dump.remote_storage.endpoints.zstd_compression_level",
        env_vars: &[],
        value_type: ValueType::Float,
        default: Some("1"),
    };

pub const RUNTIME_SECURITY_CONFIG_CMD_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.cmd_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const RUNTIME_SECURITY_CONFIG_DIRECT_SEND_FROM_SYSTEM_PROBE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.direct_send_from_system_probe",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'runtime_security_config.endpoints.additional_endpoints' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'runtime_security_config.endpoints.dd_url' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'runtime_security_config.endpoints.logs_dd_url' — set an override in the annotation
pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const RUNTIME_SECURITY_CONFIG_ENDPOINTS_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.endpoints.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const RUNTIME_SECURITY_CONFIG_EVENT_GRPC_SERVER: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.event_grpc_server",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const RUNTIME_SECURITY_CONFIG_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const RUNTIME_SECURITY_CONFIG_USE_SECRUNTIME_TRACK: SchemaEntry = SchemaEntry {
    yaml_path: "runtime_security_config.use_secruntime_track",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'sbom.additional_endpoints' — set an override in the annotation
pub const SBOM_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SBOM_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SBOM_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const SBOM_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const SBOM_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SBOM_CACHE_CLEAN_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.cache.clean_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const SBOM_CACHE_MAX_DISK_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.cache.max_disk_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100000000"),
};

pub const SBOM_CACHE_DIRECTORY: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.cache_directory",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${run_path}/sbom-agent\""),
};

pub const SBOM_CLEAR_CACHE_ON_EXIT: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.clear_cache_on_exit",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const SBOM_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const SBOM_COMPUTE_DEPENDENCIES: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.compute_dependencies",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SBOM_CONTAINER_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_ADDITIONAL_DIRECTORIES: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.additional_directories",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SBOM_CONTAINER_IMAGE_ALLOW_MISSING_REPODIGEST: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.allow_missing_repodigest",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_ANALYZERS: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.analyzers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const SBOM_CONTAINER_IMAGE_CHECK_DISK_USAGE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.check_disk_usage",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_CONTAINER_IMAGE_CONTAINER_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.container_exclude",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SBOM_CONTAINER_IMAGE_CONTAINER_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.container_include",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SBOM_CONTAINER_IMAGE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_EXCLUDE_PAUSE_CONTAINER: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.exclude_pause_container",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_CONTAINER_IMAGE_MIN_AVAILABLE_DISK: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.min_available_disk",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1Gb\""),
};

pub const SBOM_CONTAINER_IMAGE_OVERLAYFS_DIRECT_SCAN: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.overlayfs_direct_scan",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_OVERLAYFS_DISABLE_CACHE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.overlayfs_disable_cache",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_SCAN_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.scan_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SBOM_CONTAINER_IMAGE_SCAN_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.scan_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("600"),
};

pub const SBOM_CONTAINER_IMAGE_USE_MOUNT: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.use_mount",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_CONTAINER_IMAGE_USE_SPREAD_REFRESHER: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.container_image.use_spread_refresher",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'sbom.dd_url' — set an override in the annotation
pub const SBOM_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SBOM_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_HOST_ADDITIONAL_DIRECTORIES: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.host.additional_directories",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SBOM_HOST_ANALYZERS: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.host.analyzers",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const SBOM_HOST_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.host.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'sbom.logs_dd_url' — set an override in the annotation
pub const SBOM_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SBOM_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_SCAN_QUEUE_BASE_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.scan_queue.base_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"5m\""),
};

pub const SBOM_SCAN_QUEUE_MAX_BACKOFF: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.scan_queue.max_backoff",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"1h\""),
};

pub const SBOM_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SBOM_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SBOM_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const SBOM_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SBOM_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SBOM_SIMPLIFY_BOM_REFS: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.simplify_bom_refs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SBOM_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "sbom.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SCRUBBER_ADDITIONAL_KEYS: SchemaEntry = SchemaEntry {
    yaml_path: "scrubber.additional_keys",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SECRET_ALLOWED_K8S_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_allowed_k8s_namespace",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SECRET_AUDIT_FILE_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_audit_file_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1048576"),
};

pub const SECRET_BACKEND_ARGUMENTS: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_arguments",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SECRET_BACKEND_COMMAND: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_command",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SECRET_BACKEND_COMMAND_ALLOW_GROUP_EXEC_PERM: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_command_allow_group_exec_perm",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'secret_backend_config' — set an override in the annotation
pub const SECRET_BACKEND_CONFIG: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_config",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const SECRET_BACKEND_OUTPUT_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_output_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1048576"),
};

pub const SECRET_BACKEND_REMOVE_TRAILING_LINE_BREAK: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_remove_trailing_line_break",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECRET_BACKEND_SKIP_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_skip_checks",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECRET_BACKEND_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const SECRET_BACKEND_TYPE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_backend_type",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

// TODO: value_type unknown for 'secret_image_to_handle' — set an override in the annotation
pub const SECRET_IMAGE_TO_HANDLE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_image_to_handle",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const SECRET_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "secret_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SECRET_REFRESH_ON_API_KEY_FAILURE_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "secret_refresh_on_api_key_failure_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SECRET_REFRESH_SCATTER: SchemaEntry = SchemaEntry {
    yaml_path: "secret_refresh_scatter",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SECRET_SCOPE_INTEGRATION_TO_THEIR_K8S_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "secret_scope_integration_to_their_k8s_namespace",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECURITY_AGENT_CMD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.cmd_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5010"),
};

pub const SECURITY_AGENT_DISABLE_THP: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.disable_thp",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SECURITY_AGENT_EXPVAR_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.expvar_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5011"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_API_KEY: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.api_key",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_API_KEY", "DD_API_KEY"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_BLOCK_PROFILE_RATE: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.block_profile_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_CPU_DURATION: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.cpu_duration",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_CPU_DURATION"],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_DELTA_PROFILES: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.delta_profiles",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_ENABLE_BLOCK_PROFILING: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.enable_block_profiling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_ENABLE_GOROUTINE_STACKTRACES: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.enable_goroutine_stacktraces",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_ENABLE_MUTEX_PROFILING: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.enable_mutex_profiling",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.enabled",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_ENV: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.env",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_ENV", "DD_ENV"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_EXTRA_TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.extra_tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_MUTEX_PROFILE_FRACTION: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.mutex_profile_fraction",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_PERIOD: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.period",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_PERIOD"],
    value_type: ValueType::Float,
    default: Some("\"5m0s\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_PROFILE_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.profile_dd_url",
    env_vars: &[
        "DD_SECURITY_AGENT_INTERNAL_PROFILING_DD_URL",
        "DD_APM_INTERNAL_PROFILING_DD_URL",
    ],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_SITE: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.site",
    env_vars: &["DD_SECURITY_AGENT_INTERNAL_PROFILING_SITE", "DD_SITE"],
    value_type: ValueType::String,
    default: Some("\"datadoghq.com\""),
};

pub const SECURITY_AGENT_INTERNAL_PROFILING_UNIX_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.internal_profiling.unix_socket",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SECURITY_AGENT_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "security_agent.log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${log_path}/security-agent.log\""),
};

pub const SERIALIZER_COMPRESSOR_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_compressor_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_BETA_ROUTE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.series.beta_route",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/api/intake/metrics/v3beta/series\""),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.series.endpoints",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_USE_BETA: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.series.use_beta",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_VALIDATE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.series.validate",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.sketches.endpoints",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_VALIDATE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_experimental_use_v3_api.sketches.validate",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERIALIZER_MAX_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_payload_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2621440"),
};

pub const SERIALIZER_MAX_SERIES_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_series_payload_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("512000"),
};

pub const SERIALIZER_MAX_SERIES_POINTS_PER_PAYLOAD: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_series_points_per_payload",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10000"),
};

pub const SERIALIZER_MAX_SERIES_UNCOMPRESSED_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_series_uncompressed_payload_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5242880"),
};

pub const SERIALIZER_MAX_UNCOMPRESSED_PAYLOAD_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_uncompressed_payload_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4194304"),
};

pub const SERIALIZER_ZSTD_COMPRESSOR_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_zstd_compressor_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SERVER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "server_timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("30"),
};

pub const SERVERLESS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "serverless.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERVERLESS_LOGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "serverless.logs_enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SERVERLESS_SERVICE_MAPPING: SchemaEntry = SchemaEntry {
    yaml_path: "serverless.service_mapping",
    env_vars: &["DD_SERVICE_MAPPING"],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SERVERLESS_TRACE_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "serverless.trace_enabled",
    env_vars: &["DD_TRACE_ENABLED"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SERVERLESS_TRACE_MANAGED_SERVICES: SchemaEntry = SchemaEntry {
    yaml_path: "serverless.trace_managed_services",
    env_vars: &["DD_TRACE_MANAGED_SERVICES"],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'service_discovery.forwarder.additional_endpoints' — set an override in the annotation
pub const SERVICE_DISCOVERY_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SERVICE_DISCOVERY_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SERVICE_DISCOVERY_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const SERVICE_DISCOVERY_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const SERVICE_DISCOVERY_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SERVICE_DISCOVERY_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const SERVICE_DISCOVERY_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const SERVICE_DISCOVERY_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'service_discovery.forwarder.dd_url' — set an override in the annotation
pub const SERVICE_DISCOVERY_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SERVICE_DISCOVERY_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERVICE_DISCOVERY_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'service_discovery.forwarder.logs_dd_url' — set an override in the annotation
pub const SERVICE_DISCOVERY_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SERVICE_DISCOVERY_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERVICE_DISCOVERY_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SERVICE_DISCOVERY_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SERVICE_DISCOVERY_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const SERVICE_DISCOVERY_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SERVICE_DISCOVERY_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SERVICE_DISCOVERY_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SERVICE_DISCOVERY_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SERVICE_DISCOVERY_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "service_discovery.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SHARED_LIBRARY_CHECK_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "shared_library_check.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SHARED_LIBRARY_CHECK_LIBRARY_FOLDER_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "shared_library_check.library_folder_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${conf_path}/checks.d\""),
};

// TODO: value_type unknown for 'site' — set an override in the annotation
pub const SITE: SchemaEntry = SchemaEntry {
    yaml_path: "site",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SKIP_SSL_VALIDATION: SchemaEntry = SchemaEntry {
    yaml_path: "skip_ssl_validation",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SNMP_LISTENER_ALLOWED_FAILURES: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.allowed_failures",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const SNMP_LISTENER_COLLECT_DEVICE_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.collect_device_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SNMP_LISTENER_COLLECT_TOPOLOGY: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.collect_topology",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'snmp_listener.configs' — set an override in the annotation
pub const SNMP_LISTENER_CONFIGS: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.configs",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("[]"),
};

pub const SNMP_LISTENER_DISCOVERY_ALLOWED_FAILURES: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.discovery_allowed_failures",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const SNMP_LISTENER_DISCOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.discovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3600"),
};

pub const SNMP_LISTENER_LOADER: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.loader",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"core\""),
};

pub const SNMP_LISTENER_MIN_COLLECTION_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.min_collection_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("15"),
};

pub const SNMP_LISTENER_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"default\""),
};

pub const SNMP_LISTENER_OID_BATCH_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.oid_batch_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SNMP_LISTENER_PING_COUNT: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.ping.count",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SNMP_LISTENER_PING_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.ping.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SNMP_LISTENER_PING_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.ping.interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const SNMP_LISTENER_PING_LINUX_USE_RAW_SOCKET: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.ping.linux.use_raw_socket",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SNMP_LISTENER_PING_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.ping.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3000"),
};

pub const SNMP_LISTENER_RETRIES: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.retries",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("3"),
};

pub const SNMP_LISTENER_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.timeout",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SNMP_LISTENER_USE_DEVICE_ID_AS_HOSTNAME: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.use_device_id_as_hostname",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SNMP_LISTENER_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "snmp_listener.workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SOFTWARE_INVENTORY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

// TODO: value_type unknown for 'software_inventory.forwarder.additional_endpoints' — set an override in the annotation
pub const SOFTWARE_INVENTORY_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SOFTWARE_INVENTORY_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const SOFTWARE_INVENTORY_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'software_inventory.forwarder.dd_url' — set an override in the annotation
pub const SOFTWARE_INVENTORY_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SOFTWARE_INVENTORY_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'software_inventory.forwarder.logs_dd_url' — set an override in the annotation
pub const SOFTWARE_INVENTORY_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SOFTWARE_INVENTORY_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SOFTWARE_INVENTORY_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SOFTWARE_INVENTORY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("10"),
};

pub const SOFTWARE_INVENTORY_JITTER: SchemaEntry = SchemaEntry {
    yaml_path: "software_inventory.jitter",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const SSLKEYLOGFILE: SchemaEntry = SchemaEntry {
    yaml_path: "sslkeylogfile",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const STATSD_FORWARD_HOST: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_forward_host",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const STATSD_FORWARD_PORT: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_forward_port",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const STATSD_METRIC_BLOCKLIST: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_metric_blocklist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const STATSD_METRIC_BLOCKLIST_MATCH_PREFIX: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_metric_blocklist_match_prefix",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const STATSD_METRIC_NAMESPACE: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_metric_namespace",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const STATSD_METRIC_NAMESPACE_BLACKLIST: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_metric_namespace_blacklist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

pub const SYNTHETICS_COLLECTOR_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.collector.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SYNTHETICS_COLLECTOR_FLUSH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.collector.flush_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"10s\""),
};

pub const SYNTHETICS_COLLECTOR_WORKERS: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.collector.workers",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("4"),
};

// TODO: value_type unknown for 'synthetics.forwarder.additional_endpoints' — set an override in the annotation
pub const SYNTHETICS_FORWARDER_ADDITIONAL_ENDPOINTS: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.additional_endpoints",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SYNTHETICS_FORWARDER_BATCH_MAX_CONCURRENT_SEND: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.batch_max_concurrent_send",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

pub const SYNTHETICS_FORWARDER_BATCH_MAX_CONTENT_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.batch_max_content_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5000000"),
};

pub const SYNTHETICS_FORWARDER_BATCH_MAX_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.batch_max_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1000"),
};

pub const SYNTHETICS_FORWARDER_BATCH_WAIT: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.batch_wait",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("5"),
};

pub const SYNTHETICS_FORWARDER_COMPRESSION_KIND: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.compression_kind",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"zstd\""),
};

pub const SYNTHETICS_FORWARDER_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("6"),
};

pub const SYNTHETICS_FORWARDER_CONNECTION_RESET_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.connection_reset_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("0"),
};

// TODO: value_type unknown for 'synthetics.forwarder.dd_url' — set an override in the annotation
pub const SYNTHETICS_FORWARDER_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SYNTHETICS_FORWARDER_DEV_MODE_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.dev_mode_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SYNTHETICS_FORWARDER_INPUT_CHAN_SIZE: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.input_chan_size",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("100"),
};

// TODO: value_type unknown for 'synthetics.forwarder.logs_dd_url' — set an override in the annotation
pub const SYNTHETICS_FORWARDER_LOGS_DD_URL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.logs_dd_url",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const SYNTHETICS_FORWARDER_LOGS_NO_SSL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.logs_no_ssl",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SYNTHETICS_FORWARDER_SENDER_BACKOFF_BASE: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.sender_backoff_base",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SYNTHETICS_FORWARDER_SENDER_BACKOFF_FACTOR: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.sender_backoff_factor",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SYNTHETICS_FORWARDER_SENDER_BACKOFF_MAX: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.sender_backoff_max",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("120"),
};

pub const SYNTHETICS_FORWARDER_SENDER_RECOVERY_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.sender_recovery_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("2"),
};

pub const SYNTHETICS_FORWARDER_SENDER_RECOVERY_RESET: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.sender_recovery_reset",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SYNTHETICS_FORWARDER_USE_COMPRESSION: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.use_compression",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SYNTHETICS_FORWARDER_USE_V2_API: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.use_v2_api",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const SYNTHETICS_FORWARDER_ZSTD_COMPRESSION_LEVEL: SchemaEntry = SchemaEntry {
    yaml_path: "synthetics.forwarder.zstd_compression_level",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("1"),
};

pub const SYSLOG_RFC: SchemaEntry = SchemaEntry {
    yaml_path: "syslog_rfc",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const SYSLOG_URI: SchemaEntry = SchemaEntry {
    yaml_path: "syslog_uri",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const SYSTEM_TRAY_LOG_FILE: SchemaEntry = SchemaEntry {
    yaml_path: "system_tray.log_file",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"${conf_path}/logs/ddtray.log\""),
};

// TODO: value_type unknown for 'tag_value_split_separator' — set an override in the annotation
pub const TAG_VALUE_SPLIT_SEPARATOR: SchemaEntry = SchemaEntry {
    yaml_path: "tag_value_split_separator",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("{}"),
};

pub const TAGS: SchemaEntry = SchemaEntry {
    yaml_path: "tags",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

// TODO: value_type unknown for 'telemetry.checks' — set an override in the annotation
pub const TELEMETRY_CHECKS: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.checks",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const TELEMETRY_DOGSTATSD_AGGREGATOR_CHANNEL_LATENCY_BUCKETS: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.dogstatsd.aggregator_channel_latency_buckets",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const TELEMETRY_DOGSTATSD_LISTENERS_CHANNEL_LATENCY_BUCKETS: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.dogstatsd.listeners_channel_latency_buckets",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const TELEMETRY_DOGSTATSD_LISTENERS_LATENCY_BUCKETS: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.dogstatsd.listeners_latency_buckets",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: Some("[]"),
};

pub const TELEMETRY_DOGSTATSD_ORIGIN: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.dogstatsd_origin",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const TELEMETRY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const TELEMETRY_PYTHON_MEMORY: SchemaEntry = SchemaEntry {
    yaml_path: "telemetry.python_memory",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

// TODO: value_type unknown for 'tls_handshake_timeout' — set an override in the annotation
pub const TLS_HANDSHAKE_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "tls_handshake_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

pub const TRACE_AGENT_HOST_SOCKET_PATH: SchemaEntry = SchemaEntry {
    yaml_path: "trace_agent_host_socket_path",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"/var/run/datadog\""),
};

pub const TRACEMALLOC_BLACKLIST: SchemaEntry = SchemaEntry {
    yaml_path: "tracemalloc_blacklist",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const TRACEMALLOC_DEBUG: SchemaEntry = SchemaEntry {
    yaml_path: "tracemalloc_debug",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const TRACEMALLOC_EXCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "tracemalloc_exclude",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const TRACEMALLOC_INCLUDE: SchemaEntry = SchemaEntry {
    yaml_path: "tracemalloc_include",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const TRACEMALLOC_WHITELIST: SchemaEntry = SchemaEntry {
    yaml_path: "tracemalloc_whitelist",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const USE_DISKV2_CHECK: SchemaEntry = SchemaEntry {
    yaml_path: "use_diskv2_check",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const USE_DOGSTATSD: SchemaEntry = SchemaEntry {
    yaml_path: "use_dogstatsd",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const USE_IMPROVED_CGROUP_PARSER: SchemaEntry = SchemaEntry {
    yaml_path: "use_improved_cgroup_parser",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const USE_NETWORKV2_CHECK: SchemaEntry = SchemaEntry {
    yaml_path: "use_networkv2_check",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

pub const USE_PROXY_FOR_CLOUD_METADATA: SchemaEntry = SchemaEntry {
    yaml_path: "use_proxy_for_cloud_metadata",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const USE_V2_API_SERIES: SchemaEntry = SchemaEntry {
    yaml_path: "use_v2_api.series",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

pub const VECTOR_LOGS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "vector.logs.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const VECTOR_LOGS_URL: SchemaEntry = SchemaEntry {
    yaml_path: "vector.logs.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const VECTOR_METRICS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "vector.metrics.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const VECTOR_METRICS_URL: SchemaEntry = SchemaEntry {
    yaml_path: "vector.metrics.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const VECTOR_TRACES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "vector.traces.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const VECTOR_TRACES_URL: SchemaEntry = SchemaEntry {
    yaml_path: "vector.traces.url",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const VSOCK_ADDR: SchemaEntry = SchemaEntry {
    yaml_path: "vsock_addr",
    env_vars: &[],
    value_type: ValueType::String,
    default: Some("\"\""),
};

pub const WIN_SKIP_COM_INIT: SchemaEntry = SchemaEntry {
    yaml_path: "win_skip_com_init",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const WINDOWS_COUNTER_INIT_FAILURE_LIMIT: SchemaEntry = SchemaEntry {
    yaml_path: "windows_counter_init_failure_limit",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("20"),
};

pub const WINDOWS_COUNTER_REFRESH_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "windows_counter_refresh_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("60"),
};

pub const WINDOWS_USE_PYTHONPATH: SchemaEntry = SchemaEntry {
    yaml_path: "windows_use_pythonpath",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("false"),
};

pub const WORKLOADMETA_LOCAL_PROCESS_COLLECTOR_COLLECTION_INTERVAL: SchemaEntry = SchemaEntry {
    yaml_path: "workloadmeta.local_process_collector.collection_interval",
    env_vars: &[],
    value_type: ValueType::Float,
    default: Some("\"1m0s\""),
};

pub const WORKLOADMETA_REMOTE_RECV_WITHOUT_TIMEOUT: SchemaEntry = SchemaEntry {
    yaml_path: "workloadmeta.remote.recv_without_timeout",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

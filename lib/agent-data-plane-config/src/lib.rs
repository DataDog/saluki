//! ADP-native configuration model: the typed output of configuration translation.
//!
//! Source-language loading and translation live in `agent-data-plane-config-system`. This crate
//! contains only the native runtime model, bootstrap views, Saluki-only seed input, authority
//! markers, and serialized `/config` view wrappers.

use std::sync::{Arc, RwLock};

use saluki_component_config::{
    AggregateConfig, ApmStatsEncoderConfig, ApmStatsTransformConfig, ChecksIpcConfig, DatadogEventsEncoderConfig,
    DatadogForwarderConfig, DatadogLogsEncoderConfig, DatadogMetricsEncoderConfig, DatadogServiceChecksEncoderConfig,
    DatadogTraceEncoderConfig, DogStatsDConfig, DogStatsDDebugLogConfig, DogStatsDMapperConfig,
    DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig, DogStatsDStatisticsConfig, HostEnrichmentConfig,
    HostTagsConfig, ListenAddress, MrfConfig, OtlpConfig, OtlpDecoderConfig, OtlpForwarderConfig, OtlpRelayConfig,
    OttlFilterConfig, OttlTransformConfig, TagFilterlistConfig, TraceObfuscationConfig, TraceSamplerConfig,
    WorkloadConfig,
};
use serde::{Deserialize, Serialize};

/// Complete ADP-native runtime configuration after translation.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct SalukiConfiguration {
    /// Orchestration-only topology and pipeline decisions.
    pub control: ControlConfiguration,
    /// Component-native runtime configuration grouped by ownership domain.
    pub components: ComponentConfiguration,
}

/// Orchestration-only configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ControlConfiguration {
    /// Whether the data plane should run.
    pub enabled: bool,
    /// Global shutdown timeout in milliseconds.
    pub stop_timeout_millis: u64,
    /// DogStatsD pipeline gate.
    pub dogstatsd: PipelineGate,
    /// Checks pipeline gate.
    pub checks: PipelineGate,
    /// OTLP pipeline gate.
    pub otlp: OtlpPipelineGate,
    /// Local API listen address.
    pub api_listen_address: ListenAddress,
    /// Secure local API listen address.
    pub secure_api_listen_address: ListenAddress,
    /// Whether remote-agent mode is enabled.
    pub remote_agent_enabled: bool,
    /// Whether to use the new config-stream endpoint.
    pub use_new_config_stream_endpoint: bool,
    /// Whether the process runs in standalone mode.
    pub standalone_mode: bool,
    /// Runtime log level supplied by the active Datadog authority.
    pub log_level: Option<String>,
    /// IPC authentication and TLS file paths.
    pub ipc_auth: ControlIpcAuthConfiguration,
    /// Agent IPC maximum gRPC message size.
    pub agent_ipc_grpc_max_message_size: Option<usize>,
    /// IPC command port.
    pub cmd_port: Option<u16>,
    /// Data plane log file path.
    pub data_plane_log_file: Option<String>,
    /// Whether log timestamps use RFC 3339 format.
    pub log_format_rfc3339: Option<bool>,
    /// Whether syslog uses RFC formatting.
    pub syslog_rfc: Option<bool>,
    /// Syslog URI.
    pub syslog_uri: Option<String>,
    /// Default deployment environment name.
    pub env: Option<String>,
    /// Vsock address for guest/host transports.
    pub vsock_addr: Option<String>,
}

impl ControlConfiguration {
    /// Returns true when at least one pipeline needs the Datadog forwarder.
    pub fn requires_datadog_forwarder(&self) -> bool {
        self.dogstatsd.enabled()
            || self.checks.enabled()
            || self.otlp.metrics.enabled()
            || self.otlp.logs.enabled()
            || self.otlp.traces.enabled()
    }

    /// Returns true when any data pipeline is enabled.
    pub fn data_pipelines_enabled(&self) -> bool {
        self.requires_datadog_forwarder() || self.otlp.proxy.enabled
    }
}

impl Default for ControlConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            stop_timeout_millis: 4_000,
            dogstatsd: PipelineGate::on(),
            checks: PipelineGate::off(),
            otlp: OtlpPipelineGate::default(),
            api_listen_address: ListenAddress::Tcp("127.0.0.1:5000".to_string()),
            secure_api_listen_address: ListenAddress::Tcp("127.0.0.1:5010".to_string()),
            remote_agent_enabled: true,
            use_new_config_stream_endpoint: false,
            standalone_mode: false,
            log_level: None,
            ipc_auth: ControlIpcAuthConfiguration::default(),
            agent_ipc_grpc_max_message_size: None,
            cmd_port: None,
            data_plane_log_file: None,
            log_format_rfc3339: None,
            syslog_rfc: None,
            syslog_uri: None,
            env: None,
            vsock_addr: None,
        }
    }
}

/// IPC authentication and TLS file paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ControlIpcAuthConfiguration {
    /// Agent authentication token file path.
    pub auth_token_file_path: Option<String>,
    /// Agent IPC certificate file path.
    pub ipc_cert_file_path: Option<String>,
}

/// Static gate for a pipeline or sub-pipeline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PipelineGate {
    /// Whether the pipeline is built.
    pub enabled: bool,
}

impl PipelineGate {
    /// Creates an enabled gate.
    pub const fn on() -> Self {
        Self { enabled: true }
    }

    /// Creates a disabled gate.
    pub const fn off() -> Self {
        Self { enabled: false }
    }

    /// Returns whether the gate is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

impl Default for PipelineGate {
    fn default() -> Self {
        Self::off()
    }
}

/// OTLP topology gates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OtlpPipelineGate {
    /// Native OTLP source gate.
    pub native: PipelineGate,
    /// OTLP proxy-mode gate.
    pub proxy: OtlpProxyGate,
    /// Whether OTLP metrics are accepted.
    pub metrics: PipelineGate,
    /// Whether OTLP logs are accepted.
    pub logs: PipelineGate,
    /// Whether OTLP traces are accepted.
    pub traces: PipelineGate,
}

impl Default for OtlpPipelineGate {
    fn default() -> Self {
        Self {
            native: PipelineGate::off(),
            proxy: OtlpProxyGate::default(),
            metrics: PipelineGate::on(),
            logs: PipelineGate::on(),
            traces: PipelineGate::on(),
        }
    }
}

/// OTLP proxy-mode gate and target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OtlpProxyGate {
    /// Whether OTLP proxy mode is enabled.
    pub enabled: bool,
    /// Core-agent OTLP gRPC endpoint.
    pub core_agent_otlp_grpc_endpoint: String,
}

/// Component-native runtime configuration grouped by ownership domain.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct ComponentConfiguration {
    /// Forwarder components.
    pub forwarder: ForwarderConfigs,
    /// Metrics pipeline components.
    pub metrics: MetricsConfigs,
    /// Logs pipeline components.
    pub logs: LogsConfigs,
    /// Events pipeline components.
    pub events: EventsConfigs,
    /// Service-check pipeline components.
    pub service_checks: ServiceChecksConfigs,
    /// Trace pipeline components.
    pub traces: TracesConfigs,
    /// Check ingestion components.
    pub checks: ChecksConfigs,
    /// DogStatsD components.
    pub dogstatsd: DogStatsDConfigs,
    /// OTLP components.
    pub otlp: OtlpConfigs,
    /// Workload metadata components.
    pub workload: WorkloadConfigs,
}

/// Forwarder component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct ForwarderConfigs {
    /// Datadog forwarder config.
    pub datadog: DatadogForwarderConfig,
}

/// Metrics component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct MetricsConfigs {
    /// Datadog metrics encoder config.
    pub datadog_encoder: DatadogMetricsEncoderConfig,
    /// APM stats encoder config.
    pub apm_stats_encoder: ApmStatsEncoderConfig,
    /// APM stats transform config.
    pub apm_stats_transform: ApmStatsTransformConfig,
    /// Multi-region failover config.
    pub multi_region_failover: MrfConfig,
}

/// Logs component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct LogsConfigs {
    /// Datadog logs encoder config.
    pub datadog_encoder: DatadogLogsEncoderConfig,
}

/// Events component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct EventsConfigs {
    /// Datadog events encoder config.
    pub datadog_encoder: DatadogEventsEncoderConfig,
}

/// Service-check component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct ServiceChecksConfigs {
    /// Datadog service-check encoder config.
    pub datadog_encoder: DatadogServiceChecksEncoderConfig,
}

/// Trace component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct TracesConfigs {
    /// Datadog trace encoder config.
    pub encoder: DatadogTraceEncoderConfig,
    /// Trace sampler config.
    pub sampler: TraceSamplerConfig,
    /// Trace obfuscation config.
    pub obfuscation: TraceObfuscationConfig,
    /// OTTL filter config.
    pub ottl_filter: OttlFilterConfig,
    /// OTTL transform config.
    pub ottl_transform: OttlTransformConfig,
}

/// Check ingestion component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct ChecksConfigs {
    /// Checks IPC source config.
    pub ipc: ChecksIpcConfig,
}

/// DogStatsD component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct DogStatsDConfigs {
    /// DogStatsD source config.
    pub source: DogStatsDConfig,
    /// Prefix filter config.
    pub prefix_filter: DogStatsDPrefixFilterConfig,
    /// Mapper config.
    pub mapper: DogStatsDMapperConfig,
    /// Tag filterlist config.
    pub tag_filterlist: TagFilterlistConfig,
    /// Aggregate transform config.
    pub aggregate: AggregateConfig,
    /// Post-aggregate filter config.
    pub post_aggregate_filter: DogStatsDPostAggregateFilterConfig,
    /// Debug log config.
    pub debug_log: DogStatsDDebugLogConfig,
    /// DogStatsD statistics config.
    pub statistics: DogStatsDStatisticsConfig,
}

/// OTLP component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct OtlpConfigs {
    /// Native OTLP source config.
    pub source: OtlpConfig,
    /// OTLP proxy relay config.
    pub relay: OtlpRelayConfig,
    /// OTLP decoder config.
    pub decoder: OtlpDecoderConfig,
    /// OTLP forwarder config.
    pub forwarder: OtlpForwarderConfig,
}

/// Workload metadata component configs.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct WorkloadConfigs {
    /// Workload collector config.
    pub source: WorkloadConfig,
    /// Host tags config.
    pub host_tags: HostTagsConfig,
    /// Host enrichment config.
    pub host_enrichment: HostEnrichmentConfig,
}

/// Pre-runtime typed bootstrap slice.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct BootstrapConfiguration {
    /// Datadog source bootstrap values.
    pub datadog: DatadogBootstrap,
    /// Saluki-only bootstrap values.
    pub saluki: SalukiBootstrap,
}

/// Datadog bootstrap values read from local Datadog sources.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct DatadogBootstrap {
    /// Log level used before runtime authority starts.
    pub log_level: Option<String>,
    /// Metrics level used before runtime authority starts.
    pub metrics_level: Option<String>,
    /// Whether bootstrap logs are emitted as JSON.
    pub log_format_json: Option<bool>,
    /// Whether bootstrap log timestamps use RFC 3339 format.
    pub log_format_rfc3339: Option<bool>,
    /// Whether bootstrap logs are emitted to the console.
    pub log_to_console: Option<bool>,
    /// Whether bootstrap logs are emitted to syslog.
    pub log_to_syslog: Option<bool>,
    /// Whether syslog uses RFC formatting.
    pub syslog_rfc: Option<bool>,
    /// Syslog URI.
    pub syslog_uri: Option<String>,
    /// Maximum log file size, in bytes.
    pub log_file_max_size_bytes: Option<u64>,
    /// Number of rotated log files to keep.
    pub log_file_max_rolls: Option<usize>,
    /// Whether file logging is disabled.
    pub disable_file_logging: Option<bool>,
    /// ADP-specific log file path.
    pub data_plane_log_file: Option<String>,
    /// IPC command port.
    pub cmd_port: Option<u16>,
    /// IPC auth token path.
    pub auth_token_file_path: Option<String>,
    /// IPC certificate path.
    pub ipc_cert_file_path: Option<String>,
}

/// Saluki-only bootstrap values read from local Saluki sources.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct SalukiBootstrap {
    /// Optional path to a Saluki-only config file.
    pub config_path: Option<String>,
}

/// Saluki-schema-only source input.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SalukiOnlyConfiguration {
    /// Control-plane Saluki-only values.
    pub control: ControlSalukiOnly,
    /// OTLP Saluki-only values.
    pub otlp: OtlpSalukiOnly,
    /// DogStatsD Saluki-only values.
    pub dogstatsd: DogStatsDSalukiOnly,
    /// Workload Saluki-only values.
    pub workload: WorkloadSalukiOnly,
}

impl SalukiOnlyConfiguration {
    /// Seeds the native model with defaults and Saluki-only values.
    pub fn seed(&self) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        self.apply_runtime_overrides(&mut config);
        config.components.otlp.source.string_interner_size = self.otlp.string_interner_size;
        config.components.otlp.source.cached_contexts_limit = self.otlp.cached_contexts_limit;
        config.components.dogstatsd.source.context_string_interner_size_bytes =
            self.dogstatsd.string_interner_size_bytes;
        config.components.dogstatsd.source.cached_contexts_limit = self.dogstatsd.cached_contexts_limit;
        config.components.workload.source.enabled = self.workload.enabled;
        config
    }

    /// Reapplies Saluki-only values that intentionally override Datadog-derived fields.
    pub fn apply_runtime_overrides(&self, config: &mut SalukiConfiguration) {
        config.control.standalone_mode = self.control.standalone_mode;
        config.control.checks.enabled = self.control.checks_enabled;
        if let Some(stop_timeout_secs) = self.control.stop_timeout_secs {
            config.control.stop_timeout_millis = stop_timeout_secs.saturating_mul(1000);
        }
    }
}

/// Saluki-only control-plane values.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ControlSalukiOnly {
    /// Whether the process runs without remote Agent attachment.
    pub standalone_mode: bool,
    /// Whether the checks pipeline is enabled.
    pub checks_enabled: bool,
    /// Direct graceful shutdown timeout, in seconds.
    pub stop_timeout_secs: Option<u64>,
}

/// Saluki-only OTLP values.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct OtlpSalukiOnly {
    /// String interner size.
    pub string_interner_size: usize,
    /// Cached contexts limit.
    pub cached_contexts_limit: usize,
}

impl Default for OtlpSalukiOnly {
    fn default() -> Self {
        Self {
            string_interner_size: 32_768,
            cached_contexts_limit: 500_000,
        }
    }
}

/// Saluki-only DogStatsD values.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DogStatsDSalukiOnly {
    /// Context interner size in bytes.
    pub string_interner_size_bytes: u64,
    /// Cached contexts limit.
    pub cached_contexts_limit: usize,
}

impl Default for DogStatsDSalukiOnly {
    fn default() -> Self {
        Self {
            string_interner_size_bytes: 2 * 1024 * 1024,
            cached_contexts_limit: 500_000,
        }
    }
}

/// Saluki-only workload values.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkloadSalukiOnly {
    /// Whether workload collection is enabled.
    pub enabled: bool,
}

impl Default for WorkloadSalukiOnly {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Datadog runtime authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DatadogRuntimeAuthority {
    /// Local sources are authoritative for runtime.
    Local,
    /// The Datadog Agent config stream is authoritative for runtime.
    Stream,
}

/// Bundle of source and internal config views.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ConfigViews {
    /// Source-shaped Datadog compatibility view.
    pub raw: SourceConfigView,
    /// ADP-native internal view.
    pub internal: InternalConfigView,
}

/// Serialized source-shaped config view.
#[derive(Clone, Debug, Default)]
pub struct SourceConfigView {
    json: Arc<RwLock<serde_json::Value>>,
}

impl SourceConfigView {
    /// Creates a source view from scrubbed JSON.
    pub fn new(json: serde_json::Value) -> Self {
        Self {
            json: Arc::new(RwLock::new(json)),
        }
    }

    /// Replaces the serialized JSON value.
    pub fn update(&self, json: serde_json::Value) {
        *self.json.write().unwrap_or_else(|error| error.into_inner()) = json;
    }

    /// Returns the current serialized JSON value.
    pub fn as_json(&self) -> serde_json::Value {
        self.json.read().unwrap_or_else(|error| error.into_inner()).clone()
    }
}

impl PartialEq for SourceConfigView {
    fn eq(&self, other: &Self) -> bool {
        self.as_json() == other.as_json()
    }
}

impl Eq for SourceConfigView {}

impl Serialize for SourceConfigView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_json().serialize(serializer)
    }
}

/// Serialized internal native config view.
#[derive(Clone, Debug, Default)]
pub struct InternalConfigView {
    json: Arc<RwLock<serde_json::Value>>,
}

impl InternalConfigView {
    /// Creates an internal view from scrubbed JSON.
    pub fn new(json: serde_json::Value) -> Self {
        Self {
            json: Arc::new(RwLock::new(json)),
        }
    }

    /// Replaces the serialized JSON value.
    pub fn update(&self, json: serde_json::Value) {
        *self.json.write().unwrap_or_else(|error| error.into_inner()) = json;
    }

    /// Returns the current serialized JSON value.
    pub fn as_json(&self) -> serde_json::Value {
        self.json.read().unwrap_or_else(|error| error.into_inner()).clone()
    }
}

impl PartialEq for InternalConfigView {
    fn eq(&self, other: &Self) -> bool {
        self.as_json() == other.as_json()
    }
}

impl Eq for InternalConfigView {}

impl Serialize for InternalConfigView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_json().serialize(serializer)
    }
}

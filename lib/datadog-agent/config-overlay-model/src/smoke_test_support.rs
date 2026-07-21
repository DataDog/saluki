//! Types necessary to keep the configuration smoke tests working. Intended to be transitional
//! until we can get Saluki/ADP to stop deserializing directly from Agent configuration. At that
//! time the smoke tests will be rendered inert and can be removed.
use serde::{Deserialize, Serialize};

/// A configuration consumer tracked by the legacy smoke-test registry.
///
/// Most variants name structs that consume Agent configuration directly. Sentinel variants track
/// consumers that do not fit the legacy deserialization test model.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
pub enum ConfigurationStruct {
    AggregateConfiguration,
    ContainerdConfiguration,
    DatadogApmStatsEncoderConfiguration,
    DatadogEventsConfiguration,
    DatadogLogsConfiguration,
    DatadogMetricsConfiguration,
    DatadogServiceChecksConfiguration,
    DatadogTraceConfiguration,
    DogStatsDConfiguration,
    DogStatsDDebugLogConfiguration,
    DogStatsDMapperConfiguration,
    DogStatsDPrefixFilterConfiguration,
    ForwarderConfiguration,
    MrfConfiguration,
    OtlpDecoderConfiguration,
    OtlpRelayConfiguration,
    ProxyConfiguration,
    RemoteAgentClientConfiguration,
    TagFilterlistConfiguration,
    TraceObfuscationConfiguration,

    /// Keys consumed through the typed configuration translation system.
    TypedConfigSystem,

    /// Keys read via `get_typed` / `try_get_typed` rather than struct deserialization.
    #[serde(rename = "get_typed")]
    GetTyped,

    /// Sentinel for supported keys that are being promoted during the transition to
    /// typed-configuration access. These were missing from our inventory when config smoke
    /// testing was introduced and for various reasons may not be legitimate targets for that
    /// testing modality. Regardless, these fields will be part of the typed translation system
    /// and this sentinel can be removed when our testing model moves over to that system.
    #[serde(rename = "NO_SMOKE")]
    NoSmoke,
}

impl ConfigurationStruct {
    /// Apparently we travel back and forth between string constant representation and the struct
    /// names themselves. This function recovers the string constant name for a struct.
    pub fn as_smoke_test_const(&self) -> &'static str {
        match self {
            ConfigurationStruct::AggregateConfiguration => "AGGREGATE_CONFIGURATION",
            ConfigurationStruct::ContainerdConfiguration => "CONTAINERD_CONFIGURATION",
            ConfigurationStruct::DatadogApmStatsEncoderConfiguration => "DATADOG_APM_STATS_ENCODER_CONFIGURATION",
            ConfigurationStruct::DatadogEventsConfiguration => "DATADOG_EVENTS_CONFIGURATION",
            ConfigurationStruct::DatadogLogsConfiguration => "DATADOG_LOGS_CONFIGURATION",
            ConfigurationStruct::DatadogMetricsConfiguration => "DATADOG_METRICS_CONFIGURATION",
            ConfigurationStruct::DatadogServiceChecksConfiguration => "DATADOG_SERVICE_CHECKS_CONFIGURATION",
            ConfigurationStruct::DatadogTraceConfiguration => "DATADOG_TRACE_CONFIGURATION",
            ConfigurationStruct::DogStatsDConfiguration => "DOGSTATSD_CONFIGURATION",
            ConfigurationStruct::DogStatsDDebugLogConfiguration => "DOGSTATSD_DEBUG_LOG_CONFIGURATION",
            ConfigurationStruct::DogStatsDMapperConfiguration => "DOGSTATSD_MAPPER_CONFIGURATION",
            ConfigurationStruct::DogStatsDPrefixFilterConfiguration => "DOGSTATSD_PREFIX_FILTER_CONFIGURATION",
            ConfigurationStruct::ForwarderConfiguration => "FORWARDER_CONFIGURATION",
            ConfigurationStruct::MrfConfiguration => "MRF_CONFIGURATION",
            ConfigurationStruct::OtlpDecoderConfiguration => "OTLP_DECODER_CONFIGURATION",
            ConfigurationStruct::OtlpRelayConfiguration => "OTLP_RELAY_CONFIGURATION",
            ConfigurationStruct::ProxyConfiguration => "PROXY_CONFIGURATION",
            ConfigurationStruct::RemoteAgentClientConfiguration => "REMOTE_AGENT_CLIENT_CONFIGURATION",
            ConfigurationStruct::TagFilterlistConfiguration => "TAG_FILTERLIST_CONFIGURATION",
            ConfigurationStruct::TraceObfuscationConfiguration => "TRACE_OBFUSCATION_CONFIGURATION",
            ConfigurationStruct::TypedConfigSystem => "TYPED_CONFIG_SYSTEM",
            ConfigurationStruct::GetTyped => "GET_TYPED",
            ConfigurationStruct::NoSmoke => "NO_SMOKE",
        }
    }
}

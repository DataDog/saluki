//! Types necessary to keep the configuration smoke tests working. Intended to be transitional
//! until we can get Saluki/ADP to stop deserializing directly from Agent configuration. At that
//! time the smoke tests will be rendered inert and can be removed.
use serde::{Deserialize, Serialize};

/// A struct in the Saluki repository that deserializes Datadog Agent configuration.
///
/// Used as values in `schema_overlay.yaml` `used_by` fields to declare which structs consume a
/// given key. Adding a new struct here is the first step when registering its configuration keys.
///
/// This construct has been carried over from the original `config_registry` in order to support
/// configuration smoke test code generation. Each variant in this enum should be an exact name
/// match to a struct that consumes its value directly from Agent configuration either by
/// deserializing or by environment variable.
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
    OtlpConfiguration,
    OtlpDecoderConfiguration,
    OtlpRelayConfiguration,
    ProxyConfiguration,
    RemoteAgentClientConfiguration,
    TagFilterlistConfiguration,
    TraceObfuscationConfiguration,

    /// Keys read via `get_typed` / `try_get_typed` rather than struct deserialization.
    #[serde(rename = "get_typed")]
    GetTyped,
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
            ConfigurationStruct::OtlpConfiguration => "OTLP_CONFIGURATION",
            ConfigurationStruct::OtlpDecoderConfiguration => "OTLP_DECODER_CONFIGURATION",
            ConfigurationStruct::OtlpRelayConfiguration => "OTLP_RELAY_CONFIGURATION",
            ConfigurationStruct::ProxyConfiguration => "PROXY_CONFIGURATION",
            ConfigurationStruct::RemoteAgentClientConfiguration => "REMOTE_AGENT_CLIENT_CONFIGURATION",
            ConfigurationStruct::TagFilterlistConfiguration => "TAG_FILTERLIST_CONFIGURATION",
            ConfigurationStruct::TraceObfuscationConfiguration => "TRACE_OBFUSCATION_CONFIGURATION",
            ConfigurationStruct::GetTyped => "GET_TYPED",
        }
    }
}

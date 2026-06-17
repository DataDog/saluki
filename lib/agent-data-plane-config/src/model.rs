//! The ADP-native runtime configuration model: [`SalukiConfiguration`] and its component groups.
//!
//! [`SalukiConfiguration`] is the single output of configuration translation. It has two top-level
//! groups: [`ControlConfiguration`] (pipeline gates and topology shaping, read only by config-system
//! and the topology builder) and [`ComponentConfiguration`] (the per-domain component groups, each
//! read by its owning component).
//!
//! Every group wrapper embeds the `saluki-component-config` leaf structs directly. There is no
//! duplicate ADP-slice model and no `From` conversion layer: the translator writes witnessed values
//! straight into these embedded leaf structs.

use saluki_component_config::{
    checks, dogstatsd, events, forwarder, logs, metrics, otlp, service_checks, traces, workload,
};

use crate::control::ControlConfiguration;

/// The complete ADP-native runtime configuration after translation.
///
/// This is the single output model that all ADP runtime code consumes. It is produced by seeding a
/// base from [`SalukiOnlyConfiguration::seed`](crate::SalukiOnlyConfiguration::seed) and then
/// driving the Datadog witness over it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SalukiConfiguration {
    /// Read first: pipeline gates and topology-shaping decisions. Consumed by the orchestration
    /// layer (config-system and the topology builder) only, never by components.
    pub control: ControlConfiguration,

    /// Read second: the per-domain component configuration groups. Each component receives only its
    /// own slice from here.
    pub components: ComponentConfiguration,
}

/// The per-domain component configuration groups.
///
/// Each field is a group wrapper that embeds the `saluki-component-config` leaf structs for one
/// ownership domain. A component is handed only its own leaf slice (for example,
/// `&components.forwarder.datadog`), never the whole `ComponentConfiguration` or
/// [`SalukiConfiguration`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ComponentConfiguration {
    /// Outbound Datadog forwarder configuration.
    pub forwarder: ForwarderConfigs,

    /// Metrics encoder and metrics-adjacent encoder/transform configuration.
    pub metrics: MetricsConfigs,

    /// Logs encoder configuration.
    pub logs: LogsConfigs,

    /// Events encoder configuration.
    pub events: EventsConfigs,

    /// Service checks encoder configuration.
    pub service_checks: ServiceChecksConfigs,

    /// Traces encoder, sampler, and obfuscation configuration.
    pub traces: TracesConfigs,

    /// Checks IPC source configuration.
    pub checks: ChecksConfigs,

    /// DogStatsD source, mapper, aggregate, debug-log, and filter configuration.
    pub dogstatsd: DogStatsDConfigs,

    /// OTLP source, relay, decoder, and forwarder configuration.
    pub otlp: OtlpConfigs,

    /// Workload/environment collection configuration.
    pub workload: WorkloadConfigs,
}

/// Forwarder-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ForwarderConfigs {
    /// Datadog forwarder configuration (endpoints, TLS, retry, concurrency).
    pub datadog: forwarder::DatadogForwarderConfig,
}

/// Metrics-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MetricsConfigs {
    /// Datadog metrics encoder configuration.
    pub datadog_encoder: metrics::DatadogMetricsConfig,

    /// APM stats encoder configuration.
    pub apm_stats_encoder: traces::DatadogApmStatsEncoderConfig,

    /// APM stats transform configuration.
    pub apm_stats_transform: traces::ApmStatsTransformConfig,

    /// Multi-region failover configuration.
    pub multi_region_failover: forwarder::MrfConfig,
}

/// Logs-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LogsConfigs {
    /// Datadog logs encoder configuration.
    pub encoder: logs::DatadogLogsConfig,
}

/// Events-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct EventsConfigs {
    /// Datadog events encoder configuration.
    pub encoder: events::DatadogEventsConfig,
}

/// Service-checks-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ServiceChecksConfigs {
    /// Datadog service checks encoder configuration.
    pub encoder: service_checks::DatadogServiceChecksConfig,
}

/// Traces-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct TracesConfigs {
    /// Datadog traces encoder configuration.
    pub encoder: traces::DatadogTraceConfig,

    /// Trace sampler configuration.
    pub sampler: traces::TraceSamplerConfig,

    /// Trace obfuscation configuration.
    pub obfuscation: traces::TraceObfuscationConfig,
}

/// Checks-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ChecksConfigs {
    /// Checks IPC source configuration.
    pub ipc: checks::ChecksIPCConfig,
}

/// DogStatsD-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DogStatsDConfigs {
    /// DogStatsD source configuration (listeners, parser/decoding options).
    pub source: dogstatsd::DogStatsDConfig,

    /// DogStatsD mapper transform configuration.
    pub mapper: dogstatsd::DogStatsDMapperConfig,

    /// Aggregate transform configuration.
    pub aggregate: dogstatsd::AggregateConfig,

    /// DogStatsD debug-log destination configuration (dynamic-capable).
    pub debug_log: dogstatsd::DogStatsDDebugLogConfig,

    /// DogStatsD metric prefix/name filter configuration (dynamic-capable).
    pub prefix_filter: dogstatsd::DogStatsDPrefixFilterConfig,

    /// DogStatsD metric-tag filterlist configuration (dynamic-capable).
    pub tag_filterlist: dogstatsd::TagFilterlistConfig,

    /// DogStatsD post-aggregation metric filter configuration (dynamic-capable).
    pub post_aggregate_filter: dogstatsd::DogStatsDPostAggregateFilterConfig,
}

/// OTLP-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpConfigs {
    /// OTLP source configuration (receiver endpoint, interner sizes).
    pub source: otlp::OtlpSourceConfig,

    /// OTLP relay configuration.
    pub relay: otlp::OtlpRelayConfig,

    /// OTLP decoder configuration.
    pub decoder: otlp::OtlpDecoderConfig,

    /// OTLP forwarder configuration.
    pub forwarder: otlp::OtlpForwarderConfig,
}

/// Workload-domain component configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct WorkloadConfigs {
    /// Workload/environment collection configuration.
    pub config: workload::WorkloadConfig,
}

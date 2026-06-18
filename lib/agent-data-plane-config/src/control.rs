//! [`ControlConfiguration`]: pipeline gates and topology-shaping decisions.
//!
//! `ControlConfiguration` is the orchestration half of [`SalukiConfiguration`](crate::SalukiConfiguration).
//! It is read only by config-system and the topology builder; it is never handed to a component.
//! The crate boundary enforces this: `saluki-components` does not depend on `agent-data-plane-config`,
//! so a component cannot name this type.
//!
//! Its fields correspond to the binary's `data_plane.*` configuration (see
//! `bin/agent-data-plane/src/config.rs`). Gates are static for the process lifetime: they are read
//! once at topology assembly and `ControlConfiguration` carries no dynamic handles.

use std::time::Duration;

use saluki_io::net::ListenAddress;

/// Pipeline gates and topology-shaping decisions for the data plane.
///
/// Read once at topology assembly to decide which pipelines and listeners to build. Never consumed
/// by components.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ControlConfiguration {
    /// Whether the data plane runs at all.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Whether the data plane runs in standalone mode.
    ///
    /// Standalone mode is a vestige of earlier development and is not for production use. Defaults
    /// to `false`.
    pub standalone_mode: bool,

    /// Whether the data plane registers itself as a remote agent with the Core Agent.
    ///
    /// Defaults to `false`.
    pub remote_agent_enabled: bool,

    /// Whether to use the new config-stream endpoint when connecting to the Core Agent.
    ///
    /// Defaults to `false`.
    pub use_new_config_stream_endpoint: bool,

    /// The topology-wide graceful shutdown timeout.
    ///
    /// Defaults to [`Duration::ZERO`] (the config-system fills in the real value, derived from the
    /// aggregator and forwarder stop timeouts when `data_plane.stop_timeout` is unset).
    pub stop_timeout: Duration,

    /// The path the data plane writes its own process log to.
    ///
    /// Corresponds to the Datadog `data_plane.log_file` key. Defaults to empty; the config-system
    /// supplies the real default (`/var/log/datadog/agent-data-plane.log`) via the witness drive.
    pub log_file: String,

    /// The address the unprivileged ("API") HTTP server listens on.
    ///
    /// Defaults to `tcp://0.0.0.0:5100`.
    pub api_listen_address: ListenAddress,

    /// The address the privileged ("secure API") HTTP server listens on.
    ///
    /// Defaults to `tcp://0.0.0.0:5101`.
    pub secure_api_listen_address: ListenAddress,

    /// Gate for the DogStatsD pipeline.
    ///
    /// Enabled by default.
    pub dogstatsd: PipelineGate,

    /// Gate for the Checks pipeline.
    ///
    /// Disabled by default.
    pub checks: PipelineGate,

    /// Gate and proxy shaping for the OTLP pipeline.
    ///
    /// Disabled by default.
    pub otlp: OtlpControl,
}

impl ControlConfiguration {
    /// Returns `true` if the outbound Datadog forwarder is required.
    ///
    /// The forwarder is needed only when some pipeline that emits to Datadog is enabled.
    pub fn requires_datadog_forwarder(&self) -> bool {
        self.dogstatsd.enabled() || self.checks.enabled() || self.otlp.enabled
    }

    /// Returns `true` if any data pipeline is enabled.
    ///
    /// Mirrors the binary's historical `DataPlaneConfiguration::data_pipelines_enabled`.
    pub fn data_pipelines_enabled(&self) -> bool {
        self.checks.enabled() || self.dogstatsd.enabled() || self.otlp.enabled
    }

    /// Returns `true` if the baseline metrics pipeline is required.
    ///
    /// The baseline metrics pipeline (aggregation, enrichment, encoding, forwarding) is required by
    /// higher-level pipelines: Checks, DogStatsD, or OTLP when not in proxy mode.
    pub fn metrics_pipeline_required(&self) -> bool {
        self.checks.enabled() || self.dogstatsd.enabled() || (self.otlp.enabled && !self.otlp.proxy.enabled)
    }

    /// Returns `true` if the baseline logs pipeline is required.
    ///
    /// Required by Checks, or by OTLP when not in proxy mode.
    pub fn logs_pipeline_required(&self) -> bool {
        self.checks.enabled() || (self.otlp.enabled && !self.otlp.proxy.enabled)
    }

    /// Returns `true` if the baseline events pipeline is required.
    ///
    /// Required by Checks or DogStatsD.
    pub fn events_pipeline_required(&self) -> bool {
        self.checks.enabled() || self.dogstatsd.enabled()
    }

    /// Returns `true` if the baseline service checks pipeline is required.
    ///
    /// Required by Checks or DogStatsD.
    pub fn service_checks_pipeline_required(&self) -> bool {
        self.checks.enabled() || self.dogstatsd.enabled()
    }

    /// Returns `true` if the baseline traces pipeline is required.
    ///
    /// Required by OTLP when not in proxy mode, or in proxy mode when traces are not proxied.
    pub fn traces_pipeline_required(&self) -> bool {
        self.otlp.enabled && (!self.otlp.proxy.enabled || !self.otlp.proxy.proxy_traces)
    }
}

impl Default for ControlConfiguration {
    fn default() -> Self {
        // `ListenAddress` has no sensible universal default, so the whole struct cannot derive
        // `Default`; the listen-address defaults mirror the binary's `data_plane.*` defaults.
        Self {
            enabled: false,
            standalone_mode: false,
            remote_agent_enabled: false,
            use_new_config_stream_endpoint: false,
            stop_timeout: Duration::ZERO,
            log_file: String::new(),
            api_listen_address: ListenAddress::any_tcp(5100),
            secure_api_listen_address: ListenAddress::any_tcp(5101),
            dogstatsd: PipelineGate { enabled: true },
            checks: PipelineGate::default(),
            otlp: OtlpControl::default(),
        }
    }
}

/// A static on/off gate for a single pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct PipelineGate {
    /// Whether the pipeline is enabled.
    pub enabled: bool,
}

impl PipelineGate {
    /// Returns `true` if the pipeline is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Gate and proxy shaping for the OTLP pipeline.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpControl {
    /// Whether the OTLP pipeline is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// OTLP proxy-mode shaping.
    pub proxy: OtlpProxyControl,
}

/// OTLP proxy-mode shaping decisions.
///
/// In proxy mode, the data plane takes over the OTLP ingest endpoints the Core Agent would normally
/// listen on and proxies unsupported signals back to it. These fields correspond to the binary's
/// `data_plane.otlp.proxy.*` configuration.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpProxyControl {
    /// Whether OTLP proxy mode is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// The Core Agent OTLP gRPC endpoint to proxy signals to.
    ///
    /// Defaults to empty (the config-system supplies the real default, `http://localhost:4319`).
    pub core_agent_otlp_grpc_endpoint: String,

    /// Whether to proxy traces to the Core Agent.
    ///
    /// Defaults to `false`.
    pub proxy_traces: bool,

    /// Whether to proxy metrics to the Core Agent.
    ///
    /// Defaults to `false`.
    pub proxy_metrics: bool,

    /// Whether to proxy logs to the Core Agent.
    ///
    /// Defaults to `false`.
    pub proxy_logs: bool,
}

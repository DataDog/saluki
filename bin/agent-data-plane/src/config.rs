use std::{path::PathBuf, time::Duration};

use agent_data_plane_config::{ControlConfiguration, OtlpProxyGate, PipelineGate};
use datadog_agent_commons::platform::PlatformSettings;
use saluki_component_config::ListenAddress as NativeListenAddress;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;

/// General data plane configuration.
#[derive(Clone, Debug)]
pub struct DataPlaneConfiguration {
    enabled: bool,
    standalone_mode: bool,
    use_new_config_stream_endpoint: bool,
    remote_agent_enabled: bool,
    stop_timeout: Duration,
    api_listen_address: ListenAddress,
    secure_api_listen_address: ListenAddress,
    ipc_auth: DataPlaneIpcAuthConfiguration,
    checks: DataPlaneChecksConfiguration,
    dogstatsd: DataPlaneDogStatsDConfiguration,
    otlp: DataPlaneOtlpConfiguration,
}

impl DataPlaneConfiguration {
    /// Creates a `DataPlaneConfiguration` from the native control-plane slice.
    ///
    /// # Errors
    ///
    /// If one of the configured listen addresses is invalid, an error is returned.
    pub fn from_control(control: &ControlConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            enabled: control.enabled,
            standalone_mode: control.standalone_mode,
            use_new_config_stream_endpoint: control.use_new_config_stream_endpoint,
            remote_agent_enabled: control.remote_agent_enabled,
            stop_timeout: Duration::from_millis(control.stop_timeout_millis),
            api_listen_address: convert_listen_address(&control.api_listen_address, 5100)?,
            secure_api_listen_address: convert_listen_address(&control.secure_api_listen_address, 5101)?,
            ipc_auth: DataPlaneIpcAuthConfiguration::from_control(control),
            checks: DataPlaneChecksConfiguration::from_gate(&control.checks),
            dogstatsd: DataPlaneDogStatsDConfiguration::from_gate(&control.dogstatsd),
            otlp: DataPlaneOtlpConfiguration::from_control(control),
        })
    }

    /// Returns `true` if the data plane is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns `true` if the data plane is running in standalone mode.
    pub const fn standalone_mode(&self) -> bool {
        self.standalone_mode
    }

    /// Returns `true` if the new config stream endpoint should be used.
    pub const fn use_new_config_stream_endpoint(&self) -> bool {
        self.use_new_config_stream_endpoint
    }

    /// Returns `true` if the data plane should register as a remote agent.
    pub const fn remote_agent_enabled(&self) -> bool {
        self.remote_agent_enabled
    }

    /// Returns the topology shutdown timeout.
    pub const fn stop_timeout(&self) -> Duration {
        self.stop_timeout
    }

    /// Returns a reference to the API listen address.
    pub const fn api_listen_address(&self) -> &ListenAddress {
        &self.api_listen_address
    }

    /// Returns a reference to the secure API listen address.
    pub const fn secure_api_listen_address(&self) -> &ListenAddress {
        &self.secure_api_listen_address
    }

    /// Returns a reference to the IPC authentication configuration.
    pub const fn ipc_auth(&self) -> &DataPlaneIpcAuthConfiguration {
        &self.ipc_auth
    }

    /// Returns a reference to the Checks-specific data plane configuration.
    pub const fn checks(&self) -> &DataPlaneChecksConfiguration {
        &self.checks
    }

    /// Returns a reference to the DogStatsD-specific data plane configuration.
    pub const fn dogstatsd(&self) -> &DataPlaneDogStatsDConfiguration {
        &self.dogstatsd
    }

    /// Returns a reference to the OTLP-specific data plane configuration.
    pub const fn otlp(&self) -> &DataPlaneOtlpConfiguration {
        &self.otlp
    }

    /// Returns `true` if any data pipelines are enabled.
    pub const fn data_pipelines_enabled(&self) -> bool {
        self.checks().enabled() || self.dogstatsd().enabled() || self.otlp().enabled()
    }

    /// Returns `true` if the metrics pipeline is required.
    pub const fn metrics_pipeline_required(&self) -> bool {
        self.checks().enabled()
            || self.dogstatsd().enabled()
            || (self.otlp().enabled() && !self.otlp().proxy().enabled())
    }

    /// Returns `true` if the logs pipeline is required.
    pub const fn logs_pipeline_required(&self) -> bool {
        self.checks().enabled() || (self.otlp().enabled() && !self.otlp().proxy().enabled())
    }

    /// Returns `true` if the events pipeline is required.
    pub const fn events_pipeline_required(&self) -> bool {
        self.checks().enabled() || self.dogstatsd().enabled()
    }

    /// Returns `true` if the service checks pipeline is required.
    pub const fn service_checks_pipeline_required(&self) -> bool {
        self.checks().enabled() || self.dogstatsd().enabled()
    }

    /// Returns `true` if the traces pipeline is required.
    pub const fn traces_pipeline_required(&self) -> bool {
        self.otlp().enabled() && (!self.otlp().proxy().enabled() || !self.otlp().proxy().proxy_traces())
    }
}

fn convert_listen_address(address: &NativeListenAddress, default_port: u16) -> Result<ListenAddress, GenericError> {
    match address {
        NativeListenAddress::Disabled => Ok(ListenAddress::any_tcp(default_port)),
        NativeListenAddress::Tcp(value) => parse_listen_address(value, "tcp"),
        NativeListenAddress::Udp(value) => parse_listen_address(value, "udp"),
        NativeListenAddress::Unix(value) => parse_listen_address(value, "unix"),
    }
}

fn parse_listen_address(value: &str, default_scheme: &str) -> Result<ListenAddress, GenericError> {
    let raw = if value.contains("://") {
        value.to_string()
    } else {
        format!("{default_scheme}://{value}")
    };
    ListenAddress::try_from(raw.as_str()).map_err(|e| generic_error!("Invalid listen address `{}`: {}", value, e))
}

/// IPC authentication and TLS file paths.
#[derive(Clone, Debug)]
pub struct DataPlaneIpcAuthConfiguration {
    auth_token_file_path: PathBuf,
    ipc_cert_file_path: Option<PathBuf>,
}

impl DataPlaneIpcAuthConfiguration {
    fn from_control(control: &ControlConfiguration) -> Self {
        Self {
            auth_token_file_path: control
                .ipc_auth
                .auth_token_file_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(PlatformSettings::get_auth_token_path),
            ipc_cert_file_path: control
                .ipc_auth
                .ipc_cert_file_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Returns the Agent IPC certificate file path.
    pub fn ipc_cert_file_path(&self) -> PathBuf {
        if let Some(path) = self.ipc_cert_file_path.as_ref() {
            return path.clone();
        }
        let auth_token_dir = self
            .auth_token_file_path
            .parent()
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| PlatformSettings::get_config_dir_path().to_path_buf());
        auth_token_dir.join(PlatformSettings::get_ipc_cert_filename())
    }
}

/// Checks-specific data plane configuration.
#[derive(Clone, Debug)]
pub struct DataPlaneChecksConfiguration {
    enabled: bool,
}

impl DataPlaneChecksConfiguration {
    fn from_gate(gate: &PipelineGate) -> Self {
        Self {
            enabled: gate.enabled(),
        }
    }

    /// Returns `true` if Checks is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

/// DogStatsD-specific data plane configuration.
#[derive(Clone, Debug)]
pub struct DataPlaneDogStatsDConfiguration {
    enabled: bool,
}

impl DataPlaneDogStatsDConfiguration {
    fn from_gate(gate: &PipelineGate) -> Self {
        Self {
            enabled: gate.enabled(),
        }
    }

    /// Returns `true` if DogStatsD is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

/// OTLP-specific data plane configuration.
#[derive(Clone, Debug)]
pub struct DataPlaneOtlpConfiguration {
    enabled: bool,
    proxy: DataPlaneOtlpProxyConfiguration,
}

impl DataPlaneOtlpConfiguration {
    fn from_control(control: &ControlConfiguration) -> Self {
        Self {
            enabled: control.otlp.native.enabled(),
            proxy: DataPlaneOtlpProxyConfiguration::from_gate(&control.otlp.proxy),
        }
    }

    /// Returns `true` if the OTLP pipeline is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns a reference to the OTLP proxying configuration.
    pub const fn proxy(&self) -> &DataPlaneOtlpProxyConfiguration {
        &self.proxy
    }
}

/// OTLP proxying configuration.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct DataPlaneOtlpProxyConfiguration {
    enabled: bool,
    core_agent_otlp_grpc_endpoint: String,
    proxy_traces: bool,
    proxy_metrics: bool,
    proxy_logs: bool,
}

impl DataPlaneOtlpProxyConfiguration {
    fn from_gate(gate: &OtlpProxyGate) -> Self {
        Self {
            enabled: gate.enabled,
            core_agent_otlp_grpc_endpoint: gate.core_agent_otlp_grpc_endpoint.clone(),
            proxy_traces: true,
            proxy_metrics: true,
            proxy_logs: true,
        }
    }

    /// Returns `true` if the OTLP proxy is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the OTLP gRPC endpoint on the Core Agent to proxy signals to.
    pub fn core_agent_otlp_grpc_endpoint(&self) -> &str {
        &self.core_agent_otlp_grpc_endpoint
    }

    /// Returns `true` if OTLP traces should be proxied to the Core Agent.
    pub const fn proxy_traces(&self) -> bool {
        self.proxy_traces
    }

    /// Returns `true` if OTLP metrics should be proxied to the Core Agent.
    pub const fn proxy_metrics(&self) -> bool {
        self.proxy_metrics
    }

    /// Returns `true` if OTLP logs should be proxied to the Core Agent.
    pub const fn proxy_logs(&self) -> bool {
        self.proxy_logs
    }
}

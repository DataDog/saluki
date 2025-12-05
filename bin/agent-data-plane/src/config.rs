use saluki_io::net::ListenAddress;
use serde::Deserialize;

/// General data plane configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DataPlaneConfiguration {
    enabled: bool,
    standalone_mode: bool,
    use_new_config_stream_endpoint: bool,
    api_listen_address: ListenAddress,
    secure_api_listen_address: ListenAddress,
    telemetry_enabled: bool,
    telemetry_listen_addr: ListenAddress,
    dogstatsd: DataPlaneDogStatsDConfiguration,
    otlp: DataPlaneOtlpConfiguration,
}

impl DataPlaneConfiguration {
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

    /// Returns a reference to the API listen address
    ///
    /// This is also referred to as the "unprivileged" API.
    pub const fn api_listen_address(&self) -> &ListenAddress {
        &self.api_listen_address
    }

    /// Returns a reference to the secure API listen address.
    ///
    /// This is also referred to as the "privileged" API.
    pub const fn secure_api_listen_address(&self) -> &ListenAddress {
        &self.secure_api_listen_address
    }

    /// Returns `true` if telemetry is enabled.
    pub const fn telemetry_enabled(&self) -> bool {
        self.telemetry_enabled
    }

    /// Returns a reference to the telemetry listen address.
    pub const fn telemetry_listen_addr(&self) -> &ListenAddress {
        &self.telemetry_listen_addr
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
        self.dogstatsd().enabled() || self.otlp().enabled()
    }

    /// Returns `true` if the metrics pipeline is required.
    ///
    /// This indicates that the "baseline" metrics pipeline (aggregation, enrichment, encoding, forwarding) is required
    /// by higher-level data pipelines, such as DogStatsD.
    pub const fn metrics_pipeline_required(&self) -> bool {
        // We consider the metrics pipeline to be enabled if:
        // - DogStatsD is enabled
        // - OTLP is enabled and not in proxy mode
        self.dogstatsd().enabled() || (self.otlp().enabled() && !self.otlp().proxy().enabled())
    }

    /// Returns `true` if the logs pipeline is required.
    ///
    /// This indicates that the "baseline" logs pipeline (encoding, forwarding) is required by higher-level data
    /// pipelines, such as OTLP.
    pub const fn logs_pipeline_required(&self) -> bool {
        // We consider the logs pipeline to be enabled if:
        // - OTLP is enabled and not in proxy mode
        self.otlp().enabled() && !self.otlp().proxy().enabled()
    }
}

impl Default for DataPlaneConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            standalone_mode: false,
            use_new_config_stream_endpoint: false,
            api_listen_address: ListenAddress::any_tcp(5100),
            secure_api_listen_address: ListenAddress::any_tcp(5101),
            telemetry_enabled: false,
            telemetry_listen_addr: ListenAddress::any_tcp(5102),
            dogstatsd: DataPlaneDogStatsDConfiguration::default(),
            otlp: DataPlaneOtlpConfiguration::default(),
        }
    }
}

/// DogStatsD-specific data plane configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneDogStatsDConfiguration {
    /// Whether DogStatsD is enabled.
    ///
    /// When disabled, DogStatsD will not be started.
    ///
    /// Defaults to `false`.
    enabled: bool,
}

impl DataPlaneDogStatsDConfiguration {
    /// Returns `true` if DogStatsD is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

/// OTLP-specific data plane configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneOtlpConfiguration {
    enabled: bool,
    proxy: DataPlaneOtlpProxyConfiguration,
}

impl DataPlaneOtlpConfiguration {
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
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DataPlaneOtlpProxyConfiguration {
    /// Whether or not to proxy all signals to the Agent.
    ///
    /// When enabled, OTLP signals which are not supported by ADP will be proxied to the Agent. Depending on the signal
    /// type, they may be proxied to either the Core Agent or Trace Agent.
    ///
    /// Defaults to `true`.
    enabled: bool,

    /// OTLP-specific endpoint on the Core Agent to proxy signals to.
    ///
    /// In proxy mode, ADP takes over the normal "OTLP Ingest" endpoints that the Core Agent would typically listen on,
    /// so the Core Agent must be configured to listen on a different, separate port than it usually would so that ADP
    /// can proxy to it.
    ///
    /// Defaults to `http://localhost:4320`.
    core_agent_otlp_endpoint: String,
}

impl DataPlaneOtlpProxyConfiguration {
    /// Returns `true` if the OTLP proxy is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the OTLP endpoint on the Core Agent to proxy signals to.
    pub fn core_agent_otlp_endpoint(&self) -> &str {
        &self.core_agent_otlp_endpoint
    }
}

impl Default for DataPlaneOtlpProxyConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            core_agent_otlp_endpoint: "http://localhost:4320".to_string(),
        }
    }
}

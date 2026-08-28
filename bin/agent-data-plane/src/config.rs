use std::collections::HashSet;
use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_commons::ipc::config::{IpcAuthConfiguration, RemoteAgentClientConfiguration};
use datadog_agent_config::classifier::Pipeline;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// General data plane configuration.
///
/// This wrapper provides orchestration-level accessors and pipeline decisions over the typed configuration. It lives
/// during bootstrap and topology construction and is then discarded.
#[derive(Clone, Debug)]
pub struct DataPlaneConfiguration<'a> {
    config: &'a SalukiConfiguration,
}

/// Translates typed IPC settings into client configuration.
pub(crate) fn remote_agent_client_configuration(
    config: &SalukiConfiguration,
) -> Result<RemoteAgentClientConfiguration, GenericError> {
    let dp = DataPlaneConfiguration::from_configuration(config);

    #[cfg(target_os = "linux")]
    let vsock_cid = match config.control.ipc.vsock_addr.as_str() {
        "" => None,
        "host" => Some(2),
        "hypervisor" => Some(0),
        "local" => Some(3),
        other => {
            return Err(generic_error!(
                "invalid vsock address '{}'; expected one of: host, hypervisor, local",
                other
            ))
        }
    };

    #[cfg(not(target_os = "linux"))]
    if !config.control.ipc.vsock_addr.is_empty() {
        warn!("`vsock_addr` is configured but vsock is only supported on Linux. Setting will be ignored.");
    }

    Ok(RemoteAgentClientConfiguration {
        cmd_port: config.control.ipc.cmd_port,
        auth: dp.ipc_auth_configuration(),
        grpc_max_message_size: config.control.ipc.grpc_max_message_size,
        #[cfg(target_os = "linux")]
        vsock_cid,
    })
}

impl<'a> DataPlaneConfiguration<'a> {
    /// Creates a new `DataPlaneConfiguration` instance from the given configuration.
    pub fn from_configuration(config: &'a SalukiConfiguration) -> Self {
        Self { config }
    }

    /// Builds the resolved Agent IPC authentication configuration.
    pub(crate) fn ipc_auth_configuration(&self) -> IpcAuthConfiguration {
        IpcAuthConfiguration::new(
            self.config.control.ipc.auth_token_file_path.clone(),
            self.config.control.ipc.ipc_cert_file_path.clone(),
        )
    }

    /// Returns `true` if the data plane is enabled.
    pub const fn enabled(&self) -> bool {
        self.config.control.enabled
    }

    /// Returns `true` if the data plane is running in standalone mode.
    pub const fn standalone_mode(&self) -> bool {
        self.config.control.standalone_mode
    }

    /// Returns the topology shutdown timeout.
    ///
    /// Uses `data_plane.stop_timeout` when configured. Otherwise, it sums `aggregator_stop_timeout`
    /// and `forwarder_stop_timeout`, returning `Duration::MAX` if the sum overflows.
    pub fn stop_timeout(&self) -> Duration {
        match self.config.control.stop_timeout {
            Some(timeout) => timeout,
            None => self
                .config
                .control
                .aggregator_stop_timeout
                .saturating_add(self.config.shared.endpoints.forwarder.stop_timeout),
        }
    }

    /// Resolves the API listen address.
    ///
    /// This is also referred to as the "unprivileged" API.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured value is not a valid listen address.
    pub fn api_listen_address(&self) -> Result<ListenAddress, GenericError> {
        ListenAddress::try_from(self.config.control.api_listen_address.as_str())
            .map_err(|e| generic_error!("Invalid listen address for `data_plane.api_listen_address`: {}", e))
    }

    /// Resolves the secure, or privileged, API listen address.
    ///
    /// Every HTTP and gRPC client must present the exact configured Agent IPC certificate during the TLS handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured value is not a valid listen address.
    pub fn secure_api_listen_address(&self) -> Result<ListenAddress, GenericError> {
        ListenAddress::try_from(self.config.control.secure_api_listen_address.as_str()).map_err(|e| {
            generic_error!(
                "Invalid listen address for `data_plane.secure_api_listen_address`: {}",
                e
            )
        })
    }

    /// Returns `true` if Checks is enabled.
    pub const fn checks_enabled(&self) -> bool {
        self.config.control.checks
    }

    /// Returns `true` if DogStatsD is enabled.
    pub const fn dogstatsd_enabled(&self) -> bool {
        self.config.control.dogstatsd
    }

    /// Returns `true` if the OTLP pipeline is enabled.
    pub const fn otlp_enabled(&self) -> bool {
        self.config.control.otlp
    }

    /// Returns `true` if the OTLP proxy is enabled.
    pub const fn otlp_proxy_enabled(&self) -> bool {
        self.config.domains.otlp.proxy.enabled
    }

    /// Returns `true` if OTLP traces should be proxied to the Core Agent.
    pub const fn otlp_proxy_traces_enabled(&self) -> bool {
        self.config.domains.otlp.proxy.traces_enabled
    }

    /// Returns `true` if any data pipelines are enabled.
    pub const fn data_pipelines_enabled(&self) -> bool {
        self.checks_enabled() || self.dogstatsd_enabled() || self.otlp_enabled()
    }

    /// Returns `true` if the metrics pipeline is required.
    ///
    /// Connected topologies need this pipeline whenever they have a data pipeline so the liveness metric can be
    /// enriched and forwarded, including when the only data pipeline is an OTLP proxy. Standalone mode only creates
    /// the pipeline for data sources that use it directly.
    pub const fn metrics_pipeline_required(&self) -> bool {
        self.checks_enabled()
            || self.dogstatsd_enabled()
            || (self.otlp_enabled() && !self.otlp_proxy_enabled())
            || (!self.standalone_mode() && self.data_pipelines_enabled())
    }

    /// Returns `true` if the logs pipeline is required.
    ///
    /// This indicates that the "baseline" logs pipeline (encoding, forwarding) is required by higher-level data
    /// pipelines, such as Checks or OTLP.
    pub const fn logs_pipeline_required(&self) -> bool {
        // We consider the logs pipeline to be enabled if:
        // - Checks is enabled
        // - OTLP is enabled and not in proxy mode
        self.checks_enabled() || (self.otlp_enabled() && !self.otlp_proxy_enabled())
    }

    /// Returns `true` if the events pipeline is required.
    ///
    /// This indicates that the "baseline" events pipeline (encoding, forwarding) is required by higher-level data
    /// pipelines, such as Checks or DogStatsD.
    pub const fn events_pipeline_required(&self) -> bool {
        self.checks_enabled() || self.dogstatsd_enabled()
    }

    /// Returns `true` if the service checks pipeline is required.
    ///
    /// Connected topologies need this pipeline whenever they have a data pipeline so the liveness service check can
    /// be encoded and forwarded, including when the only data pipeline is an OTLP proxy. Standalone mode only creates
    /// the pipeline for data sources that use it directly.
    pub const fn service_checks_pipeline_required(&self) -> bool {
        self.checks_enabled() || self.dogstatsd_enabled() || (!self.standalone_mode() && self.data_pipelines_enabled())
    }

    /// Returns `true` if the traces pipeline is required.
    ///
    /// This indicates that the "baseline" traces pipeline (encoding, forwarding) is required by higher-level data
    /// pipelines, such as OTLP.
    pub const fn traces_pipeline_required(&self) -> bool {
        // We consider the traces pipeline to be enabled if:
        // - OTLP is enabled and not in proxy mode or proxy mode is enabled and proxy traces are disabled
        self.otlp_enabled() && (!self.otlp_proxy_enabled() || !self.otlp_proxy_traces_enabled())
    }

    /// Returns the set of [`Pipeline`] variants that are active based on our configuration.
    pub fn active_pipelines(&self) -> HashSet<Pipeline> {
        let mut s = HashSet::new();
        if self.dogstatsd_enabled() {
            s.insert(Pipeline::DogStatsD);
        }
        if self.checks_enabled() {
            s.insert(Pipeline::Checks);
        }
        if self.otlp_enabled() {
            s.insert(Pipeline::Otlp);
        }
        if self.traces_pipeline_required() {
            s.insert(Pipeline::Traces);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_commons::platform::PlatformSettings;

    use super::*;

    #[test]
    fn remote_agent_client_configuration_resolves_default_auth_paths() {
        let client_config =
            remote_agent_client_configuration(&SalukiConfiguration::default()).expect("valid IPC configuration");

        assert_eq!(
            client_config.auth.auth_token_file_path(),
            PlatformSettings::get_auth_token_path()
        );
        assert_eq!(
            client_config.auth.ipc_cert_file_path(),
            PlatformSettings::get_config_dir_path().join(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[test]
    fn remote_agent_client_configuration_uses_typed_auth_paths() {
        let mut config = SalukiConfiguration::default();
        config.control.ipc.auth_token_file_path = "/secret/auth_token".into();
        config.control.ipc.ipc_cert_file_path = "/secret/ipc_cert.pem".into();

        let client_config = remote_agent_client_configuration(&config).expect("valid IPC configuration");
        assert_eq!(
            client_config.auth.auth_token_file_path(),
            std::path::Path::new("/secret/auth_token")
        );
        assert_eq!(
            client_config.auth.ipc_cert_file_path(),
            std::path::Path::new("/secret/ipc_cert.pem")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remote_agent_client_configuration_resolves_vsock_addresses() {
        for (value, expected_cid) in [
            ("", None),
            ("host", Some(2)),
            ("hypervisor", Some(0)),
            ("local", Some(3)),
        ] {
            let mut config = SalukiConfiguration::default();
            config.control.ipc.cmd_port = 5001;
            config.control.ipc.grpc_max_message_size = 4 * 1024 * 1024;
            config.control.ipc.vsock_addr = value.to_string();

            let client_config = remote_agent_client_configuration(&config).expect("valid IPC configuration");
            assert_eq!(client_config.vsock_cid, expected_cid);
            assert_eq!(client_config.cmd_port, 5001);
            assert_eq!(client_config.grpc_max_message_size, 4 * 1024 * 1024);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remote_agent_client_configuration_rejects_invalid_vsock_addresses() {
        for value in ["invalid", "2", "HOST", "host ", "vm0"] {
            let mut config = SalukiConfiguration::default();
            config.control.ipc.vsock_addr = value.to_string();

            assert!(
                remote_agent_client_configuration(&config).is_err(),
                "expected error for input: {value:?}",
            );
        }
    }

    fn pipeline_configuration(
        checks_enabled: bool, dogstatsd_enabled: bool, otlp_enabled: bool, otlp_proxy_enabled: bool,
        otlp_proxy_traces_enabled: bool,
    ) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.control.checks = checks_enabled;
        config.control.dogstatsd = dogstatsd_enabled;
        config.control.otlp = otlp_enabled;
        config.domains.otlp.proxy.enabled = otlp_proxy_enabled;
        config.domains.otlp.proxy.traces_enabled = otlp_proxy_traces_enabled;
        config
    }

    // Pipeline-requirement predicates. Each scenario is chosen to walk one documented branch of the
    // `*_pipeline_required` predicates on `DataPlaneConfiguration`, asserting the full predicate set so that a
    // regression in any single predicate surfaces.

    #[test]
    fn dogstatsd_only_requires_metrics_events_and_service_checks_pipelines() {
        let config = pipeline_configuration(false, true, false, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn checks_enabled_requires_every_pipeline_except_traces() {
        let config = pipeline_configuration(true, false, false, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn otlp_without_proxy_requires_metrics_logs_and_traces_pipelines() {
        let config = pipeline_configuration(false, false, true, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(dp.traces_pipeline_required());
    }

    #[test]
    fn standalone_otlp_proxy_mode_does_not_require_liveness_baseline_pipelines() {
        // Standalone OTLP proxy mode must only construct the local proxy path, which avoids resolving output endpoints.
        let mut config = pipeline_configuration(false, false, true, true, true);
        config.control.standalone_mode = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(!dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn connected_otlp_proxy_mode_requires_liveness_baseline_pipelines() {
        // Connected mode still forwards the liveness metric and service check, so those baselines stay required even
        // when every OTLP signal is proxied.
        let config = pipeline_configuration(false, false, true, true, true);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn otlp_proxy_mode_with_local_traces_requires_liveness_and_traces_pipelines() {
        // Proxy mode is enabled but trace proxying is turned off, so ADP must handle traces locally. The liveness
        // metric and service-check baselines remain required regardless of the OTLP routing.
        let config = pipeline_configuration(false, false, true, true, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(dp.traces_pipeline_required());
    }

    #[test]
    fn no_pipelines_enabled_requires_no_baseline_pipelines() {
        let config = pipeline_configuration(false, false, false, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(!dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn stop_timeout_uses_saluki_override() {
        let mut config = SalukiConfiguration::default();
        config.control.stop_timeout = Some(Duration::from_secs(11));
        config.control.aggregator_stop_timeout = Duration::from_secs(3);
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(7);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.stop_timeout(), Duration::from_secs(11));
    }

    #[test]
    fn stop_timeout_sums_component_timeouts() {
        let mut config = SalukiConfiguration::default();
        config.control.aggregator_stop_timeout = Duration::from_secs(3);
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(7);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.stop_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn stop_timeout_saturates_when_sum_overflows() {
        let mut config = SalukiConfiguration::default();
        config.control.aggregator_stop_timeout = Duration::MAX;
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(1);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.stop_timeout(), Duration::MAX);
    }
}

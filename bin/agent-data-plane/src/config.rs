use std::collections::HashSet;
use std::time::Duration;

use agent_data_plane_config::{ConfigValue, SalukiConfiguration};
use datadog_agent_commons::ipc::config::{IpcAuthConfiguration, RemoteAgentClientConfiguration};
use datadog_agent_config::classifier::Pipeline;
use datadog_agent_runtime::env::{HostSettings, WorkloadSettings};
use saluki_components::sources::{listener_reached_by, OtlpRoute, OtlpRoutes};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;

use crate::component_inputs::{otlp_proxy_destinations, otlp_receiver};

/// General data plane configuration.
///
/// This wrapper provides orchestration-level accessors and pipeline decisions over the typed configuration. It lives
/// during bootstrap and topology construction and is then discarded.
#[derive(Clone, Debug)]
pub struct DataPlaneConfiguration<'a> {
    config: &'a SalukiConfiguration,

    /// Error to log when an OTLP proxy destination reaches one of ADP's own OTLP listeners.
    otlp_proxy_loop: Option<String>,
}

/// The OTLP signals that ADP accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OtlpSignals {
    /// Whether OTLP metrics are accepted.
    pub metrics: bool,

    /// Whether OTLP logs are accepted.
    pub logs: bool,

    /// Whether OTLP traces are accepted.
    pub traces: bool,
}

/// Translates typed IPC settings into client configuration.
pub(crate) fn remote_agent_client_configuration(
    config: &SalukiConfiguration,
) -> Result<RemoteAgentClientConfiguration, GenericError> {
    let ipc = &config.control.ipc;
    let auth = DataPlaneConfiguration::from_configuration(config).ipc_auth_configuration();
    RemoteAgentClientConfiguration::from_parts(
        &ipc.cmd_host,
        ipc.cmd_port,
        auth,
        ipc.grpc_max_message_size,
        &ipc.vsock_addr,
    )
    .error_context(
        "Invalid Datadog Agent IPC configuration. Check `cmd_host` (or the deprecated `ipc_address`) and `vsock_addr`.",
    )
}

/// Translates the typed workload settings into the workload provider's settings.
///
/// Paths are only carried over when they were set explicitly, so that defaulted values select automatic detection.
pub(crate) fn workload_settings(config: &SalukiConfiguration) -> WorkloadSettings {
    let environment = &config.shared.environment;

    let mut settings = WorkloadSettings::new(config.control.ipc.remote_agent_string_interner_size_bytes);
    settings.containerd_socket_path = explicit(&environment.containerd.socket_path);
    settings.containerd_connection_timeout = environment.containerd.connection_timeout;
    settings.containerd_query_timeout = environment.containerd.query_timeout;
    settings.proc_root = explicit(&environment.container_roots.proc_root);
    settings.cgroup_root = explicit(&environment.container_roots.cgroup_root);
    settings
}

/// Translates the typed hostname into the host provider's settings.
///
/// The hostname is only carried over when it was set explicitly.
pub(crate) fn host_settings(config: &SalukiConfiguration) -> HostSettings {
    let mut settings = HostSettings::default();
    settings.hostname = explicit(&config.shared.environment.hostname);
    settings
}

fn explicit<T: Clone>(value: &ConfigValue<T>) -> Option<T> {
    value.is_explicit().then(|| value.value.clone())
}

impl<'a> DataPlaneConfiguration<'a> {
    /// Creates a new `DataPlaneConfiguration` instance from the given configuration.
    pub fn from_configuration(config: &'a SalukiConfiguration) -> Self {
        Self {
            config,
            otlp_proxy_loop: None,
        }
    }

    /// Checks whether an OTLP proxy destination reaches one of ADP's own OTLP listeners.
    ///
    /// ADP would then proxy requests to itself. Like [`otlp_blocked`](Self::otlp_blocked), this blocks OTLP ingest
    /// instead of failing startup, and [`otlp_proxy_loop_message`](Self::otlp_proxy_loop_message) returns the error
    /// to log. Only a configuration that proxies a signal is checked.
    pub async fn with_otlp_proxy_loop_check(mut self) -> Self {
        self.otlp_proxy_loop = self.find_otlp_proxy_loop().await;
        self
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
    /// and `forwarder_stop_timeout`, returning `Duration::MAX` if the sum overflows. See
    /// [`SalukiConfiguration::stop_timeout`].
    pub fn stop_timeout(&self) -> Duration {
        self.config.stop_timeout()
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
    ///
    /// The pipeline needs at least one accepted OTLP signal. Like the Agent, ADP treats OTLP as disabled when none is
    /// accepted, instead of failing startup. It is also disabled when [`otlp_blocked`](Self::otlp_blocked) is `true`,
    /// or when [`with_otlp_proxy_loop_check`](Self::with_otlp_proxy_loop_check) found a loop.
    pub const fn otlp_enabled(&self) -> bool {
        self.config.control.otlp
            && !self.otlp_blocked()
            && self.otlp_proxy_loop.is_none()
            && self.otlp_signals_enabled()
    }

    /// Returns `true` if the OTLP pipeline is enabled but no OTLP signal (metrics, logs, or traces) is accepted.
    ///
    /// It is `false` when [`otlp_blocked`](Self::otlp_blocked) is `true`, so a blocked pipeline gets one error.
    pub const fn otlp_enabled_without_signals(&self) -> bool {
        self.config.control.otlp && !self.otlp_blocked() && !self.otlp_signals_enabled()
    }

    /// Returns `true` if OTLP is enabled and any proxy flag is set, even for a signal that isn't accepted.
    ///
    /// The flags select OTLP proxy mode, in which the Agent moves its OTLP receiver to the endpoints that ADP proxies
    /// to. ADP proxies a signal only when it accepts it and the signal's own flag is set: see
    /// [`otlp_routes`](Self::otlp_routes).
    pub const fn otlp_proxy_mode(&self) -> bool {
        self.config.control.otlp && self.config.domains.otlp.proxy.enabled()
    }

    /// Returns `true` if ADP is in OTLP proxy mode and a receiver sets `tls`.
    ///
    /// A proxy flag counts even for a disabled signal, as in the Agent.
    ///
    /// ADP rejects TLS here because the Agent and ADP can't easily use the same customer TLS configuration to set up
    /// both sides. ADP reads the canonical configuration locations to set up its own listener, which would leave the
    /// Agent listening for the proxied signals in plaintext. That would violate the customer's intended security
    /// configuration, so ADP rejects a configuration that asks for TLS.
    pub const fn otlp_blocked(&self) -> bool {
        let receiver = &self.config.domains.otlp.receiver;
        self.otlp_proxy_mode() && (receiver.grpc.tls_configured || receiver.http.tls_configured)
    }

    /// Returns the error to log when [`otlp_blocked`](Self::otlp_blocked) is `true`, and `None` otherwise.
    ///
    /// It names the protocols that set `tls` and the proxy flags that are `true`. It is worded like the Agent's
    /// `ErrDataPlaneProxyTLS`.
    pub fn otlp_blocked_message(&self) -> Option<String> {
        if !self.otlp_blocked() {
            return None;
        }

        let otlp = &self.config.domains.otlp;
        let protocols = [
            ("grpc", otlp.receiver.grpc.tls_configured),
            ("http", otlp.receiver.http.tls_configured),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(protocol, _)| format!("otlp_config.receiver.protocols.{protocol}"))
        .collect::<Vec<_>>()
        .join(" and ");
        let signals = [
            ("logs", otlp.proxy.logs_enabled),
            ("metrics", otlp.proxy.metrics_enabled),
            ("traces", otlp.proxy.traces_enabled),
        ]
        .into_iter()
        .filter(|(_, proxied)| *proxied)
        .map(|(signal, _)| format!("data_plane.experimental.otlp.proxy.{signal}"))
        .collect::<Vec<_>>()
        .join(" and ");

        Some(format!(
            "OTLP receiver TLS is not supported when ADP proxies OTLP signals to the Agent: remove tls from \
            {protocols}, or set {signals} to false"
        ))
    }

    /// Returns the error to log when [`with_otlp_proxy_loop_check`](Self::with_otlp_proxy_loop_check) found a loop,
    /// and `None` otherwise.
    pub fn otlp_proxy_loop_message(&self) -> Option<&str> {
        self.otlp_proxy_loop.as_deref()
    }

    /// Returns the OTLP signals that ADP accepts.
    ///
    /// Like the Agent, ADP only accepts a signal whose pipeline it builds, and doesn't serve the others at all.
    /// Metrics and traces follow `otlp_config.metrics.enabled` and `otlp_config.traces.enabled`. Logs also need the
    /// logs agent, `logs_enabled`, as the Agent only builds its OTLP logs pipeline when the logs agent is enabled.
    pub const fn otlp_signals(&self) -> OtlpSignals {
        let otlp = &self.config.domains.otlp;
        OtlpSignals {
            metrics: otlp.receiver.metrics_enabled,
            logs: otlp.receiver.logs_enabled && self.config.control.logs_enabled,
            traces: otlp.traces.enabled,
        }
    }

    /// Returns `true` if ADP accepts at least one OTLP signal.
    const fn otlp_signals_enabled(&self) -> bool {
        let signals = self.otlp_signals();
        signals.metrics || signals.logs || signals.traces
    }

    /// Returns how ADP serves each OTLP signal.
    ///
    /// A signal is disabled when the OTLP pipeline isn't enabled or ADP doesn't accept the signal. ADP proxies an
    /// accepted signal to the Agent when the signal's proxy flag is set, and processes it itself otherwise. The
    /// topology and every pipeline predicate follow these routes.
    pub const fn otlp_routes(&self) -> OtlpRoutes {
        const fn route(accepted: bool, proxied: bool) -> OtlpRoute {
            match (accepted, proxied) {
                (false, _) => OtlpRoute::Disabled,
                (true, true) => OtlpRoute::Proxied,
                (true, false) => OtlpRoute::Internal,
            }
        }

        let enabled = self.otlp_enabled();
        let signals = self.otlp_signals();
        let proxy = &self.config.domains.otlp.proxy;
        OtlpRoutes {
            metrics: route(enabled && signals.metrics, proxy.metrics_enabled),
            logs: route(enabled && signals.logs, proxy.logs_enabled),
            traces: route(enabled && signals.traces, proxy.traces_enabled),
        }
    }

    /// Returns the error to log if a proxy destination of a proxied protocol reaches one of ADP's own OTLP listeners.
    ///
    /// The OTLP source reports invalid listen addresses and destinations when it's built, so they don't count here.
    async fn find_otlp_proxy_loop(&self) -> Option<String> {
        if !self.otlp_routes().any_proxied() {
            return None;
        }

        let otlp = &self.config.domains.otlp;
        let destinations = otlp_proxy_destinations(otlp).ok()?;
        let receiver = otlp_receiver(&otlp.receiver);
        let grpc_listener = receiver.grpc.listen_address().ok().flatten();
        let http_listener = receiver.http.listen_address().ok().flatten();
        let listeners = [
            ("otlp_config.receiver.protocols.grpc.endpoint", grpc_listener.as_ref()),
            ("otlp_config.receiver.protocols.http.endpoint", http_listener.as_ref()),
        ];
        let proxied = [
            (
                "data_plane.experimental.otlp.proxy.agent_receiver_grpc_endpoint",
                &otlp.proxy.grpc_endpoint,
                &destinations.grpc,
                grpc_listener.is_some(),
            ),
            (
                "data_plane.experimental.otlp.proxy.agent_receiver_http_endpoint",
                &otlp.proxy.http_endpoint,
                &destinations.http,
                http_listener.is_some(),
            ),
        ];

        for (destination_key, address, destination, served) in proxied {
            // A protocol without a listener proxies nothing.
            if !served {
                continue;
            }

            let Some(listener) = listener_reached_by(destination, listeners.iter().filter_map(|(_, l)| *l)).await
            else {
                continue;
            };
            let listener_key = listeners
                .iter()
                .find_map(|(key, l)| (*l == Some(listener)).then_some(*key))?;
            return Some(format!(
                "The OTLP proxy destination `{destination_key}` ({address}) reaches ADP's own OTLP listener \
                `{listener_key}` ({listener}), so ADP would proxy OTLP requests to itself: set `{destination_key}` to \
                the Agent's relocated OTLP receiver, or change `{listener_key}`"
            ));
        }

        None
    }

    /// Returns `true` if the APM pipeline is enabled.
    pub const fn apm_enabled(&self) -> bool {
        self.config.control.apm
    }

    /// Returns `true` if any data pipelines are enabled.
    pub const fn data_pipelines_enabled(&self) -> bool {
        self.checks_enabled() || self.dogstatsd_enabled() || self.otlp_enabled() || self.apm_enabled()
    }

    /// Returns `true` if the metrics pipeline is required.
    ///
    /// Connected topologies need this pipeline whenever they have a data pipeline so the liveness metric can be
    /// enriched and forwarded, including when the only data pipeline is an OTLP proxy. The trace sampler reports its
    /// sampling telemetry as metric events, so a running traces pipeline also requires this one. Standalone mode
    /// only creates the pipeline for data sources that use it directly.
    pub const fn metrics_pipeline_required(&self) -> bool {
        self.checks_enabled()
            || self.dogstatsd_enabled()
            || matches!(self.otlp_routes().metrics, OtlpRoute::Internal)
            || (!self.standalone_mode() && self.data_pipelines_enabled())
            || self.traces_pipeline_required()
    }

    /// Returns `true` if the logs pipeline is required.
    ///
    /// This indicates that the "baseline" logs pipeline (encoding, forwarding) is required by higher-level data
    /// pipelines, such as Checks or OTLP. It also decides whether the classifier's `logs` pipeline is active, so the
    /// startup compatibility check follows the topology.
    pub const fn logs_pipeline_required(&self) -> bool {
        self.checks_enabled() || matches!(self.otlp_routes().logs, OtlpRoute::Internal)
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
        matches!(self.otlp_routes().traces, OtlpRoute::Internal)
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
        if self.logs_pipeline_required() {
            s.insert(Pipeline::Logs);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::otlp::{GrpcTransport, Proxy};
    use datadog_agent_commons::platform::PlatformSettings;

    use super::*;

    /// Returns a default configuration with the IPC host the Datadog witness writes by default.
    fn ipc_configuration() -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.control.ipc.cmd_host = "localhost".to_string();
        config
    }

    #[test]
    fn remote_agent_client_configuration_uses_the_ipc_host() {
        let cases = [
            // Like the Datadog Agent's IPC clients, `localhost` is kept as a name and resolved when connecting.
            ("localhost", "https://localhost:5001/"),
            ("::1", "https://[::1]:5001/"),
            ("127.0.0.2", "https://127.0.0.2:5001/"),
        ];
        for (cmd_host, expected) in cases {
            let mut config = ipc_configuration();
            config.control.ipc.cmd_port = 5001;
            config.control.ipc.cmd_host = cmd_host.to_string();

            let client_config = remote_agent_client_configuration(&config).expect("valid IPC configuration");
            assert_eq!(
                client_config.endpoint().to_string(),
                expected,
                "for cmd_host {cmd_host:?}"
            );
        }
    }

    #[test]
    fn remote_agent_client_configuration_rejects_non_loopback_hosts() {
        let mut config = ipc_configuration();
        config.control.ipc.cmd_host = "10.0.0.1".to_string();
        assert!(remote_agent_client_configuration(&config).is_err());
    }

    #[test]
    fn remote_agent_client_configuration_resolves_default_auth_paths() {
        let client_config = remote_agent_client_configuration(&ipc_configuration()).expect("valid IPC configuration");

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
    fn otlp_needs_at_least_one_signal() {
        let otlp_enabled = |metrics, logs, traces| {
            let mut config = SalukiConfiguration::default();
            config.control.otlp = true;
            config.domains.otlp.receiver.metrics_enabled = metrics;
            config.domains.otlp.receiver.logs_enabled = logs;
            config.control.logs_enabled = logs;
            config.domains.otlp.traces.enabled = traces;
            let dp = DataPlaneConfiguration::from_configuration(&config);
            (dp.otlp_enabled(), dp.otlp_enabled_without_signals())
        };

        assert_eq!(otlp_enabled(false, false, false), (false, true));
        assert_eq!(otlp_enabled(true, false, false), (true, false));
        assert_eq!(otlp_enabled(false, true, false), (true, false));
        assert_eq!(otlp_enabled(false, false, true), (true, false));
    }

    #[test]
    fn otlp_without_signals_activates_no_otlp_pipelines() {
        let mut config = pipeline_configuration(false, false, true, Proxy::default());
        config.domains.otlp.receiver.metrics_enabled = false;
        config.domains.otlp.receiver.logs_enabled = false;
        config.domains.otlp.traces.enabled = false;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.traces_pipeline_required());
        assert!(dp.active_pipelines().is_empty());
    }

    #[test]
    fn remote_agent_client_configuration_uses_typed_auth_paths() {
        let mut config = ipc_configuration();
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
            let mut config = ipc_configuration();
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
        let mut config = ipc_configuration();
        config.control.ipc.vsock_addr = "invalid".to_string();

        assert!(remote_agent_client_configuration(&config).is_err());
    }

    #[test]
    fn workload_settings_only_carry_explicit_paths() {
        let mut config = SalukiConfiguration::default();
        config.control.ipc.remote_agent_string_interner_size_bytes = std::num::NonZeroUsize::new(4096).unwrap();
        config.shared.environment.containerd.socket_path = ConfigValue::defaulted("/run/containerd.sock".into());
        config.shared.environment.containerd.connection_timeout = Duration::from_secs(2);
        config.shared.environment.containerd.query_timeout = Duration::from_secs(7);
        config.shared.environment.container_roots.proc_root = ConfigValue::explicit("/host/proc".into());
        config.shared.environment.container_roots.cgroup_root = ConfigValue::defaulted("/host/sys/fs/cgroup".into());

        let settings = workload_settings(&config);
        assert_eq!(settings.string_interner_size_bytes.get(), 4096);
        assert_eq!(settings.containerd_socket_path, None);
        assert_eq!(settings.containerd_connection_timeout, Duration::from_secs(2));
        assert_eq!(settings.containerd_query_timeout, Duration::from_secs(7));
        assert_eq!(settings.proc_root, Some("/host/proc".into()));
        assert_eq!(settings.cgroup_root, None);
    }

    #[test]
    fn host_settings_require_an_explicit_hostname() {
        let mut config = SalukiConfiguration::default();
        config.shared.environment.hostname = ConfigValue::defaulted("defaulted-host".to_string());
        assert_eq!(host_settings(&config).hostname, None);

        config.shared.environment.hostname = ConfigValue::explicit("explicit-host".to_string());
        assert_eq!(host_settings(&config).hostname.as_deref(), Some("explicit-host"));
    }

    /// Returns OTLP proxy settings that proxy the given signals.
    fn proxy(logs: bool, metrics: bool, traces: bool) -> Proxy {
        Proxy {
            logs_enabled: logs,
            metrics_enabled: metrics,
            traces_enabled: traces,
            ..Proxy::default()
        }
    }

    fn pipeline_configuration(
        checks_enabled: bool, dogstatsd_enabled: bool, otlp_enabled: bool, otlp_proxy: Proxy,
    ) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.control.checks = checks_enabled;
        config.control.dogstatsd = dogstatsd_enabled;
        config.control.otlp = otlp_enabled;
        // Accept every OTLP signal. Logs need both OTLP logs and the logs agent.
        config.domains.otlp.receiver.metrics_enabled = true;
        config.domains.otlp.receiver.logs_enabled = true;
        config.control.logs_enabled = true;
        config.domains.otlp.traces.enabled = true;
        config.domains.otlp.proxy = otlp_proxy;
        config
    }

    #[test]
    fn otlp_logs_are_accepted_only_with_the_logs_agent() {
        let otlp_signals = |otlp_logs, logs_agent| {
            let mut config = pipeline_configuration(false, false, true, Proxy::default());
            config.domains.otlp.receiver.logs_enabled = otlp_logs;
            config.control.logs_enabled = logs_agent;
            config.domains.otlp.traces.enabled = false;
            let dp = DataPlaneConfiguration::from_configuration(&config);
            (dp.otlp_signals(), dp.logs_pipeline_required())
        };
        let signals = |logs| OtlpSignals {
            metrics: true,
            logs,
            traces: false,
        };

        assert_eq!(otlp_signals(true, true), (signals(true), true));
        // Like the Datadog Agent, OTLP logs need the logs agent, and both default to off.
        assert_eq!(otlp_signals(true, false), (signals(false), false));
        assert_eq!(otlp_signals(false, true), (signals(false), false));
        assert_eq!(otlp_signals(false, false), (signals(false), false));
    }

    #[test]
    fn each_otlp_signal_is_routed_on_its_own() {
        // Every combination of the accepted signals, their proxy flags, OTLP ingest, and receiver TLS. Standalone mode
        // keeps the liveness pipelines out of the predicates.
        for row in 0..256u32 {
            let bit = |n: u32| row & (1 << n) != 0;
            let (accepted, flags) = ([bit(0), bit(1), bit(2)], [bit(3), bit(4), bit(5)]);
            let (otlp, tls_configured) = (bit(6), bit(7));

            let mut config = pipeline_configuration(false, false, otlp, proxy(flags[1], flags[0], flags[2]));
            config.control.standalone_mode = true;
            config.domains.otlp.receiver.metrics_enabled = accepted[0];
            config.domains.otlp.receiver.logs_enabled = accepted[1];
            config.control.logs_enabled = accepted[1];
            config.domains.otlp.traces.enabled = accepted[2];
            config.domains.otlp.receiver.http.tls_configured = tls_configured;
            let dp = DataPlaneConfiguration::from_configuration(&config);

            // Any flag puts ADP in proxy mode, and proxy mode with TLS blocks OTLP ingest.
            let blocked = otlp && flags.contains(&true) && tls_configured;
            let enabled = otlp && !blocked && accepted.contains(&true);
            let route = |signal: usize| match (enabled && accepted[signal], flags[signal]) {
                (false, _) => OtlpRoute::Disabled,
                (true, true) => OtlpRoute::Proxied,
                (true, false) => OtlpRoute::Internal,
            };
            let expected = OtlpRoutes {
                metrics: route(0),
                logs: route(1),
                traces: route(2),
            };
            let internal = |route: OtlpRoute| route == OtlpRoute::Internal;

            let row = format!("accepted {accepted:?}, flags {flags:?}, otlp {otlp}, tls {tls_configured}");
            assert_eq!(dp.otlp_routes(), expected, "{row}");
            assert_eq!(dp.otlp_blocked(), blocked, "{row}");
            assert_eq!(dp.otlp_enabled(), enabled, "{row}");
            assert_eq!(dp.traces_pipeline_required(), internal(expected.traces), "{row}");
            assert_eq!(dp.logs_pipeline_required(), internal(expected.logs), "{row}");
            // The trace sampler reports its telemetry as metrics.
            assert_eq!(
                dp.metrics_pipeline_required(),
                internal(expected.metrics) || internal(expected.traces),
                "{row}"
            );

            let pipelines = dp.active_pipelines();
            assert_eq!(pipelines.contains(&Pipeline::Otlp), enabled, "{row}");
            assert_eq!(
                pipelines.contains(&Pipeline::Traces),
                internal(expected.traces),
                "{row}"
            );
            assert_eq!(pipelines.contains(&Pipeline::Logs), internal(expected.logs), "{row}");
        }
    }

    #[test]
    fn proxying_metrics_keeps_logs_and_traces_internal() {
        let config = pipeline_configuration(false, false, true, proxy(false, true, false));
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(
            dp.otlp_routes(),
            OtlpRoutes {
                metrics: OtlpRoute::Proxied,
                logs: OtlpRoute::Internal,
                traces: OtlpRoute::Internal,
            }
        );
    }

    #[test]
    fn a_proxy_flag_for_a_signal_that_is_not_accepted_selects_proxy_mode_only() {
        let mut config = pipeline_configuration(false, false, true, proxy(true, false, false));
        config.control.logs_enabled = false;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.otlp_proxy_mode());
        assert_eq!(
            dp.otlp_routes(),
            OtlpRoutes {
                metrics: OtlpRoute::Internal,
                logs: OtlpRoute::Disabled,
                traces: OtlpRoute::Internal,
            }
        );
    }

    // Pipeline-requirement predicates. Each scenario is chosen to walk one documented branch of the
    // `*_pipeline_required` predicates on `DataPlaneConfiguration`, asserting the full predicate set so that a
    // regression in any single predicate surfaces.

    #[test]
    fn dogstatsd_only_requires_metrics_events_and_service_checks_pipelines() {
        let config = pipeline_configuration(false, true, false, Proxy::default());
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
        let config = pipeline_configuration(true, false, false, Proxy::default());
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
        assert!(dp.active_pipelines().contains(&Pipeline::Logs));
    }

    #[test]
    fn otlp_without_proxy_requires_metrics_logs_and_traces_pipelines() {
        let config = pipeline_configuration(false, false, true, Proxy::default());
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(dp.traces_pipeline_required());
        assert!(dp.active_pipelines().contains(&Pipeline::Logs));
    }

    #[test]
    fn standalone_otlp_proxy_mode_does_not_require_liveness_baseline_pipelines() {
        // Standalone OTLP proxy mode must only construct the local proxy path, which avoids resolving output endpoints.
        let mut config = pipeline_configuration(false, false, true, proxy(true, true, true));
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
        let config = pipeline_configuration(false, false, true, proxy(true, true, true));
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
        assert!(!dp.active_pipelines().contains(&Pipeline::Logs));
    }

    #[test]
    fn otlp_proxy_mode_with_local_traces_requires_liveness_and_traces_pipelines() {
        // Proxy mode is enabled but trace proxying is turned off, so ADP must handle traces locally. The liveness
        // metric and service-check baselines remain required regardless of the OTLP routing.
        let config = pipeline_configuration(false, false, true, proxy(true, true, false));
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(dp.traces_pipeline_required());
        assert!(!dp.active_pipelines().contains(&Pipeline::Logs));
    }

    #[test]
    fn otlp_proxy_mode_is_off_when_no_signal_is_proxied() {
        let config = pipeline_configuration(false, false, true, proxy(false, false, false));
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.otlp_proxy_mode());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(dp.traces_pipeline_required());
    }

    #[test]
    fn standalone_otlp_proxy_traces_only_keeps_metrics_and_logs_internal() {
        let mut config = pipeline_configuration(false, false, true, proxy(false, false, true));
        config.control.standalone_mode = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.otlp_proxy_mode());
        assert!(dp.metrics_pipeline_required());
        assert!(dp.logs_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn standalone_otlp_proxy_metrics_only_processes_traces_and_logs() {
        let mut config = pipeline_configuration(false, false, true, proxy(false, true, false));
        config.control.standalone_mode = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.otlp_proxy_mode());
        assert!(dp.logs_pipeline_required());
        assert!(dp.traces_pipeline_required());
        // The trace sampler's telemetry needs the metrics pipeline; OTLP metrics themselves are proxied.
        assert!(dp.metrics_pipeline_required());
    }

    #[test]
    fn standalone_local_traces_requires_metrics_pipeline_for_sampler_telemetry() {
        // The sampler reports its telemetry as metric events, so a running traces pipeline requires
        // the metrics pipeline even in standalone mode.
        let mut config = pipeline_configuration(false, false, true, proxy(true, true, false));
        config.control.standalone_mode = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.traces_pipeline_required());
        assert!(dp.metrics_pipeline_required());
    }

    #[test]
    fn connected_apm_only_requires_liveness_baseline_pipelines() {
        // The APM relay forwards to the trace-agent what it does not handle itself, so it needs no baseline pipeline of
        // its own, but connected mode still forwards the liveness metric and service check.
        let mut config = pipeline_configuration(false, false, false, Proxy::default());
        config.control.apm = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn standalone_apm_only_requires_no_baseline_pipelines() {
        // The traces pipeline would have no events source to feed it, so the relay connects to the forwarder alone.
        let mut config = pipeline_configuration(false, false, false, Proxy::default());
        config.control.apm = true;
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
    fn no_pipelines_enabled_requires_no_baseline_pipelines() {
        let config = pipeline_configuration(false, false, false, Proxy::default());
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(!dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    /// Returns a configuration that enables OTLP and DogStatsD, accepts only OTLP metrics, proxies the given signals,
    /// and sets TLS on the gRPC receiver.
    fn tls_configuration(logs: bool, metrics: bool, traces: bool) -> SalukiConfiguration {
        let mut config = pipeline_configuration(false, true, true, proxy(logs, metrics, traces));
        config.domains.otlp.receiver.logs_enabled = false;
        config.domains.otlp.traces.enabled = false;
        config.domains.otlp.receiver.grpc.tls_configured = true;
        config
    }

    #[test]
    fn receiver_tls_with_any_proxy_flag_blocks_otlp_ingest() {
        // The proxy flag for the enabled metrics signal, and the flags for the disabled logs and traces signals.
        for (logs, metrics, traces) in [(false, true, false), (true, false, false), (false, false, true)] {
            let config = tls_configuration(logs, metrics, traces);
            let dp = DataPlaneConfiguration::from_configuration(&config);

            assert!(dp.otlp_blocked(), "{:?}", (logs, metrics, traces));
            assert!(!dp.otlp_enabled());
            assert!(!dp.otlp_enabled_without_signals());
            assert!(!dp.logs_pipeline_required());
            assert!(!dp.traces_pipeline_required());
            // DogStatsD is unaffected.
            assert!(dp.data_pipelines_enabled());
            assert_eq!(dp.active_pipelines(), HashSet::from([Pipeline::DogStatsD]));
        }
    }

    #[test]
    fn receiver_tls_on_either_protocol_blocks_otlp_ingest() {
        for (grpc, http) in [(true, false), (false, true), (true, true)] {
            let mut config = tls_configuration(true, false, false);
            config.domains.otlp.receiver.grpc.tls_configured = grpc;
            config.domains.otlp.receiver.http.tls_configured = http;
            let dp = DataPlaneConfiguration::from_configuration(&config);

            assert!(dp.otlp_blocked(), "{:?}", (grpc, http));
        }
    }

    #[test]
    fn receiver_tls_without_a_proxy_flag_allows_otlp_ingest() {
        // With every flag false, ADP handles every signal itself, so the TLS is not proxied.
        let config = tls_configuration(false, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.otlp_blocked());
        assert!(dp.otlp_enabled());
        assert!(dp.active_pipelines().contains(&Pipeline::Otlp));
    }

    #[test]
    fn receiver_tls_blocks_nothing_while_otlp_is_off() {
        let mut config = tls_configuration(true, true, true);
        config.control.otlp = false;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(!dp.otlp_blocked());
        assert!(!dp.otlp_enabled());
    }

    #[test]
    fn blocked_message_names_the_protocol_and_the_proxy_flags_that_are_set() {
        let mut config = tls_configuration(true, false, true);
        let dp = DataPlaneConfiguration::from_configuration(&config);
        assert_eq!(
            dp.otlp_blocked_message().as_deref(),
            Some(
                "OTLP receiver TLS is not supported when ADP proxies OTLP signals to the Agent: remove tls from \
                otlp_config.receiver.protocols.grpc, or set data_plane.experimental.otlp.proxy.logs and \
                data_plane.experimental.otlp.proxy.traces to false"
            )
        );

        config.domains.otlp.receiver.grpc.tls_configured = false;
        config.domains.otlp.receiver.http.tls_configured = true;
        config.domains.otlp.proxy.logs_enabled = false;
        config.domains.otlp.proxy.metrics_enabled = true;
        config.domains.otlp.proxy.traces_enabled = false;
        let dp = DataPlaneConfiguration::from_configuration(&config);
        assert_eq!(
            dp.otlp_blocked_message().as_deref(),
            Some(
                "OTLP receiver TLS is not supported when ADP proxies OTLP signals to the Agent: remove tls from \
                otlp_config.receiver.protocols.http, or set data_plane.experimental.otlp.proxy.metrics to false"
            )
        );

        config.domains.otlp.receiver.grpc.tls_configured = true;
        let dp = DataPlaneConfiguration::from_configuration(&config);
        let message = dp.otlp_blocked_message().expect("both protocols block OTLP ingest");
        assert!(
            message.contains(
                "remove tls from otlp_config.receiver.protocols.grpc and otlp_config.receiver.protocols.http,"
            ),
            "{message}"
        );
    }

    #[test]
    fn there_is_no_blocked_message_unless_otlp_is_blocked() {
        let config = tls_configuration(false, false, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.otlp_blocked_message(), None);
    }

    #[test]
    fn a_blocked_otlp_pipeline_reports_no_missing_signals() {
        let mut config = tls_configuration(true, false, false);
        config.domains.otlp.receiver.metrics_enabled = false;
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.otlp_blocked());
        assert!(!dp.otlp_enabled_without_signals());
    }

    /// Returns a configuration that accepts every OTLP signal, proxies the given signals, and listens on `grpc` and
    /// `http`.
    fn listener_configuration(metrics: bool, traces: bool, grpc: &str, http: &str) -> SalukiConfiguration {
        let mut config = pipeline_configuration(false, false, true, proxy(false, metrics, traces));
        let otlp = &mut config.domains.otlp;
        otlp.receiver.grpc.endpoint = grpc.to_string();
        otlp.receiver.http.endpoint = http.to_string();
        otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
        otlp.proxy.http_endpoint = "127.0.0.1:4320".to_string();
        config
    }

    async fn otlp_proxy_loop(config: &SalukiConfiguration) -> Option<String> {
        let dp = DataPlaneConfiguration::from_configuration(config)
            .with_otlp_proxy_loop_check()
            .await;
        dp.otlp_proxy_loop_message().map(str::to_string)
    }

    #[tokio::test]
    async fn an_otlp_proxy_destination_that_reaches_an_otlp_listener_blocks_otlp_ingest() {
        // The Agent's default relocation, configured as ADP's own gRPC listener.
        let config = listener_configuration(true, false, "127.0.0.1:4319", "127.0.0.1:4318");
        let dp = DataPlaneConfiguration::from_configuration(&config)
            .with_otlp_proxy_loop_check()
            .await;

        assert_eq!(
            dp.otlp_proxy_loop_message(),
            Some(
                "The OTLP proxy destination `data_plane.experimental.otlp.proxy.agent_receiver_grpc_endpoint` \
                (127.0.0.1:4319) reaches ADP's own OTLP listener `otlp_config.receiver.protocols.grpc.endpoint` \
                (tcp://127.0.0.1:4319), so ADP would proxy OTLP requests to itself: set \
                `data_plane.experimental.otlp.proxy.agent_receiver_grpc_endpoint` to the Agent's relocated OTLP \
                receiver, or change `otlp_config.receiver.protocols.grpc.endpoint`"
            )
        );
        assert!(!dp.otlp_enabled());
        assert!(!dp.otlp_enabled_without_signals());
        assert_eq!(
            dp.otlp_routes(),
            OtlpRoutes {
                metrics: OtlpRoute::Disabled,
                logs: OtlpRoute::Disabled,
                traces: OtlpRoute::Disabled,
            }
        );
        assert!(!dp.traces_pipeline_required());
        assert!(!dp.active_pipelines().contains(&Pipeline::Otlp));
    }

    #[tokio::test]
    async fn the_otlp_proxy_loop_check_resolves_addresses() {
        // A listener on every interface is reached through the loopback address.
        let config = listener_configuration(true, false, "0.0.0.0:4319", "127.0.0.1:4318");
        let message = otlp_proxy_loop(&config)
            .await
            .expect("a loop through the unspecified address");
        assert!(message.contains("(tcp://0.0.0.0:4319)"), "{message}");

        // A destination name resolves to the listener's address. The destination of either protocol counts.
        let mut config = listener_configuration(true, false, "127.0.0.1:4317", "127.0.0.1:4318");
        config.domains.otlp.proxy.http_endpoint = "localhost:4318".to_string();
        let message = otlp_proxy_loop(&config).await.expect("a loop through a host name");
        assert!(
            message.contains("`data_plane.experimental.otlp.proxy.agent_receiver_http_endpoint` (localhost:4318)"),
            "{message}"
        );
        assert!(
            message.contains("`otlp_config.receiver.protocols.http.endpoint`"),
            "{message}"
        );

        // Different ports don't loop.
        let config = listener_configuration(true, false, "0.0.0.0:4317", "0.0.0.0:4318");
        assert_eq!(otlp_proxy_loop(&config).await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_otlp_proxy_destination_on_the_listener_socket_blocks_otlp_ingest() {
        let mut config = listener_configuration(false, true, "/run/adp/otlp.sock", "127.0.0.1:4318");
        config.domains.otlp.receiver.grpc.transport = GrpcTransport::Unix;
        config.domains.otlp.proxy.grpc_endpoint = "/run/adp/otlp.sock".to_string();
        let message = otlp_proxy_loop(&config).await.expect("a loop through the socket");
        assert!(message.contains("(unix:///run/adp/otlp.sock)"), "{message}");

        config.domains.otlp.proxy.grpc_endpoint = "/run/agent/otlp.sock".to_string();
        assert_eq!(otlp_proxy_loop(&config).await, None);
    }

    #[tokio::test]
    async fn the_otlp_proxy_loop_check_needs_a_proxied_signal() {
        // Every signal is internal.
        let config = listener_configuration(false, false, "127.0.0.1:4319", "127.0.0.1:4320");
        assert_eq!(otlp_proxy_loop(&config).await, None);

        // The only proxy flag is for a signal that isn't accepted, so nothing is proxied.
        let mut config = listener_configuration(false, true, "127.0.0.1:4319", "127.0.0.1:4320");
        config.domains.otlp.traces.enabled = false;
        assert_eq!(otlp_proxy_loop(&config).await, None);

        // A protocol without a listener proxies nothing.
        let config = listener_configuration(true, false, "", "127.0.0.1:4319");
        assert_eq!(otlp_proxy_loop(&config).await, None);
    }

    #[test]
    fn stop_timeout_is_the_configuration_stop_timeout() {
        let mut config = SalukiConfiguration::default();
        config.control.aggregator_stop_timeout = Duration::from_secs(3);
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(7);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.stop_timeout(), Duration::from_secs(10));
    }
}

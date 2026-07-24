use std::collections::HashSet;
use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::classifier::Pipeline;
use saluki_io::net::ListenAddress;

/// General data plane configuration.
///
/// This wrapper provides orchestration-level accessors and pipeline decisions over the typed configuration. It lives
/// during bootstrap and topology construction and is then discarded.
#[derive(Clone, Debug)]
pub struct DataPlaneConfiguration<'a> {
    config: &'a SalukiConfiguration,
}

impl<'a> DataPlaneConfiguration<'a> {
    /// Creates a new `DataPlaneConfiguration` instance from the given configuration.
    pub fn from_configuration(config: &'a SalukiConfiguration) -> Self {
        Self { config }
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
    /// Uses `data_plane.stop_timeout_seconds` when configured. Otherwise, it sums
    /// `aggregator_stop_timeout` and `forwarder_stop_timeout`, returning 30 seconds if the sum
    /// overflows.
    pub fn stop_timeout(&self) -> Duration {
        match self.config.control.stop_timeout_seconds {
            Some(seconds) => Duration::from_secs(seconds),
            None => self
                .config
                .control
                .aggregator_stop_timeout
                .checked_add(self.config.shared.endpoints.forwarder.stop_timeout)
                // HACK: arbitrary fallback to make the function infallible
                .unwrap_or(Duration::from_secs(30)),
        }
    }

    /// Returns a reference to the API listen address
    ///
    /// This is also referred to as the "unprivileged" API.
    pub const fn api_listen_address(&self) -> &ListenAddress {
        &self.config.control.api_listen_address
    }

    /// Returns a reference to the secure API listen address.
    ///
    /// This is also referred to as the "privileged" API.
    pub const fn secure_api_listen_address(&self) -> &ListenAddress {
        &self.config.control.secure_api_listen_address
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
    /// This indicates that the "baseline" metrics pipeline (aggregation, enrichment, encoding, forwarding) is required
    /// by higher-level data pipelines, such as DogStatsD.
    pub const fn metrics_pipeline_required(&self) -> bool {
        // We consider the metrics pipeline to be enabled if:
        // - Checks is enabled
        // - DogStatsD is enabled
        // - OTLP is enabled and not in proxy mode
        self.checks_enabled() || self.dogstatsd_enabled() || (self.otlp_enabled() && !self.otlp_proxy_enabled())
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
    /// This indicates that the "baseline" service checks pipeline (encoding, forwarding) is required by higher-level
    /// data pipelines, such as Checks or DogStatsD.
    pub const fn service_checks_pipeline_required(&self) -> bool {
        self.checks_enabled() || self.dogstatsd_enabled()
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
    use super::*;

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
        assert!(!dp.service_checks_pipeline_required());
        assert!(dp.traces_pipeline_required());
    }

    #[test]
    fn otlp_proxy_mode_proxying_all_signals_requires_no_baseline_pipelines() {
        // With proxy mode enabled and traces still proxied to the Core Agent (the default), ADP handles no signals
        // itself, so no baseline pipeline is required even though a data pipeline (OTLP) is enabled.
        let config = pipeline_configuration(false, false, true, true, true);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(!dp.service_checks_pipeline_required());
        assert!(!dp.traces_pipeline_required());
    }

    #[test]
    fn otlp_proxy_mode_with_local_traces_requires_traces_pipeline() {
        // Proxy mode is enabled but trace proxying is turned off, so ADP must handle traces locally and the traces
        // pipeline becomes required again while the other baseline pipelines stay off.
        let config = pipeline_configuration(false, false, true, true, false);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert!(dp.data_pipelines_enabled());
        assert!(!dp.metrics_pipeline_required());
        assert!(!dp.logs_pipeline_required());
        assert!(!dp.events_pipeline_required());
        assert!(!dp.service_checks_pipeline_required());
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
        config.control.stop_timeout_seconds = Some(11);
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
    fn stop_timeout_uses_fallback_when_sum_overflows() {
        let mut config = SalukiConfiguration::default();
        config.control.aggregator_stop_timeout = Duration::MAX;
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(1);
        let dp = DataPlaneConfiguration::from_configuration(&config);

        assert_eq!(dp.stop_timeout(), Duration::from_secs(30));
    }
}

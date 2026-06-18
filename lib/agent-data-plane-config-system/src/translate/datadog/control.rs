//! Control-domain translation: the `data_plane.*` topology-shaping keys that land in
//! [`ControlConfiguration`] rather than a component slice.
//!
//! These are the witnessed `data_plane.*` keys plus the two stop-timeout components. The pipeline
//! gates `data_plane.enabled` and `data_plane.dogstatsd.enabled` are now part of the witness set
//! (they exist in the Datadog Agent core schema). The gates `data_plane.checks.enabled`,
//! `data_plane.otlp.enabled`, and `data_plane.standalone_mode` are not in the Datadog schema and
//! take their seeded/default values.
//!
//! Mirrors the binary's original `data_plane.*` -> control mapping in `bin/agent-data-plane/src/config.rs`.

use agent_data_plane_config::ControlConfiguration;
use datadog_agent_config::TranslateError;
use saluki_io::net::ListenAddress;

use crate::translate::Translator;

/// `data_plane.enabled` -> whether ADP is enabled (the master pipeline gate).
pub fn set_enabled(control: &mut ControlConfiguration, value: bool) {
    control.enabled = value;
}

/// `data_plane.dogstatsd.enabled` -> whether the DogStatsD pipeline is enabled.
pub fn set_dogstatsd_enabled(control: &mut ControlConfiguration, value: bool) {
    control.dogstatsd.enabled = value;
}

/// `data_plane.api_listen_address` -> the unprivileged API listen address.
///
/// On a malformed address, records an error against the key and leaves the seeded/default value in
/// place.
pub fn set_api_listen_address(control: &mut ControlConfiguration, value: String) -> Result<(), TranslateError> {
    match ListenAddress::try_from(value) {
        Ok(addr) => {
            control.api_listen_address = addr;
            Ok(())
        }
        Err(reason) => Err(TranslateError::for_key(
            "data_plane.api_listen_address",
            reason.to_string(),
        )),
    }
}

/// `data_plane.secure_api_listen_address` -> the privileged API listen address.
///
/// On a malformed address, records an error against the key and leaves the seeded/default value in
/// place.
pub fn set_secure_api_listen_address(control: &mut ControlConfiguration, value: String) -> Result<(), TranslateError> {
    match ListenAddress::try_from(value) {
        Ok(addr) => {
            control.secure_api_listen_address = addr;
            Ok(())
        }
        Err(reason) => Err(TranslateError::for_key(
            "data_plane.secure_api_listen_address",
            reason.to_string(),
        )),
    }
}

/// `data_plane.remote_agent_enabled` -> whether ADP registers as a remote agent.
pub fn set_remote_agent_enabled(control: &mut ControlConfiguration, value: bool) {
    control.remote_agent_enabled = value;
}

/// `data_plane.use_new_config_stream_endpoint` -> whether to use the new config-stream endpoint.
pub fn set_use_new_config_stream_endpoint(control: &mut ControlConfiguration, value: bool) {
    control.use_new_config_stream_endpoint = value;
}

/// `data_plane.log_file` -> the ADP process log-file path.
pub fn set_log_file(control: &mut ControlConfiguration, value: String) {
    control.log_file = value;
}

/// `data_plane.otlp.proxy.traces.enabled` -> whether OTLP traces are proxied to the Core Agent.
pub fn set_otlp_proxy_traces_enabled(control: &mut ControlConfiguration, value: bool) {
    control.otlp.proxy.proxy_traces = value;
}

/// `data_plane.otlp.proxy.metrics.enabled` -> whether OTLP metrics are proxied to the Core Agent.
pub fn set_otlp_proxy_metrics_enabled(control: &mut ControlConfiguration, value: bool) {
    control.otlp.proxy.proxy_metrics = value;
}

/// `data_plane.otlp.proxy.logs.enabled` -> whether OTLP logs are proxied to the Core Agent.
pub fn set_otlp_proxy_logs_enabled(control: &mut ControlConfiguration, value: bool) {
    control.otlp.proxy.proxy_logs = value;
}

/// `aggregator_stop_timeout` (seconds) -> stashed for the `control.stop_timeout` derivation.
pub fn set_aggregator_stop_timeout(t: &mut Translator, value: i64) {
    t.set_aggregator_stop_timeout_secs(value.max(0) as u64);
}

/// `forwarder_stop_timeout` (seconds) -> stashed for the `control.stop_timeout` derivation.
pub fn set_forwarder_stop_timeout(t: &mut Translator, value: i64) {
    t.set_forwarder_stop_timeout_secs(value.max(0) as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_address_parses_and_errors() {
        let mut control = ControlConfiguration::default();
        set_api_listen_address(&mut control, "tcp://0.0.0.0:6000".to_string()).expect("valid address");
        assert_eq!(control.api_listen_address, ListenAddress::any_tcp(6000));
        assert!(set_api_listen_address(&mut control, "not-an-address".to_string()).is_err());
    }
}

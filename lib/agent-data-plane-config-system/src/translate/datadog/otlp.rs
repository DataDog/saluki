//! OTLP-domain translation: the OTLP source receiver/signal settings, the decoder/relay traces
//! settings, and the forwarder's Core Agent target.
//!
//! Mirrors the conversions the original OTLP component constructors performed. The native model
//! carries the receiver and per-signal config on the OTLP source (`otlp.source.otlp_config`) and a
//! `TracesConfig` copy on the decoder and relay; OTLP-traces keys fan out to the source and decoder
//! copies so they stay consistent.

use saluki_component_config::otlp::{OtlpSourceConfig, TracesConfig};

use crate::translate::Translator;

// ----- receiver -----

/// `otlp_config.receiver.protocols.grpc.endpoint` -> OTLP gRPC receiver endpoint.
pub fn set_grpc_endpoint(source: &mut OtlpSourceConfig, value: String) {
    source.otlp_config.receiver.protocols.grpc.endpoint = value;
}

/// `otlp_config.receiver.protocols.grpc.transport` -> OTLP gRPC receiver transport.
pub fn set_grpc_transport(source: &mut OtlpSourceConfig, value: String) {
    source.otlp_config.receiver.protocols.grpc.transport = value;
}

/// `otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib` -> OTLP gRPC max receive size (MiB).
pub fn set_grpc_max_recv_msg_size_mib(source: &mut OtlpSourceConfig, value: i64) {
    source.otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib = value.max(0) as u64;
}

/// `otlp_config.receiver.protocols.http.endpoint` -> OTLP HTTP receiver endpoint.
pub fn set_http_endpoint(source: &mut OtlpSourceConfig, value: String) {
    source.otlp_config.receiver.protocols.http.endpoint = value;
}

// ----- per-signal enables -----

/// `otlp_config.metrics.enabled` -> OTLP metrics support.
pub fn set_metrics_enabled(source: &mut OtlpSourceConfig, value: bool) {
    source.otlp_config.metrics.enabled = value;
}

/// `otlp_config.logs.enabled` -> OTLP logs support.
pub fn set_logs_enabled(source: &mut OtlpSourceConfig, value: bool) {
    source.otlp_config.logs.enabled = value;
}

// ----- traces (fan out to source + decoder + relay copies) -----

/// Returns mutable references to every native OTLP `TracesConfig` derived from the Datadog
/// `otlp_config.traces.*` keys.
///
/// The relay carries only a receiver (no `TracesConfig`), so only the source and decoder copies are
/// kept in sync here.
fn traces_targets(t: &mut Translator) -> Vec<&mut TracesConfig> {
    let native = t.native_mut();
    vec![
        &mut native.components.otlp.source.otlp_config.traces,
        &mut native.components.otlp.decoder.traces,
    ]
}

/// Applies `f` to every OTLP `TracesConfig` copy.
fn for_each_traces(t: &mut Translator, f: impl Fn(&mut TracesConfig)) {
    for target in traces_targets(t) {
        f(target);
    }
}

/// `otlp_config.traces.enabled` -> OTLP traces support.
pub fn set_traces_enabled(t: &mut Translator, value: bool) {
    for_each_traces(t, |tr| tr.enabled = value);
}

/// `otlp_config.traces.internal_port` -> internal port for forwarding OTLP traces to the Core Agent.
pub fn set_traces_internal_port(t: &mut Translator, value: i64) {
    let port = value.clamp(0, u16::MAX as i64) as u16;
    for_each_traces(t, |tr| tr.internal_port = port);
    t.native_mut().components.otlp.forwarder.core_agent_traces_internal_port = port;
}

/// `otlp_config.traces.probabilistic_sampler.sampling_percentage` -> OTLP traces sampling
/// percentage.
pub fn set_traces_sampling_percentage(t: &mut Translator, value: f64) {
    for_each_traces(t, |tr| tr.probabilistic_sampler.sampling_percentage = value);
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::SalukiOnlyConfiguration;

    use super::*;

    #[test]
    fn traces_enable_fans_out_to_source_and_decoder() {
        let mut t = Translator::new(SalukiOnlyConfiguration::default().seed());
        set_traces_enabled(&mut t, false);
        set_traces_internal_port(&mut t, 6000);
        let native = t.finish();
        assert!(!native.components.otlp.source.otlp_config.traces.enabled);
        assert!(!native.components.otlp.decoder.traces.enabled);
        assert_eq!(native.components.otlp.source.otlp_config.traces.internal_port, 6000);
        assert_eq!(native.components.otlp.forwarder.core_agent_traces_internal_port, 6000);
    }
}

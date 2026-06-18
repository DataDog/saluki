use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "otlp_config.logs.enabled" => {
            let enabled = bool_value(value);
            translator.native.control.otlp.logs.enabled = enabled;
            translator.native.components.otlp.source.logs_enabled = enabled;

            None
        }
        "otlp_config.metrics.enabled" => {
            let enabled = bool_value(value);
            translator.native.control.otlp.metrics.enabled = enabled;
            translator.native.components.otlp.source.metrics_enabled = enabled;

            None
        }
        "otlp_config.traces.enabled" => {
            let enabled = bool_value(value);
            translator.native.control.otlp.traces.enabled = enabled;
            translator.native.components.otlp.source.traces_enabled = enabled;

            None
        }
        "otlp_config.receiver.protocols.grpc.endpoint" => {
            translator.native.components.otlp.source.grpc_endpoint = string_value(value);

            None
        }
        "otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib" => {
            translator.native.components.otlp.source.grpc_max_recv_msg_size_mib = u64_value(value, 4);

            None
        }
        "otlp_config.receiver.protocols.grpc.transport" => {
            translator.native.components.otlp.source.grpc_transport = string_value(value);

            None
        }
        "otlp_config.receiver.protocols.http.endpoint" => {
            translator.native.components.otlp.source.http_endpoint = string_value(value);

            None
        }
        "otlp_config.traces.internal_port" => {
            translator.native.components.otlp.source.traces_internal_port = u16_value(value, 5003);

            None
        }
        "otlp_config.traces.probabilistic_sampler.sampling_percentage" => {
            let percentage = f64_value(value, 100.0);
            translator.native.components.otlp.source.traces_sampling_percentage = percentage;
            translator.native.components.traces.sampler.rate = percentage / 100.0;

            None
        }
        _ => Some(value),
    }
}

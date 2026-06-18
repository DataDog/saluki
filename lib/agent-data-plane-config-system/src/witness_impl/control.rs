use agent_data_plane_config::PipelineGate;
use saluki_component_config::ListenAddress;
use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "agent_ipc.grpc_max_message_size" => {
            translator.native.control.agent_ipc_grpc_max_message_size = Some(usize_value(value, 0));

            None
        }
        "aggregator_stop_timeout" => {
            translator.aggregator_stop_timeout_secs = u64_value(value, 2);
            None
        }
        "auth_token_file_path" => {
            translator.native.control.ipc_auth.auth_token_file_path = optional_string_value(value);

            None
        }
        "cmd_port" => {
            translator.native.control.cmd_port = Some(u16_value(value, 5001));

            None
        }
        "data_plane.api_listen_address" => {
            translator.native.control.api_listen_address = ListenAddress::Tcp(string_value(value));

            None
        }
        "data_plane.dogstatsd.enabled" => {
            translator.native.control.dogstatsd = PipelineGate {
                enabled: bool_value(value),
            };

            None
        }
        "data_plane.enabled" => {
            translator.native.control.enabled = bool_value(value);

            None
        }
        "data_plane.log_file" => {
            translator.native.control.data_plane_log_file = optional_string_value(value);

            None
        }
        "data_plane.otlp.enabled" => {
            translator.native.control.otlp.native = PipelineGate {
                enabled: bool_value(value),
            };

            None
        }
        "data_plane.otlp.proxy.enabled" => {
            translator.native.control.otlp.proxy.enabled = bool_value(value);

            None
        }
        "data_plane.otlp.proxy.logs.enabled" => {
            translator.native.control.otlp.logs = PipelineGate {
                enabled: bool_value(value),
            };

            None
        }
        "data_plane.otlp.proxy.metrics.enabled" => {
            translator.native.control.otlp.metrics = PipelineGate {
                enabled: bool_value(value),
            };

            None
        }
        "data_plane.otlp.proxy.receiver.protocols.grpc.endpoint" => {
            translator.native.control.otlp.proxy.core_agent_otlp_grpc_endpoint = string_value(value);

            None
        }
        "data_plane.otlp.proxy.traces.enabled" => {
            translator.native.control.otlp.traces = PipelineGate {
                enabled: bool_value(value),
            };

            None
        }
        "data_plane.remote_agent_enabled" => {
            translator.native.control.remote_agent_enabled = bool_value(value);
            None
        }
        "data_plane.secure_api_listen_address" => {
            translator.native.control.secure_api_listen_address = ListenAddress::Tcp(string_value(value));

            None
        }
        "data_plane.use_new_config_stream_endpoint" => {
            translator.native.control.use_new_config_stream_endpoint = bool_value(value);

            None
        }
        "env" => {
            translator.native.control.env = optional_string_value(value);

            None
        }
        "forwarder_stop_timeout" => {
            translator.forwarder_stop_timeout_secs = u64_value(value, 2);
            None
        }
        "ipc_cert_file_path" => {
            translator.native.control.ipc_auth.ipc_cert_file_path = optional_string_value(value);

            None
        }
        "log_format_rfc3339" => {
            translator.native.control.log_format_rfc3339 = Some(bool_value(value));

            None
        }
        "log_level" => {
            translator.native.control.log_level = Some(string_value(value));
            None
        }
        "syslog_rfc" => {
            translator.native.control.syslog_rfc = Some(bool_value(value));

            None
        }
        "syslog_uri" => {
            translator.native.control.syslog_uri = optional_string_value(value);

            None
        }
        "vsock_addr" => {
            translator.native.control.vsock_addr = optional_string_value(value);

            None
        }
        _ => Some(value),
    }
}

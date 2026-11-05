//! Reads only the configuration keys needed to plan Agent listeners and omit environment defaults.

use std::{borrow::Cow, collections::BTreeMap};

use panoramic_core::participant::require_early_config_value;
use serde_json::Value;

const NAME: &str = "datadog.yaml";

pub(crate) struct ConfigFile(Value);

impl ConfigFile {
    pub(crate) fn parse(files: &BTreeMap<String, Vec<u8>>) -> Result<Self, String> {
        let Some(bytes) = files.get(NAME) else {
            return Ok(Self(Value::Object(Default::default())));
        };
        let text = simdutf8::basic::from_utf8(bytes).map_err(|error| format!("{NAME} must be UTF-8: {error}"))?;
        let value: Value = serde_saphyr::from_str(text).map_err(|error| format!("invalid {NAME}: {error}"))?;
        if value.is_null() {
            return Ok(Self(Value::Object(Default::default())));
        }
        if !value.is_object() {
            return Err(format!("{NAME} must contain a configuration mapping"));
        }
        Ok(Self(value))
    }

    pub(crate) fn omit_defaults(&self, env: &mut BTreeMap<String, String>) {
        env.retain(|name, _| {
            let Some(suffix) = name.strip_prefix("DD_") else {
                return true;
            };
            let key = file_key(suffix);
            self.lookup(&key).is_none()
        });
    }

    pub(crate) fn value(&self, suffix: &str) -> Result<Option<(String, Cow<'_, str>)>, String> {
        let key = file_key(suffix);
        let Some(value) = self.lookup(&key) else {
            return Ok(None);
        };
        let value = match value {
            Value::String(value) => {
                require_early_config_value(NAME, &key, value)?;
                Cow::Borrowed(value.as_str())
            }
            Value::Bool(value) => Cow::Owned(value.to_string()),
            Value::Number(value) => Cow::Owned(value.to_string()),
            _ => {
                return Err(format!(
                    "configuration file '{NAME}' key '{key}' must be a scalar value"
                ))
            }
        };
        Ok(Some((format!("{NAME} key '{key}'"), value)))
    }

    fn lookup(&self, key: &str) -> Option<&Value> {
        key.split('.').try_fold(&self.0, |value, segment| {
            value
                .as_object()?
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(segment))
                .map(|(_, value)| value)
        })
    }
}

fn file_key(suffix: &str) -> String {
    match suffix {
        "APM_RECEIVER_PORT" => "apm_config.receiver_port",
        "PROCESS_CONFIG_CMD_PORT" => "process_config.cmd_port",
        "AGENT_IPC_PORT" => "agent_ipc.port",
        "DATA_PLANE_ENABLED" => "data_plane.enabled",
        "DATA_PLANE_DOGSTATSD_ENABLED" => "data_plane.dogstatsd.enabled",
        "DATA_PLANE_API_LISTEN_ADDRESS" => "data_plane.api_listen_address",
        "DATA_PLANE_SECURE_API_LISTEN_ADDRESS" => "data_plane.secure_api_listen_address",
        "DATA_PLANE_TELEMETRY_LISTEN_ADDR" => "data_plane.telemetry_listen_addr",
        "DATA_PLANE_OTLP_ENABLED" => "data_plane.otlp.enabled",
        "DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY" => "data_plane.otlp.receiver_grpc_endpoint_temporary",
        "DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY" => "data_plane.otlp.receiver_http_endpoint_temporary",
        "OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT" => "otlp_config.receiver.protocols.grpc.endpoint",
        "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT" => "otlp_config.receiver.protocols.http.endpoint",
        "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE" => "otlp_config.receiver.protocols.http.tls.cert_file",
        "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE" => "otlp_config.receiver.protocols.http.tls.key_file",
        _ => return suffix.to_ascii_lowercase(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        case::FileSource,
        endpoint::{
            test_support::{assert_network_endpoint, assert_socket_endpoint},
            EndpointKind, Evidence,
        },
        layout::{Os, Realization, RealizationKind},
        participant::{CaseValues, EffectiveConfig, ParticipantType},
    };
    use serde_json::json;

    use super::*;
    use crate::{Agent, AgentAdp};

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    fn case(yaml: &str, env: &[(&str, &str)]) -> CaseValues {
        CaseValues {
            env: env
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            files: BTreeMap::from([(
                NAME.into(),
                FileSource {
                    from: NAME.into(),
                    template: true,
                },
            )]),
            file_contents: BTreeMap::from([(NAME.into(), yaml.as_bytes().to_vec())]),
            ..Default::default()
        }
    }

    fn effective(participant: &dyn ParticipantType, case: CaseValues) -> EffectiveConfig {
        participant.validate_config(LINUX, &case).unwrap();
        let mut env = participant.default_env(LINUX, &case);
        env.extend(case.env);
        EffectiveConfig {
            env,
            files: case.file_contents,
            params: case.params,
            endpoints: case.endpoints,
        }
    }

    #[test]
    fn every_container_default_yields_to_its_file_key() {
        let defaults = AgentAdp.default_env(LINUX, &CaseValues::default());
        let entries = [
            ("DD_CMD_PORT", json!({"cmd_port": 5002})),
            ("DD_GUI_PORT", json!({"GUI_port": 5003})),
            ("DD_EXPVAR_PORT", json!({"expvar_port": 5004})),
            ("DD_APM_RECEIVER_PORT", json!({"apm_config": {"receiver_port": 5005}})),
            (
                "DD_PROCESS_CONFIG_CMD_PORT",
                json!({"process_config": {"cmd_port": 5006}}),
            ),
            ("DD_AGENT_IPC_PORT", json!({"agent_ipc": {"port": 5007}})),
            ("DD_DOGSTATSD_PORT", json!({"dogstatsd_port": 0})),
            (
                "DD_DATA_PLANE_API_LISTEN_ADDRESS",
                json!({"data_plane": {"api_listen_address": "tcp://0.0.0.0:5008"}}),
            ),
            (
                "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
                json!({"data_plane": {"secure_api_listen_address": "tcp://0.0.0.0:5009"}}),
            ),
            (
                "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR",
                json!({"data_plane": {"telemetry_listen_addr": "tcp://0.0.0.0:5010"}}),
            ),
            (
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
                json!({"otlp_config": {"receiver": {"protocols": {"grpc": {"endpoint": "0.0.0.0:5011"}}}}}),
            ),
            (
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
                json!({"otlp_config": {"receiver": {"protocols": {"http": {"endpoint": "0.0.0.0:5012"}}}}}),
            ),
        ];
        assert_eq!(entries.len(), defaults.len());
        for (env_key, file) in entries {
            let values = case(&file.to_string(), &[]);
            AgentAdp.validate_config(LINUX, &values).unwrap();
            let mut expected = defaults.clone();
            assert!(expected.remove(env_key).is_some());
            assert_eq!(AgentAdp.default_env(LINUX, &values), expected, "{env_key}");
            let values = case(&file.to_string(), &[(env_key, &defaults[env_key])]);
            let config = effective(&AgentAdp, values);
            assert_eq!(config.env[env_key], defaults[env_key]);
        }
    }

    #[test]
    fn file_socket_paths_and_empty_values_control_each_transport_for_both_products() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for (key, endpoint, kind) in [
                ("dogstatsd_socket", "dogstatsd_uds", EndpointKind::UnixDatagram),
                (
                    "dogstatsd_stream_socket",
                    "dogstatsd_uds_stream",
                    EndpointKind::UnixStream,
                ),
            ] {
                for path in ["", "/airlock/subject/shared/metrics.sock"] {
                    let config = effective(participant, case(&json!({key: path}).to_string(), &[]));
                    let endpoints = participant.endpoints(LINUX, &config).unwrap();
                    if path.is_empty() {
                        assert!(!endpoints.contains_key(endpoint));
                    } else {
                        assert_socket_endpoint(
                            &endpoints[endpoint],
                            kind,
                            path,
                            Some(Evidence::socket_exists(endpoint)),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn file_dogstatsd_ports_are_not_overridden_by_type_defaults_or_recipe_environment() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for port in [0, 8125, 65535] {
                let config = effective(
                    participant,
                    case(
                        &format!("dogstatsd_port: {port}\ndogstatsd_non_local_traffic: true"),
                        &[],
                    ),
                );
                assert!(!config.env.contains_key("DD_DOGSTATSD_PORT"));
                let endpoints = participant.endpoints(LINUX, &config).unwrap();
                if port == 0 {
                    assert!(!endpoints.contains_key("dogstatsd_udp"));
                } else {
                    assert_network_endpoint(&endpoints["dogstatsd_udp"], EndpointKind::Udp, port, None);
                }
                let rendered = panoramic_core::recipe::test_support::rendered(config.env.clone(), config.files);
                let recipe = participant.recipe(LINUX, &rendered).unwrap();
                panoramic_core::recipe::test_support::assert_image_recipe(&recipe, AgentAdp::LINUX_IMAGE, &config.env);
            }
        }
    }

    #[test]
    fn environment_then_adp_aliases_override_file_keys_only_for_their_owner() {
        for (key, endpoint, kind, file_value, dd_value, adp_value) in [
            (
                "DOGSTATSD_PORT",
                "dogstatsd_udp",
                EndpointKind::Udp,
                "8125",
                "9125",
                "10125",
            ),
            (
                "DOGSTATSD_SOCKET",
                "dogstatsd_uds",
                EndpointKind::UnixDatagram,
                "/file.sock",
                "/env.sock",
                "/adp.sock",
            ),
            (
                "DOGSTATSD_STREAM_SOCKET",
                "dogstatsd_uds_stream",
                EndpointKind::UnixStream,
                "/file.sock",
                "/env.sock",
                "/adp.sock",
            ),
        ] {
            let yaml = format!(
                "{}: '{file_value}'\ndogstatsd_non_local_traffic: true",
                key.to_lowercase()
            );
            let dd_key = format!("DD_{key}");
            let adp_key = format!("ADP_DD_{key}");
            for (adp, overrides, expected) in [
                (false, vec![], file_value),
                (true, vec![], file_value),
                (true, vec![(&dd_key[..], dd_value)], dd_value),
                (true, vec![(&adp_key[..], adp_value)], adp_value),
                (
                    true,
                    vec![(&dd_key[..], dd_value), (&adp_key[..], adp_value)],
                    adp_value,
                ),
                (
                    false,
                    vec![(&dd_key[..], dd_value), (&adp_key[..], "invalid")],
                    dd_value,
                ),
            ] {
                let mut values = case(&yaml, &overrides);
                values.env.insert("DD_DATA_PLANE_ENABLED".into(), adp.to_string());
                let config = effective(&AgentAdp, values);
                let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
                if kind == EndpointKind::Udp {
                    let port = expected.parse().unwrap();
                    let ready = adp.then(|| {
                        Evidence::output_matches(
                            "adp",
                            format!(r#"listen_addr:"udp://[^"]+:{port}" \| DogStatsD listener started\."#),
                        )
                    });
                    assert_network_endpoint(&endpoints[endpoint], kind, port, ready);
                } else {
                    assert_socket_endpoint(
                        &endpoints[endpoint],
                        kind,
                        expected,
                        Some(Evidence::socket_exists(endpoint)),
                    );
                }
            }
        }
    }

    #[test]
    fn file_command_ipc_and_adp_api_ports_match_their_endpoints() {
        let config = effective(&AgentAdp, case(
            "cmd_port: 5011\nagent_ipc: {port: 5012}\ndata_plane:\n  enabled: true\n  api_listen_address: tcp://0.0.0.0:5013\n  secure_api_listen_address: tcp://0.0.0.0:5014\n",
            &[],
        ));
        let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
        for (name, kind, port) in [
            ("agent_cmd", EndpointKind::Tcp, 5011),
            ("agent_ipc", EndpointKind::Tcp, 5012),
            ("api", EndpointKind::Http, 5013),
            ("secure_api", EndpointKind::Tcp, 5014),
        ] {
            assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
        }
    }

    #[test]
    fn only_file_keys_read_for_endpoints_require_early_values() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for (key, override_value) in [
                ("dogstatsd_socket", "/airlock/p/shared/dsd.sock"),
                ("dogstatsd_stream_socket", "/airlock/p/shared/stream.sock"),
                ("dogstatsd_port", "8125"),
                ("cmd_port", "5001"),
                ("agent_ipc.port", "5004"),
                ("dogstatsd_non_local_traffic", "true"),
                ("bind_host", "0.0.0.0"),
            ] {
                let yaml_key = if key == "agent_ipc.port" {
                    "agent_ipc:\n  port"
                } else {
                    key
                };
                let yaml = format!("{yaml_key}: '{{{{ peer.late.container_id }}}}'");
                let error = participant.validate_config(LINUX, &case(&yaml, &[])).expect_err(&yaml);
                assert!(
                    error.contains(NAME) && error.contains(key) && error.contains("contains reference"),
                    "{error}"
                );
                let env_key = format!("DD_{}", key.replace('.', "_").to_uppercase());
                effective(participant, case(&yaml, &[(&env_key, override_value)]));
            }
            let config = effective(participant, case("hostname: '{{ peer.late.container_id }}'\nexpvar_port: '{{ peer.late.container_id }}'\ndogstatsd_socket: /airlock/p/shared/metrics.sock", &[]));
            assert_socket_endpoint(
                &participant.endpoints(LINUX, &config).unwrap()["dogstatsd_uds"],
                EndpointKind::UnixDatagram,
                "/airlock/p/shared/metrics.sock",
                Some(Evidence::socket_exists("dogstatsd_uds")),
            );
        }
    }

    #[test]
    fn disabled_dogstatsd_does_not_read_its_file_paths_and_ports() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            let config = effective(participant, case("use_dogstatsd: false\ndogstatsd_socket: '{{ peer.late.container_id }}'\ndogstatsd_port: not-a-port\ndogstatsd_stream_socket: relative.sock", &[]));
            assert!(!participant
                .endpoints(LINUX, &config)
                .unwrap()
                .keys()
                .any(|key| key.starts_with("dogstatsd_")));
        }
    }

    #[test]
    fn invalid_listener_file_values_name_the_file_and_key() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for (key, value) in [
                ("dogstatsd_port", "-1"),
                ("dogstatsd_port", "65536"),
                ("dogstatsd_port", "nope"),
                ("dogstatsd_port", "1.5"),
                ("dogstatsd_port", "null"),
                ("dogstatsd_socket", "relative.sock"),
                ("dogstatsd_socket", "[one, two]"),
                ("dogstatsd_socket", "true"),
                ("dogstatsd_stream_socket", "relative.sock"),
                ("dogstatsd_stream_socket", "{path: /run/sock}"),
                ("dogstatsd_stream_socket", "'{{ unclosed'"),
                ("use_dogstatsd", "maybe"),
                ("dogstatsd_non_local_traffic", "maybe"),
            ] {
                let yaml = format!("{key}: {value}");
                let error = participant.validate_config(LINUX, &case(&yaml, &[])).expect_err(&yaml);
                assert!(error.contains(NAME) && error.contains(key), "{yaml}: {error}");
                let config = EffectiveConfig {
                    files: case(&yaml, &[]).file_contents,
                    ..Default::default()
                };
                assert!(participant.endpoints(LINUX, &config).unwrap_err().contains(key));
            }
        }
    }

    #[test]
    fn malformed_files_fail_before_defaults_and_empty_files_keep_defaults() {
        for bytes in [b"\xff".as_slice(), b"[", b"[1, 2]", b"text"] {
            let mut values = case("", &[]);
            values.file_contents.insert(NAME.into(), bytes.into());
            assert!(AgentAdp.validate_config(LINUX, &values).unwrap_err().contains(NAME));
        }
        for yaml in ["", "# no settings", "{}"] {
            let values = case(yaml, &[]);
            AgentAdp.validate_config(LINUX, &values).unwrap();
            assert_eq!(
                AgentAdp.default_env(LINUX, &values),
                AgentAdp.default_env(LINUX, &CaseValues::default())
            );
        }
    }
}

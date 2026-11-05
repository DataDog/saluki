//! Native macOS processes and configuration for the Agent and ADP.
//!
//! Both processes start together. ADP's IPC client re-reads the Agent's token during its connect
//! retry (about 20 seconds), allowing the Agent to create the token during startup.

use std::collections::BTreeMap;

use panoramic_core::{
    layout::Realization,
    participant::{CaseValues, ExitChannel, OutputChannel, ProcessSpec, RenderedConfig},
    recipe::{HostProcess, HostRecipe, Recipe},
};

use super::{listeners, AgentAdp};

pub(super) fn default_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("DD_GUI_PORT".into(), "-1".into()),
        ("DD_APM_RECEIVER_PORT".into(), "58126".into()),
        ("DD_PROCESS_CONFIG_CMD_PORT".into(), "56062".into()),
        (
            "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR".into(),
            "tcp://0.0.0.0:55102".into(),
        ),
    ])
}

pub(super) fn validate_config(realization: Realization, config: &CaseValues) -> Result<(), String> {
    for key in config.env.keys() {
        if key.starts_with("ADP_DD_") {
            return Err(format!(
                "{key} is unsupported on macos host-process-group; ADP_DD_ variables require Linux service scripts"
            ));
        }
        if matches!(
            key.as_str(),
            "DD_AUTH_TOKEN_FILE_PATH" | "DD_RUN_PATH" | "DD_DATA_PLANE_FORCE_ENABLE"
        ) {
            return Err(format!("{key} is owned by agent-adp on macos host-process-group"));
        }
    }
    listeners::endpoints(realization, &config.env)
        .map(|_| ())
        .map_err(|error| format!("{error} on macos host-process-group"))
}

pub(super) fn recipe(config: &RenderedConfig) -> Result<Recipe, String> {
    if !config.files().is_empty() {
        return Err("agent-adp does not accept case configuration files".into());
    }
    let directory = config.directory();
    let mut env = config.env().clone();
    env.insert("DD_AUTH_TOKEN_FILE_PATH".into(), format!("{directory}/auth_token"));
    let mut agent_env = BTreeMap::from([("DD_RUN_PATH".into(), directory.into())]);
    if config
        .env()
        .get("DD_DATA_PLANE_ENABLED")
        .is_some_and(|value| value == "true")
    {
        agent_env.insert("DD_DATA_PLANE_FORCE_ENABLE".into(), "true".into());
    }
    if listeners::otlp_proxy_enabled(&env)? && !env.contains_key("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT") {
        // Proxy mode requires a configured gRPC protocol; ADP owns its public listener.
        let address = env
            .get("DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY")
            .ok_or("macos host-process-group OTLP proxy requires the planned ADP gRPC listener address")?;
        agent_env.insert(
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
            address.clone(),
        );
    }
    Ok(Recipe::HostProcessGroup(
        HostRecipe::new(vec![
            HostProcess::new(
                "agent",
                AgentAdp::AGENT_HOST_INSTALL,
                vec!["run".into(), "-c".into(), directory.into()],
            )
            .with_env(agent_env),
            HostProcess::new(
                "adp",
                AgentAdp::ADP_HOST_BINARY,
                vec!["-c".into(), format!("{directory}/datadog.yaml"), "run".into()],
            ),
        ])
        .with_env(env)
        .with_file("datadog.yaml", 0o644, Vec::new()),
    ))
}

pub(super) const PROCESS_SPECS: [ProcessSpec; 2] = [
    ProcessSpec {
        name: "adp",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
    ProcessSpec {
        name: "agent",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
];

#[cfg(test)]
mod tests {
    use panoramic_core::{
        layout::{Os, RealizationKind},
        participant::{EffectiveConfig, ParticipantType},
        recipe::test_support::{files, host_processes, rendered_in},
    };

    use super::*;

    const MACOS: Realization = Realization {
        os: Os::MacOs,
        kind: RealizationKind::HostProcessGroup,
    };
    const DIRECTORY: &str = "/tmp/panoramic-testrun0/0000/subject";

    #[test]
    fn macos_rejects_adp_dd_variables_and_definition_owned_keys() {
        for key in [
            "ADP_DD_DOGSTATSD_PORT",
            "ADP_DD_ANYTHING",
            "DD_AUTH_TOKEN_FILE_PATH",
            "DD_RUN_PATH",
            "DD_DATA_PLANE_FORCE_ENABLE",
            "DD_CMD_PORT",
        ] {
            let error = AgentAdp
                .validate_config(
                    MACOS,
                    &CaseValues {
                        env: BTreeMap::from([(key.into(), "invalid".into())]),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert!(
                error.contains(key) && error.contains("macos host-process-group"),
                "{error}"
            );
        }
    }

    #[test]
    fn macos_default_env_fixes_only_settings_without_a_listener() {
        assert_eq!(
            AgentAdp.default_env(MACOS, &CaseValues::default()),
            BTreeMap::from([
                ("DD_GUI_PORT".into(), "-1".into()),
                ("DD_APM_RECEIVER_PORT".into(), "58126".into()),
                ("DD_PROCESS_CONFIG_CMD_PORT".into(), "56062".into()),
                (
                    "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR".into(),
                    "tcp://0.0.0.0:55102".into()
                ),
            ])
        );
    }

    #[test]
    fn the_macos_recipe_runs_the_agent_then_adp_from_the_host_roles_in_the_participant_directory() {
        let env = BTreeMap::from([("DD_HOSTNAME".into(), "test".into())]);
        let recipe = AgentAdp
            .recipe(MACOS, &rendered_in(DIRECTORY, env.clone(), BTreeMap::new()))
            .unwrap();
        let processes = host_processes(&recipe);
        assert_eq!(processes.len(), 2);
        assert_eq!(
            (processes[0].0, processes[0].1),
            ("agent", AgentAdp::AGENT_HOST_INSTALL)
        );
        assert_eq!(processes[0].2, ["run", "-c", DIRECTORY]);
        assert_eq!((processes[1].0, processes[1].1), ("adp", AgentAdp::ADP_HOST_BINARY));
        assert_eq!(processes[1].2, ["-c", &format!("{DIRECTORY}/datadog.yaml"), "run"]);
        for (_, _, _, process_env) in processes {
            assert_eq!(
                process_env["DD_AUTH_TOKEN_FILE_PATH"],
                format!("{DIRECTORY}/auth_token")
            );
            assert_eq!(process_env["DD_HOSTNAME"], "test");
        }
        assert_eq!(files(&recipe), vec![("datadog.yaml", 0o644, b"".as_slice())]);
    }

    #[test]
    fn only_the_agent_process_receives_the_run_path_and_force_enable_when_adp_is_enabled() {
        for enabled in [None, Some("false"), Some("true")] {
            let env = enabled
                .map(|value| ("DD_DATA_PLANE_ENABLED".into(), value.into()))
                .into_iter()
                .collect();
            let recipe = AgentAdp
                .recipe(MACOS, &rendered_in(DIRECTORY, env, BTreeMap::new()))
                .unwrap();
            let processes = host_processes(&recipe);
            assert_eq!(processes[0].3["DD_RUN_PATH"], DIRECTORY);
            assert_eq!(
                processes[0].3.get("DD_DATA_PLANE_FORCE_ENABLE").map(String::as_str),
                (enabled == Some("true")).then_some("true")
            );
            assert!(!processes[1].3.contains_key("DD_RUN_PATH"));
            assert!(!processes[1].3.contains_key("DD_DATA_PLANE_FORCE_ENABLE"));
        }
    }

    #[test]
    fn the_macos_proxy_configures_the_agents_grpc_protocol_at_adps_grpc_address_unless_the_case_sets_it() {
        for proxy in ["true", "false"] {
            for pinned in [None, Some("0.0.0.0:29001")] {
                let mut env = BTreeMap::from([
                    ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                    ("DD_DATA_PLANE_OTLP_ENABLED".into(), "true".into()),
                    ("DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(), proxy.into()),
                    (
                        "DD_DATA_PLANE_OTLP_PROXY_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
                        "127.0.0.1:20001".into(),
                    ),
                    (
                        "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY".into(),
                        "127.0.0.1:20002".into(),
                    ),
                ]);
                if let Some(pinned) = pinned {
                    env.insert("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(), pinned.into());
                }
                let recipe = AgentAdp
                    .recipe(MACOS, &rendered_in(DIRECTORY, env, BTreeMap::new()))
                    .unwrap();
                let processes = host_processes(&recipe);
                assert_eq!(
                    processes[0]
                        .3
                        .get("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT")
                        .map(String::as_str),
                    pinned.or((proxy == "true").then_some("127.0.0.1:20002"))
                );
                assert_eq!(
                    processes[1]
                        .3
                        .get("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT")
                        .map(String::as_str),
                    pinned
                );
            }
        }
    }

    #[test]
    fn macos_validates_the_agents_grpc_address_in_both_proxy_modes() {
        let key = "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT";
        for proxy in ["true", "false"] {
            let mut config = CaseValues {
                env: BTreeMap::from([
                    ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                    ("DD_DATA_PLANE_OTLP_ENABLED".into(), "true".into()),
                    ("DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(), proxy.into()),
                ]),
                ..Default::default()
            };
            for value in [
                "",
                "localhost",
                "tcp://localhost:29001",
                "localhost:29001/path",
                "user@localhost:29001",
                "localhost:29001?q=x",
                "localhost:29001#x",
                "localhost:65536",
                " localhost:29001",
            ] {
                config.env.insert(key.into(), value.into());
                let error = AgentAdp.validate_config(MACOS, &config).unwrap_err();
                assert!(
                    error.contains(key) && error.contains("macos host-process-group"),
                    "{error}"
                );
            }
            for value in ["localhost:29001", "127.0.0.1:29001", "[::1]:29001"] {
                config.env.insert(key.into(), value.into());
                AgentAdp.validate_config(MACOS, &config).unwrap();
            }
        }
    }

    #[test]
    fn macos_endpoints_and_recipe_reject_invalid_proxy_switches() {
        let keys = [
            "DD_DATA_PLANE_ENABLED",
            "DD_DATA_PLANE_OTLP_ENABLED",
            "DD_DATA_PLANE_OTLP_PROXY_ENABLED",
        ];
        for key in keys {
            let mut env = keys
                .into_iter()
                .map(|key| (key.into(), "true".into()))
                .collect::<BTreeMap<_, _>>();
            env.insert(key.into(), "True".into());
            let error = AgentAdp
                .endpoints(
                    MACOS,
                    &EffectiveConfig {
                        env: env.clone(),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert!(error.contains(key), "{error}");
            let error = AgentAdp
                .recipe(MACOS, &rendered_in(DIRECTORY, env, BTreeMap::new()))
                .err()
                .unwrap();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn the_macos_recipe_rejects_case_files() {
        let config = rendered_in(
            DIRECTORY,
            BTreeMap::new(),
            BTreeMap::from([("datadog.yaml".into(), vec![])]),
        );
        assert_eq!(
            AgentAdp.recipe(MACOS, &config).err().unwrap(),
            "agent-adp does not accept case configuration files"
        );
    }

    #[test]
    fn the_macos_processes_are_waited_on_with_separate_pipes() {
        assert_eq!(
            AgentAdp.process_specs(MACOS),
            &[
                ProcessSpec {
                    name: "adp",
                    exit: ExitChannel::Wait,
                    output: OutputChannel::Pipes
                },
                ProcessSpec {
                    name: "agent",
                    exit: ExitChannel::Wait,
                    output: OutputChannel::Pipes
                },
            ]
        );
        assert!(AgentAdp.readiness(MACOS, &EffectiveConfig::default()).is_empty());
    }
}

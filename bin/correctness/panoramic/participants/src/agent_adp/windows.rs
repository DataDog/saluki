//! Windows container entrypoint and environment for the Core Agent and ADP.

use panoramic_core::{
    participant::{CaseValues, ExitChannel, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    recipe::{ContainerRecipe, Recipe},
};

use super::{late, AgentAdp};
use crate::WINDOWS_CASE_FILES_DIR;

const ENTRYPOINT_PATH: &str = r"C:\adp\participant-entrypoint.ps1";
const ENTRYPOINT: &[u8] = include_bytes!("assets/windows/entrypoint.ps1");

const WINDOWS_ENV_ALIASES: &[(&str, &str)] = &[
    ("DD_DATA_PLANE_ENABLED", "DD_DATA_PLANE__ENABLED"),
    ("DD_DATA_PLANE_STANDALONE_MODE", "DD_DATA_PLANE__STANDALONE_MODE"),
    (
        "DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT",
        "DD_DATA_PLANE__USE_NEW_CONFIG_STREAM_ENDPOINT",
    ),
    (
        "DD_DATA_PLANE_REMOTE_AGENT_ENABLED",
        "DD_DATA_PLANE__REMOTE_AGENT_ENABLED",
    ),
    ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "DD_DATA_PLANE__DOGSTATSD__ENABLED"),
    ("DD_DATA_PLANE_OTLP_ENABLED", "DD_DATA_PLANE__OTLP__ENABLED"),
    (
        "DD_DATA_PLANE_OTLP_PROXY_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_TRACES_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__TRACES__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_METRICS_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__METRICS__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_LOGS_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__LOGS__ENABLED",
    ),
    ("DD_DATA_PLANE_LOG_FILE", "DD_DATA_PLANE__LOG_FILE"),
];

pub(super) fn validate_config(config: &CaseValues) -> Result<(), String> {
    if config.files.contains_key("cont-init.d/") {
        return Err("agent-adp cont-init.d is supported in Linux containers only".into());
    }
    for key in config.env.keys() {
        if key.starts_with("ADP_DD_") {
            return Err(format!(
                "{key} is unsupported on windows-container; ADP_DD_ variables require Linux service scripts"
            ));
        }
    }
    Ok(())
}

pub(super) fn recipe(config: &RenderedConfig) -> Result<Recipe, String> {
    let mut env = config.env().clone();
    for &(source, target) in WINDOWS_ENV_ALIASES {
        if let Some(value) = env.get(source).cloned() {
            env.entry(target.into()).or_insert(value);
        }
    }
    // The Core Agent otherwise disables the data plane when streaming its Windows configuration.
    if env.get("DD_DATA_PLANE_ENABLED").is_some_and(|value| value == "true") {
        env.entry("DD_DATA_PLANE_FORCE_ENABLE".into())
            .or_insert_with(|| "true".into());
    }
    let mut recipe = ContainerRecipe::new(AgentAdp::WINDOWS_IMAGE)
        .with_entrypoint(["pwsh", "-NoProfile", "-NonInteractive", "-File", ENTRYPOINT_PATH])
        .with_command(["-c", r"C:\ProgramData\Datadog\datadog.yaml", "run"])
        .with_env(env)
        .with_file(ENTRYPOINT_PATH, 0o644, ENTRYPOINT.to_vec());
    if let Some(hook) = late::windows_hook(config.env(), config.params()) {
        recipe = recipe.with_file(r"C:\adp\participant-late.ps1", 0o644, hook);
    }
    for (name, contents) in config.files() {
        let Some(relative) = name.strip_prefix("files/") else {
            return Err(format!(
                "agent-adp does not accept case configuration file '{name}' on Windows"
            ));
        };
        recipe = recipe.with_file(
            format!(r"{WINDOWS_CASE_FILES_DIR}\{}", relative.replace('/', r"\")),
            0o644,
            contents.clone(),
        );
    }
    Ok(Recipe::Container(recipe))
}

pub(super) const PROCESS_SPECS: [ProcessSpec; 2] = [
    ProcessSpec {
        name: "adp",
        exit: ExitChannel::Record { file: "adp" },
        output: OutputChannel::Container,
    },
    ProcessSpec {
        name: "agent",
        exit: ExitChannel::Record { file: "agent" },
        output: OutputChannel::File {
            file: "agent.log",
            stream: OutputStream::Merged,
        },
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use panoramic_core::{
        layout::{Os, Realization, RealizationKind},
        participant::ParticipantType,
        recipe::test_support::{assert_container_command, files, rendered},
    };

    use super::*;

    const WINDOWS: Realization = Realization {
        os: Os::Windows,
        kind: RealizationKind::WindowsContainer,
    };

    fn assert_recipe_env(env: BTreeMap<String, String>, expected: &BTreeMap<String, String>) {
        let recipe = AgentAdp.recipe(WINDOWS, &rendered(env, BTreeMap::new())).unwrap();
        assert_container_command(
            &recipe,
            AgentAdp::WINDOWS_IMAGE,
            &[
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                r"C:\adp\participant-entrypoint.ps1",
            ],
            &["-c", r"C:\ProgramData\Datadog\datadog.yaml", "run"],
            expected,
        );
    }

    #[test]
    fn the_windows_recipe_runs_the_participant_entrypoint_from_the_windows_image_with_the_case_command() {
        let env = BTreeMap::from([("DD_HOSTNAME".into(), "windows-test".into())]);
        assert_recipe_env(env.clone(), &env);
        let recipe = AgentAdp.recipe(WINDOWS, &rendered(env, BTreeMap::new())).unwrap();
        assert_eq!(
            files(&recipe),
            vec![(r"C:\adp\participant-entrypoint.ps1", 0o644, ENTRYPOINT)]
        );
    }

    #[test]
    fn the_windows_recipe_adds_nested_aliases_without_overriding_explicit_values() {
        for (source, target) in [
            ("DD_DATA_PLANE_ENABLED", "DD_DATA_PLANE__ENABLED"),
            ("DD_DATA_PLANE_STANDALONE_MODE", "DD_DATA_PLANE__STANDALONE_MODE"),
            (
                "DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT",
                "DD_DATA_PLANE__USE_NEW_CONFIG_STREAM_ENDPOINT",
            ),
            (
                "DD_DATA_PLANE_REMOTE_AGENT_ENABLED",
                "DD_DATA_PLANE__REMOTE_AGENT_ENABLED",
            ),
            ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "DD_DATA_PLANE__DOGSTATSD__ENABLED"),
            ("DD_DATA_PLANE_OTLP_ENABLED", "DD_DATA_PLANE__OTLP__ENABLED"),
            (
                "DD_DATA_PLANE_OTLP_PROXY_ENABLED",
                "DD_DATA_PLANE__OTLP__PROXY__ENABLED",
            ),
            (
                "DD_DATA_PLANE_OTLP_PROXY_TRACES_ENABLED",
                "DD_DATA_PLANE__OTLP__PROXY__TRACES__ENABLED",
            ),
            (
                "DD_DATA_PLANE_OTLP_PROXY_METRICS_ENABLED",
                "DD_DATA_PLANE__OTLP__PROXY__METRICS__ENABLED",
            ),
            (
                "DD_DATA_PLANE_OTLP_PROXY_LOGS_ENABLED",
                "DD_DATA_PLANE__OTLP__PROXY__LOGS__ENABLED",
            ),
            ("DD_DATA_PLANE_LOG_FILE", "DD_DATA_PLANE__LOG_FILE"),
        ] {
            for explicit in [None, Some("explicit")] {
                let mut env = BTreeMap::from([(source.into(), "mapped".into()), ("CASE_VALUE".into(), "kept".into())]);
                if let Some(value) = explicit {
                    env.insert(target.into(), value.into());
                }
                let mut expected = env.clone();
                expected.insert(target.into(), explicit.unwrap_or("mapped").into());
                assert_recipe_env(env, &expected);
            }
            let env = BTreeMap::from([(target.into(), "explicit".into())]);
            assert_recipe_env(env.clone(), &env);
        }
    }

    #[test]
    fn the_windows_recipe_force_enables_adp_in_the_agent_only_when_adp_is_enabled() {
        for enabled in [None, Some("false"), Some("true"), Some("True")] {
            for forced in [None, Some("false"), Some("true")] {
                let mut env = BTreeMap::new();
                if let Some(value) = enabled {
                    env.insert("DD_DATA_PLANE_ENABLED".into(), value.into());
                }
                if let Some(value) = forced {
                    env.insert("DD_DATA_PLANE_FORCE_ENABLE".into(), value.into());
                }
                let mut expected = env.clone();
                if let Some(value) = enabled {
                    expected.insert("DD_DATA_PLANE__ENABLED".into(), value.into());
                }
                if enabled == Some("true") && forced.is_none() {
                    expected.insert("DD_DATA_PLANE_FORCE_ENABLE".into(), "true".into());
                }
                assert_recipe_env(env, &expected);
            }
        }
    }

    #[test]
    fn windows_rejects_adp_dd_variables() {
        for key in ["ADP_DD_DOGSTATSD_PORT", "ADP_DD_ANYTHING", "ADP_DD_"] {
            let error = AgentAdp
                .validate_config(
                    WINDOWS,
                    &CaseValues {
                        env: BTreeMap::from([(key.into(), "value".into())]),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert!(
                error.contains(key) && error.contains("windows-container") && error.contains("Linux service scripts"),
                "{error}"
            );
        }
        AgentAdp
            .validate_config(
                WINDOWS,
                &CaseValues {
                    env: BTreeMap::from([("DD_DATA_PLANE_FORCE_ENABLE".into(), "false".into())]),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn windows_default_env_matches_the_linux_container_port_table() {
        let linux = Realization {
            os: Os::Linux,
            kind: RealizationKind::LinuxContainer,
        };
        let windows = AgentAdp.default_env(WINDOWS, &CaseValues::default());
        assert_eq!(windows, AgentAdp.default_env(linux, &CaseValues::default()));
        assert_eq!(windows.len(), 12);
        assert_eq!(windows["DD_CMD_PORT"], "55001");
        assert_eq!(windows["DD_DOGSTATSD_PORT"], "58125");
        assert_eq!(windows["DD_DATA_PLANE_API_LISTEN_ADDRESS"], "tcp://0.0.0.0:55100");
    }

    #[test]
    fn the_windows_recipe_rejects_unknown_case_files() {
        let config = rendered(BTreeMap::new(), BTreeMap::from([("datadog.yaml".into(), vec![])]));
        assert_eq!(
            AgentAdp.recipe(WINDOWS, &config).err().unwrap(),
            "agent-adp does not accept case configuration file 'datadog.yaml' on Windows"
        );
    }

    #[test]
    fn windows_case_files_land_under_the_files_directory_with_backslash_paths() {
        let config = rendered(
            BTreeMap::new(),
            BTreeMap::from([
                ("files/run.ps1".into(), b"script".to_vec()),
                ("files/nested/data.txt".into(), b"fixture".to_vec()),
            ]),
        );
        let recipe = AgentAdp.recipe(WINDOWS, &config).unwrap();
        assert_eq!(
            files(&recipe),
            vec![
                (ENTRYPOINT_PATH, 0o644, ENTRYPOINT),
                (r"C:\panoramic\files\nested\data.txt", 0o644, b"fixture".as_slice()),
                (r"C:\panoramic\files\run.ps1", 0o644, b"script".as_slice()),
            ]
        );
    }

    #[test]
    fn windows_rejects_cont_init_case_files() {
        let config = CaseValues {
            files: BTreeMap::from([(
                "cont-init.d/".into(),
                panoramic_core::case::FileSource {
                    from: "hooks".into(),
                    template: false,
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            AgentAdp.validate_config(WINDOWS, &config).unwrap_err(),
            "agent-adp cont-init.d is supported in Linux containers only"
        );
    }

    #[test]
    fn windows_declares_recorded_exits_for_both_processes_and_file_output_for_the_agent() {
        assert_eq!(
            AgentAdp.process_specs(WINDOWS),
            &[
                ProcessSpec {
                    name: "adp",
                    exit: ExitChannel::Record { file: "adp" },
                    output: OutputChannel::Container
                },
                ProcessSpec {
                    name: "agent",
                    exit: ExitChannel::Record { file: "agent" },
                    output: OutputChannel::File {
                        file: "agent.log",
                        stream: OutputStream::Merged
                    }
                },
            ]
        );
    }

    #[test]
    fn the_windows_entrypoint_writes_both_exit_records_and_the_agent_log_where_the_backend_reads_them() {
        let recipe = AgentAdp
            .recipe(WINDOWS, &rendered(BTreeMap::new(), BTreeMap::new()))
            .unwrap();
        let placed = files(&recipe);
        let entrypoint = String::from_utf8(placed[0].2.to_vec()).unwrap().replace("\r\n", "\n");
        for required in [
            "PANORAMIC_EXIT_DIR",
            "PANORAMIC_LOG_DIR",
            "agent.log",
            "Start-ThreadJob",
            "$AgentProcess.WaitForExit()",
            "Write-ExitRecord \"agent\" $AgentProcess.ExitCode",
            "Write-ExitRecord \"adp\" $ExitCode",
            "[BitConverter]::ToUInt32",
            "[BitConverter]::GetBytes($Code)",
            "[DateTimeOffset]::UtcNow",
            "ToUnixTimeSeconds()",
            "TicksPerSecond",
            "{2:D7}`n",
            "[System.IO.File]::WriteAllText",
            "exit $ExitCode",
            "DD_DATA_PLANE_LOG_FILE",
            "DD_LOG_FILE",
            r"C:\ProgramData\Datadog\logs",
            r#"datadogagent > "%PANORAMIC_LOG_DIR%\agent.log" 2>&1"#,
            "$Agent.Kill($true)",
            "Wait-Job | Receive-Job",
        ] {
            assert!(entrypoint.contains(required), "missing {required}");
        }
        for forbidden in [
            "/airlock",
            "PANORAMIC_DYNAMIC",
            "RedirectStandardOutput",
            "RedirectStandardError",
        ] {
            assert!(!entrypoint.contains(forbidden), "unexpected {forbidden}");
        }
        assert!(
            entrypoint
                .find(r#"if (Test-Path "C:\adp\participant-late.ps1") { . "C:\adp\participant-late.ps1" }"#)
                .unwrap()
                < entrypoint.find("Initialize-LogDirectories\n").unwrap()
        );
        assert!(
            entrypoint.find("Initialize-LogDirectories\n").unwrap()
                < entrypoint.find("$Agent = Start-Process").unwrap()
        );
        assert!(
            entrypoint.find("$AgentWait = Start-ThreadJob").unwrap()
                < entrypoint.find(r#"& "C:\adp\agent-data-plane.exe" @args"#).unwrap()
        );
        assert!(
            entrypoint.find(r#"& "C:\adp\agent-data-plane.exe" @args"#).unwrap()
                < entrypoint.find("Write-ExitRecord \"adp\" $ExitCode").unwrap()
        );
        assert!(
            entrypoint.find("Write-ExitRecord \"adp\" $ExitCode").unwrap()
                < entrypoint.find("$Agent.Kill($true)").unwrap()
        );
    }
}

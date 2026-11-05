//! Commands offered by the Agent and ADP on each realization.

use async_trait::async_trait;
use panoramic_core::{
    participant::{OpContext, Operation},
    step::{ExecOutput, HandleError},
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Programs {
    /// Absolute image paths, run with `exec`.
    Paths { adp: &'static str, agent: &'static str },
    /// The participant's artifact-bound programs, run with `exec_process` and `-c` relative to its directory.
    HostProcesses,
}

#[derive(Clone, Copy)]
enum Program {
    Adp,
    Agent,
}

impl Programs {
    async fn exec(&self, cx: &OpContext<'_>, program: Program, args: Vec<String>) -> Result<ExecOutput, HandleError> {
        match self {
            Self::Paths { adp, agent } => {
                let path = match program {
                    Program::Adp => adp,
                    Program::Agent => agent,
                };
                cx.exec(std::iter::once((*path).into()).chain(args).collect()).await
            }
            Self::HostProcesses => {
                let (process, config) = match program {
                    Program::Adp => ("adp", "datadog.yaml"),
                    Program::Agent => ("agent", "."),
                };
                cx.exec_process(process, ["-c".into(), config.into()].into_iter().chain(args).collect())
                    .await
            }
        }
    }
}

const LINUX: Programs = Programs::Paths {
    adp: "/opt/datadog-agent/embedded/bin/agent-data-plane",
    agent: "/opt/datadog-agent/bin/agent/agent",
};
const WINDOWS: Programs = Programs::Paths {
    adp: r"C:\adp\agent-data-plane.exe",
    agent: r"C:\Program Files\Datadog\Datadog Agent\bin\agent.exe",
};

struct AgentAdpOperation {
    command: Command,
    programs: Programs,
}

enum Command {
    CliAdp,
    CliAgent,
    ConfigDump(Program),
    ConfigSet,
    DogstatsdCapture,
    DogstatsdReplay,
    DogstatsdStats,
}

macro_rules! operations {
    ($programs:expr) => {
        &[
            &AgentAdpOperation {
                command: Command::CliAdp,
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::CliAgent,
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::ConfigDump(Program::Adp),
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::ConfigSet,
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::DogstatsdCapture,
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::DogstatsdReplay,
                programs: $programs,
            },
            &AgentAdpOperation {
                command: Command::DogstatsdStats,
                programs: $programs,
            },
        ]
    };
}

pub(super) const LINUX_OPERATIONS: &[&dyn Operation] = operations!(LINUX);
pub(super) const MACOS_OPERATIONS: &[&dyn Operation] = operations!(Programs::HostProcesses);
pub(super) const WINDOWS_OPERATIONS: &[&dyn Operation] = operations!(WINDOWS);
pub(crate) const AGENT_OPERATIONS: &[&dyn Operation] = &[
    &AgentAdpOperation {
        command: Command::CliAgent,
        programs: LINUX,
    },
    &AgentAdpOperation {
        command: Command::ConfigDump(Program::Agent),
        programs: LINUX,
    },
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CliArgs {
    args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSetArgs {
    key: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureArgs {
    duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsArgs {
    seconds: u64,
}

fn decode<T: serde::de::DeserializeOwned>(args: Value, expected: &str) -> Result<T, HandleError> {
    serde_json::from_value(args).map_err(|_| HandleError::Harness {
        message: format!("operation requires exactly {expected}"),
    })
}

#[async_trait]
impl Operation for AgentAdpOperation {
    fn name(&self) -> &'static str {
        match self.command {
            Command::CliAdp => "cli_adp",
            Command::CliAgent => "cli_agent",
            Command::ConfigDump(_) => "config_dump",
            Command::ConfigSet => "config_set",
            Command::DogstatsdCapture => "dogstatsd_capture",
            Command::DogstatsdReplay => "dogstatsd_replay",
            Command::DogstatsdStats => "dogstatsd_stats",
        }
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let (program, argv, label) = match self.command {
            Command::CliAdp | Command::CliAgent => {
                let args: CliArgs = decode(args, "{\"args\": [string]}")?;
                let (program, label) = match self.command {
                    Command::CliAdp => (Program::Adp, "ADP CLI"),
                    _ => (Program::Agent, "Agent CLI"),
                };
                (program, args.args, label)
            }
            Command::ConfigDump(program) => {
                let view = args
                    .as_object()
                    .filter(|args| args.len() == 1)
                    .and_then(|args| args.get("view"))
                    .and_then(Value::as_str)
                    .filter(|view| matches!(*view, "source" | "runtime"))
                    .ok_or_else(|| HandleError::Harness {
                        message: "config_dump requires exactly {\"view\": \"source\"|\"runtime\"}".into(),
                    })?;
                if matches!(program, Program::Agent) && view == "source" {
                    return Err(HandleError::Harness {
                        message: "the Agent offers only the runtime configuration view".into(),
                    });
                }
                let mut argv = vec!["config".into()];
                let label = match program {
                    Program::Adp => {
                        argv.push("--json".into());
                        if view == "runtime" {
                            argv.push("--runtime".into());
                        }
                        "ADP configuration CLI"
                    }
                    // The Agent exposes only its runtime configuration view, in YAML.
                    Program::Agent => "Agent configuration CLI",
                };
                (program, argv, label)
            }
            Command::ConfigSet => {
                let args: ConfigSetArgs = decode(args, "{\"key\": string, \"value\": string}")?;
                (
                    Program::Agent,
                    vec!["config".into(), "set".into(), args.key, args.value],
                    "Agent config set",
                )
            }
            Command::DogstatsdCapture => {
                let args: CaptureArgs = decode(args, "{\"duration_ms\": unsigned integer}")?;
                (
                    Program::Adp,
                    vec![
                        "dogstatsd".into(),
                        "capture".into(),
                        "--duration".into(),
                        format!("{}ms", args.duration_ms),
                        "--compressed".into(),
                        "false".into(),
                    ],
                    "ADP DogStatsD capture",
                )
            }
            Command::DogstatsdReplay => {
                let args: ReplayArgs = decode(args, "{\"path\": string}")?;
                (
                    Program::Adp,
                    vec!["dogstatsd".into(), "replay".into(), "--file".into(), args.path],
                    "ADP DogStatsD replay",
                )
            }
            Command::DogstatsdStats => {
                let args: StatsArgs = decode(args, "{\"seconds\": unsigned integer}")?;
                (
                    Program::Adp,
                    vec![
                        "dogstatsd".into(),
                        "stats".into(),
                        "--duration-secs".into(),
                        args.seconds.to_string(),
                    ],
                    "ADP DogStatsD statistics",
                )
            }
        };
        let output = self.programs.exec(cx, program, argv).await?;
        match output.exit_code {
            0 => {}
            126 | 127 => {
                return Err(HandleError::Harness {
                    message: format!("{label} could not run (exit {})", output.exit_code),
                })
            }
            code => {
                let mut message = format!("{label} exited with {code}");
                if matches!(self.command, Command::ConfigSet) {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    let stderr = &stderr[..stderr.floor_char_boundary(512)];
                    if !stderr.is_empty() {
                        message.push_str(": ");
                        message.push_str(stderr);
                    }
                }
                return Err(HandleError::Product { message });
            }
        }
        match self.command {
            Command::CliAdp | Command::CliAgent => Ok(json!({"output": combined_output(&output)})),
            Command::ConfigDump(Program::Adp) => {
                serde_json::from_slice(&output.stdout).map_err(|_| HandleError::Product {
                    message: "ADP configuration CLI returned invalid JSON".into(),
                })
            }
            Command::ConfigDump(Program::Agent) => {
                let invalid = || HandleError::Product {
                    message: "Agent configuration CLI returned invalid YAML configuration".into(),
                };
                let text = simdutf8::basic::from_utf8(&output.stdout).map_err(|_| invalid())?;
                let value: Value = serde_saphyr::from_str(text).map_err(|_| invalid())?;
                if !value.is_object() {
                    return Err(invalid());
                }
                Ok(value)
            }
            Command::ConfigSet | Command::DogstatsdReplay => Ok(json!({})),
            Command::DogstatsdCapture => Ok(json!({"path": capture_path_from_output(&combined_output(&output))?})),
            Command::DogstatsdStats => Ok(json!({"metrics": metric_names(&combined_output(&output))?})),
        }
    }
}

fn combined_output(output: &ExecOutput) -> String {
    [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ]
    .concat()
}

fn capture_path_from_output(output: &str) -> Result<&str, HandleError> {
    let (_, after_prefix) = output
        .rsplit_once("Data will be written to '")
        .ok_or_else(|| HandleError::Product {
            message: "DogStatsD capture did not report its output path".into(),
        })?;
    let (path, _) = after_prefix.split_once('\'').ok_or_else(|| HandleError::Product {
        message: "DogStatsD capture reported an unterminated output path".into(),
    })?;
    if path.is_empty() {
        return Err(HandleError::Product {
            message: "DogStatsD capture reported an empty output path".into(),
        });
    }
    Ok(path)
}

fn metric_names(output: &str) -> Result<Vec<&str>, HandleError> {
    let mut rows = output.lines().filter_map(|line| {
        line.trim_start()
            .strip_prefix('|')
            .and_then(|row| row.split_once('|'))
            .map(|(metric, _)| metric.trim())
    });
    if rows.next() != Some("Metric") {
        return Err(HandleError::Product {
            message: "DogStatsD statistics did not return a metric table".into(),
        });
    }
    Ok(rows.collect())
}

#[cfg(test)]
mod tests {
    use panoramic_core::participant::test_support::{run_operation, run_operation_with_process};

    use super::*;

    const ADP: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";
    const AGENT: &str = "/opt/datadog-agent/bin/agent/agent";

    #[tokio::test]
    async fn agent_config_dump_parses_the_runtime_yaml_and_rejects_the_source_view_without_running_anything() {
        assert_eq!(
            AGENT_OPERATIONS
                .iter()
                .map(|operation| operation.name())
                .collect::<Vec<_>>(),
            ["cli_agent", "config_dump"]
        );
        let result = run_operation(
            operation(AGENT_OPERATIONS, "config_dump"),
            json!({"view": "runtime"}),
            |command| {
                assert_eq!(command.argv, [AGENT, "config"]);
                assert!(command.env.is_empty() && command.stdin.is_none());
                Ok(output(
                    0,
                    b"dogstatsd_port: 0\ndata_plane:\n  enabled: false\ntags: [a, b]\n",
                    b"",
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            json!({"dogstatsd_port": 0, "data_plane": {"enabled": false}, "tags": ["a", "b"]})
        );
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let error = run_operation(
            operation(AGENT_OPERATIONS, "config_dump"),
            json!({"view": "source"}),
            |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(output(0, b"dogstatsd_port: 0\n", b""))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            HandleError::Harness {
                message: "the Agent offers only the runtime configuration view".into()
            }
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let result = run_operation(
            operation(AGENT_OPERATIONS, "cli_agent"),
            json!({"args": ["status"]}),
            |command| {
                assert_eq!(command.argv, [AGENT, "status"]);
                Ok(output(0, b"agent status", b""))
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"output": "agent status"}));
    }

    #[tokio::test]
    async fn agent_config_dump_validates_arguments_output_and_error_classification() {
        let operation = operation(AGENT_OPERATIONS, "config_dump");
        for args in [
            Value::Null,
            json!({}),
            json!({"view": "unknown"}),
            json!({"view": "source", "extra": true}),
        ] {
            let error = run_operation(operation, args, |_| panic!("invalid arguments must not exec"))
                .await
                .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
        }
        for text in [b"\xff".as_slice(), b"", b"secret output", b"[secret]", b"key: ["] {
            let error = run_operation(operation, json!({"view": "runtime"}), |_| {
                Ok(output(0, text, b"stderr-secret"))
            })
            .await
            .unwrap_err();
            assert_eq!(
                error,
                HandleError::Product {
                    message: "Agent configuration CLI returned invalid YAML configuration".into()
                }
            );
        }
        for code in [1, 3, 126, 127] {
            let error = run_operation(operation, json!({"view": "runtime"}), |_| {
                Ok(output(code, b"secret output", b"stderr-secret"))
            })
            .await
            .unwrap_err();
            assert_eq!(matches!(error, HandleError::Harness { .. }), code == 126 || code == 127);
            assert!(error.to_string().contains("Agent configuration CLI"));
            assert!(!error.to_string().contains("secret"));
        }
        let expected = HandleError::Harness {
            message: "execution unavailable".into(),
        };
        assert_eq!(
            run_operation(operation, json!({"view": "runtime"}), |_| Err(expected.clone()))
                .await
                .unwrap_err(),
            expected
        );
    }

    #[tokio::test]
    async fn operations_use_absolute_container_paths_or_declared_host_programs_with_relative_config() {
        for (name, process, args, argv, stdout, expected) in [
            (
                "cli_adp",
                "adp",
                json!({"args": ["config", "a b"]}),
                vec!["config", "a b"],
                "ready",
                json!({"output": "ready"}),
            ),
            (
                "cli_agent",
                "agent",
                json!({"args": ["status"]}),
                vec!["status"],
                "ready",
                json!({"output": "ready"}),
            ),
            (
                "config_dump",
                "adp",
                json!({"view": "source"}),
                vec!["config", "--json"],
                r#"{"key":true}"#,
                json!({"key": true}),
            ),
            (
                "config_dump",
                "adp",
                json!({"view": "runtime"}),
                vec!["config", "--json", "--runtime"],
                r#"{"key":true}"#,
                json!({"key": true}),
            ),
            (
                "config_set",
                "agent",
                json!({"key": "log_level", "value": "debug"}),
                vec!["config", "set", "log_level", "debug"],
                "",
                json!({}),
            ),
            (
                "dogstatsd_capture",
                "adp",
                json!({"duration_ms": 1250}),
                vec!["dogstatsd", "capture", "--duration", "1250ms", "--compressed", "false"],
                "Data will be written to 'capture.dog'",
                json!({"path": "capture.dog"}),
            ),
            (
                "dogstatsd_replay",
                "adp",
                json!({"path": "capture with spaces.dog"}),
                vec!["dogstatsd", "replay", "--file", "capture with spaces.dog"],
                "",
                json!({}),
            ),
            (
                "dogstatsd_stats",
                "adp",
                json!({"seconds": 3}),
                vec!["dogstatsd", "stats", "--duration-secs", "3"],
                "| Metric | Count |\n| requests | 1 |",
                json!({"metrics": ["requests"]}),
            ),
        ] {
            for (operations, paths) in [
                (LINUX_OPERATIONS, Some((ADP, AGENT))),
                (MACOS_OPERATIONS, None),
                (
                    WINDOWS_OPERATIONS,
                    Some((
                        r"C:\adp\agent-data-plane.exe",
                        r"C:\Program Files\Datadog\Datadog Agent\bin\agent.exe",
                    )),
                ),
            ] {
                let operation = operation(operations, name);
                let calls = std::sync::atomic::AtomicUsize::new(0);
                let result = run_operation_with_process(
                    operation,
                    args.clone(),
                    |command| {
                        let Some((adp, agent)) = paths else {
                            panic!("host commands must use the declared process")
                        };
                        let binary = if process == "adp" { adp } else { agent };
                        assert_eq!(
                            command.argv,
                            std::iter::once(binary).chain(argv.iter().copied()).collect::<Vec<_>>()
                        );
                        assert!(command.env.is_empty() && command.stdin.is_none());
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(output(0, stdout.as_bytes(), b""))
                    },
                    |actual_process, actual_args| {
                        assert!(paths.is_none());
                        assert_eq!(actual_process, process);
                        let config = if process == "adp" { "datadog.yaml" } else { "." };
                        assert_eq!(
                            actual_args,
                            ["-c", config]
                                .into_iter()
                                .chain(argv.iter().copied())
                                .collect::<Vec<_>>()
                        );
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(output(0, stdout.as_bytes(), b""))
                    },
                )
                .await
                .unwrap();
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert_eq!(result, expected, "{name}");
            }
        }
    }

    fn operation(operations: &[&'static dyn Operation], name: &str) -> &'static dyn Operation {
        *operations
            .iter()
            .find(|operation| operation.name() == name)
            .expect("a declared operation")
    }

    fn cli_output(exit_code: i64, stdout: &[u8]) -> ExecOutput {
        ExecOutput {
            stdout: stdout.into(),
            stderr: b"stderr-secret".to_vec(),
            exit_code,
        }
    }

    #[tokio::test]
    async fn config_dump_runs_the_cli_for_the_requested_view_and_returns_parsed_json() {
        let expected = serde_json::json!({"domains": {"remote_configuration": {"enabled": true}}});
        for view in ["source", "runtime"] {
            let value = run_operation(
                operation(LINUX_OPERATIONS, "config_dump"),
                serde_json::json!({"view": view}),
                |command| {
                    let mut argv = vec!["/opt/datadog-agent/embedded/bin/agent-data-plane", "config", "--json"];
                    if view == "runtime" {
                        argv.push("--runtime");
                    }
                    assert_eq!(command.argv, argv);
                    assert!(command.env.is_empty());
                    assert!(command.stdin.is_none());
                    Ok(cli_output(0, &serde_json::to_vec(&expected).unwrap()))
                },
            )
            .await
            .unwrap();
            assert_eq!(value, expected, "{view}");
        }
    }

    #[tokio::test]
    async fn config_dump_rejects_every_argument_shape_except_one_known_view_before_exec() {
        for args in [
            Value::Null,
            serde_json::json!("source"),
            serde_json::json!(["source"]),
            serde_json::json!({}),
            serde_json::json!({"view": "unknown-view-secret"}),
            serde_json::json!({"view": null}),
            serde_json::json!({"view": true}),
            serde_json::json!({"view": 1}),
            serde_json::json!({"view": ["source"]}),
            serde_json::json!({"view": {"source": true}}),
            serde_json::json!({"view": "source", "extra": "argument-secret"}),
            serde_json::json!({"view": "runtime", "extra": "argument-secret"}),
            serde_json::json!({"extra": "argument-secret"}),
        ] {
            let error = run_operation(operation(LINUX_OPERATIONS, "config_dump"), args.clone(), |_| {
                panic!("invalid arguments must not exec")
            })
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }), "{args}: {error}");
            assert!(error.to_string().contains("view"));
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_classifies_cli_exits_without_disclosing_output() {
        for code in [1, 3, 10, 125, 126, 127, 128, 255] {
            let error = run_operation(
                operation(LINUX_OPERATIONS, "config_dump"),
                serde_json::json!({"view": "runtime"}),
                |_| Ok(cli_output(code, b"stdout-secret")),
            )
            .await
            .unwrap_err();
            let expected = match code {
                126 | 127 => HandleError::Harness {
                    message: format!("ADP configuration CLI could not run (exit {code})"),
                },
                _ => HandleError::Product {
                    message: format!("ADP configuration CLI exited with {code}"),
                },
            };
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_reports_invalid_json_as_a_product_error_without_disclosing_output() {
        for stdout in [
            b"stdout-secret".as_slice(),
            b"{\"stdout-secret\":}".as_slice(),
            b"{\"api_key\":\"stdout-secret\"} trailing".as_slice(),
            b"\xffstdout-secret".as_slice(),
            b"".as_slice(),
        ] {
            let error = run_operation(
                operation(LINUX_OPERATIONS, "config_dump"),
                serde_json::json!({"view": "source"}),
                |_| Ok(cli_output(0, stdout)),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error,
                HandleError::Product {
                    message: "ADP configuration CLI returned invalid JSON".into(),
                }
            );
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_preserves_exec_failure_classification() {
        for error in [
            HandleError::Harness {
                message: "exec unavailable".into(),
            },
            HandleError::Product {
                message: "exec timed out".into(),
            },
        ] {
            let actual = run_operation(
                operation(LINUX_OPERATIONS, "config_dump"),
                serde_json::json!({"view": "source"}),
                |_| Err(error.clone()),
            )
            .await
            .unwrap_err();
            assert_eq!(actual, error);
        }
    }

    fn output(code: i64, stdout: &[u8], stderr: &[u8]) -> ExecOutput {
        ExecOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code: code,
        }
    }

    #[tokio::test]
    async fn adp_and_agent_cli_operations_run_their_binary_and_return_combined_lossy_output() {
        for (operation, binary) in [
            (operation(LINUX_OPERATIONS, "cli_adp"), ADP),
            (operation(LINUX_OPERATIONS, "cli_agent"), AGENT),
        ] {
            for args in [json!([]), json!(["config", "a b", "'quoted'"])] {
                let value = run_operation(operation, json!({"args": args}), |command| {
                    let expected = std::iter::once(binary)
                        .chain(args.as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()))
                        .collect::<Vec<_>>();
                    assert_eq!(command.argv, expected);
                    assert!(command.env.is_empty() && command.stdin.is_none());
                    Ok(output(0, b" stdout\xff", b"stderr\xfe\n"))
                })
                .await
                .unwrap();
                assert_eq!(value, json!({"output": " stdout\u{fffd}stderr\u{fffd}\n"}));
            }
        }
    }

    #[tokio::test]
    async fn cli_operations_classify_nonzero_exits_without_disclosing_output() {
        for (operation, label) in [
            (operation(LINUX_OPERATIONS, "cli_adp"), "ADP CLI"),
            (operation(LINUX_OPERATIONS, "cli_agent"), "Agent CLI"),
        ] {
            for code in [1, 10, 125, 126, 127, 128, 255] {
                let error = run_operation(operation, json!({"args": []}), |_| {
                    Ok(output(code, b"stdout-secret", b"stderr-secret"))
                })
                .await
                .unwrap_err();
                let expected = if matches!(code, 126 | 127) {
                    HandleError::Harness {
                        message: format!("{label} could not run (exit {code})"),
                    }
                } else {
                    HandleError::Product {
                        message: format!("{label} exited with {code}"),
                    }
                };
                assert_eq!(error, expected);
                assert!(!error.to_string().contains("secret"));
            }
        }
    }

    #[tokio::test]
    async fn operations_reject_missing_extra_and_wrongly_typed_arguments_before_exec() {
        for operations in [LINUX_OPERATIONS, MACOS_OPERATIONS, WINDOWS_OPERATIONS] {
            for (operation, valid) in [
                (operation(operations, "cli_adp"), json!({"args": ["config"]})),
                (operation(operations, "cli_agent"), json!({"args": []})),
                (operation(operations, "config_dump"), json!({"view": "source"})),
                (
                    operation(operations, "config_set"),
                    json!({"key": "log_level", "value": "debug"}),
                ),
                (operation(operations, "dogstatsd_capture"), json!({"duration_ms": 1250})),
                (
                    operation(operations, "dogstatsd_replay"),
                    json!({"path": "/capture.dog"}),
                ),
                (operation(operations, "dogstatsd_stats"), json!({"seconds": 3})),
            ] {
                let mut invalid = vec![Value::Null, json!("argument-secret"), json!([]), json!({})];
                let mut extra = valid.clone();
                extra["extra"] = "argument-secret".into();
                invalid.push(extra);
                for key in valid.as_object().unwrap().keys() {
                    let mut missing = valid.clone();
                    missing.as_object_mut().unwrap().remove(key);
                    invalid.push(missing);
                    for value in [
                        Value::Null,
                        json!(false),
                        json!({"secret": 1}),
                        json!([1]),
                        json!(-1),
                        json!(1.5),
                    ] {
                        let mut wrong = valid.clone();
                        wrong[key] = value;
                        invalid.push(wrong);
                    }
                    if key == "args" {
                        for value in [json!("config"), json!(["config", true])] {
                            let mut wrong = valid.clone();
                            wrong[key] = value;
                            invalid.push(wrong);
                        }
                    }
                    if matches!(key.as_str(), "duration_ms" | "seconds") {
                        let mut wrong = valid.clone();
                        wrong[key] = "3".into();
                        invalid.push(wrong);
                    }
                }
                for args in invalid {
                    let error = run_operation(operation, args.clone(), |_| panic!("invalid arguments must not exec"))
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(error, HandleError::Harness { .. }),
                        "{} {args}: {error}",
                        operation.name()
                    );
                    assert!(!error.to_string().contains("secret"));
                }
            }
        }
    }

    #[tokio::test]
    async fn config_set_runs_the_agent_command_and_bounds_failure_stderr_on_a_character_boundary() {
        let result = run_operation(
            operation(LINUX_OPERATIONS, "config_set"),
            json!({"key": "log_level", "value": "debug"}),
            |command| {
                assert_eq!(command.argv, [AGENT, "config", "set", "log_level", "debug"]);
                Ok(output(0, b"set", b""))
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({}));
        let stderr = format!("  {}suffix  ", "é".repeat(300));
        let error = run_operation(
            operation(LINUX_OPERATIONS, "config_set"),
            json!({"key": "some.key", "value": "a b"}),
            |_| Ok(output(1, b"stdout-secret", stderr.as_bytes())),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            HandleError::Product {
                message: format!("Agent config set exited with 1: {}", "é".repeat(256))
            }
        );
        for code in [126, 127] {
            let error = run_operation(
                operation(LINUX_OPERATIONS, "config_set"),
                json!({"key": "x", "value": "y"}),
                |_| Ok(output(code, b"stdout-secret", b"stderr-secret")),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn capture_duration_uses_go_milliseconds_and_capture_returns_the_reported_path() {
        // ADP's capture command reports its path as `Data will be written to '<path>'`.
        let result = run_operation(
            operation(LINUX_OPERATIONS, "dogstatsd_capture"),
            json!({"duration_ms": 1250}),
            |command| {
                assert_eq!(
                    command.argv,
                    [
                        ADP,
                        "dogstatsd",
                        "capture",
                        "--duration",
                        "1250ms",
                        "--compressed",
                        "false"
                    ]
                );
                Ok(output(
                    0,
                    b"log\n",
                    b"Capture started. Data will be written to '/capture with spaces.dog'.\n",
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"path": "/capture with spaces.dog"}));
    }

    #[tokio::test]
    async fn capture_without_a_complete_nonempty_path_is_a_product_error() {
        for (text, reason) in [
            ("no path", "did not report"),
            ("Data will be written to '/capture", "unterminated"),
            ("Data will be written to ''", "empty"),
        ] {
            let error = run_operation(
                operation(LINUX_OPERATIONS, "dogstatsd_capture"),
                json!({"duration_ms": 1000}),
                |_| Ok(output(0, text.as_bytes(), b"")),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Product { .. }));
            assert!(error.to_string().contains(reason), "{error}");
        }
        assert_eq!(
            capture_path_from_output("Data will be written to '/old'\nData will be written to '/new'").unwrap(),
            "/new"
        );
    }

    #[tokio::test]
    async fn replay_passes_the_capture_path_as_one_argument() {
        let result = run_operation(
            operation(LINUX_OPERATIONS, "dogstatsd_replay"),
            json!({"path": "/capture with spaces.dog"}),
            |command| {
                assert_eq!(
                    command.argv,
                    [ADP, "dogstatsd", "replay", "--file", "/capture with spaces.dog"]
                );
                Ok(output(0, b"done", b""))
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn statistics_list_exact_metric_names_after_the_table_header() {
        // ADP's stats summary prints a table whose header's first cell is `Metric`.
        let table = concat!(
            "logs\n+------------------+------+-------+-----------+\n",
            "| Metric           | Tags | Count | Last Seen |\n",
            "+------------------+------+-------+-----------+\n",
            "| replay.requests  |      | 1     |           |\n",
            "| other.metric     |      | 2     |           |\n",
            "+------------------+------+-------+-----------+\n",
        );
        let result = run_operation(
            operation(LINUX_OPERATIONS, "dogstatsd_stats"),
            json!({"seconds": 3}),
            |command| {
                assert_eq!(command.argv, [ADP, "dogstatsd", "stats", "--duration-secs", "3"]);
                Ok(output(0, table.as_bytes(), b""))
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"metrics": ["replay.requests", "other.metric"]}));
        let names = metric_names(table).unwrap();
        assert!(names.contains(&"replay.requests"));
        assert!(!names.contains(&"replay.request"));
        assert_eq!(
            metric_names("| Metric | Tags | Count | Last Seen |\n").unwrap(),
            Vec::<&str>::new()
        );
        for text in ["", "not a table", "| wrong header | count |"] {
            let error = run_operation(
                operation(LINUX_OPERATIONS, "dogstatsd_stats"),
                json!({"seconds": 1}),
                |_| Ok(output(0, text.as_bytes(), b"")),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Product { .. }));
        }
    }

    #[tokio::test]
    async fn capture_replay_and_statistics_classify_exits_and_preserve_mechanism_errors() {
        for (operation, args) in [
            (
                operation(LINUX_OPERATIONS, "dogstatsd_capture"),
                json!({"duration_ms": 1000}),
            ),
            (
                operation(LINUX_OPERATIONS, "dogstatsd_replay"),
                json!({"path": "/capture"}),
            ),
            (operation(LINUX_OPERATIONS, "dogstatsd_stats"), json!({"seconds": 1})),
        ] {
            for code in [1, 126, 127] {
                let error = run_operation(operation, args.clone(), |_| {
                    Ok(output(code, b"stdout-secret", b"stderr-secret"))
                })
                .await
                .unwrap_err();
                assert_eq!(matches!(error, HandleError::Harness { .. }), code == 126 || code == 127);
                assert!(!error.to_string().contains("secret"));
            }
            for error in [
                HandleError::Harness {
                    message: "execution unavailable".into(),
                },
                HandleError::Product {
                    message: "refused".into(),
                },
            ] {
                assert_eq!(
                    run_operation(operation, args.clone(), |_| Err(error.clone()))
                        .await
                        .unwrap_err(),
                    error
                );
            }
        }
    }
}

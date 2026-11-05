//! Commands offered by the Agent and ADP in Linux containers.

use async_trait::async_trait;
use panoramic_core::{
    participant::{OpContext, Operation},
    step::{ExecOutput, HandleError},
};
use serde::Deserialize;
use serde_json::{json, Value};

const ADP: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";
const AGENT: &str = "/opt/datadog-agent/bin/agent/agent";

pub(super) enum Command {
    CliAdp,
    CliAgent,
    ConfigSet,
    DogstatsdCapture,
    DogstatsdReplay,
    DogstatsdStats,
}

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
impl Operation for Command {
    fn name(&self) -> &'static str {
        match self {
            Self::CliAdp => "cli_adp",
            Self::CliAgent => "cli_agent",
            Self::ConfigSet => "config_set",
            Self::DogstatsdCapture => "dogstatsd_capture",
            Self::DogstatsdReplay => "dogstatsd_replay",
            Self::DogstatsdStats => "dogstatsd_stats",
        }
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let (argv, label) = match self {
            Self::CliAdp | Self::CliAgent => {
                let args: CliArgs = decode(args, "{\"args\": [string]}")?;
                let (binary, label) = match self {
                    Self::CliAdp => (ADP, "ADP CLI"),
                    _ => (AGENT, "Agent CLI"),
                };
                (std::iter::once(binary.into()).chain(args.args).collect(), label)
            }
            Self::ConfigSet => {
                let args: ConfigSetArgs = decode(args, "{\"key\": string, \"value\": string}")?;
                (
                    vec![AGENT.into(), "config".into(), "set".into(), args.key, args.value],
                    "Agent config set",
                )
            }
            Self::DogstatsdCapture => {
                let args: CaptureArgs = decode(args, "{\"duration_ms\": unsigned integer}")?;
                (
                    vec![
                        ADP.into(),
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
            Self::DogstatsdReplay => {
                let args: ReplayArgs = decode(args, "{\"path\": string}")?;
                (
                    vec![
                        ADP.into(),
                        "dogstatsd".into(),
                        "replay".into(),
                        "--file".into(),
                        args.path,
                    ],
                    "ADP DogStatsD replay",
                )
            }
            Self::DogstatsdStats => {
                let args: StatsArgs = decode(args, "{\"seconds\": unsigned integer}")?;
                (
                    vec![
                        ADP.into(),
                        "dogstatsd".into(),
                        "stats".into(),
                        "--duration-secs".into(),
                        args.seconds.to_string(),
                    ],
                    "ADP DogStatsD statistics",
                )
            }
        };
        let output = cx.exec(argv).await?;
        match output.exit_code {
            0 => {}
            126 | 127 => {
                return Err(HandleError::Harness {
                    message: format!("{label} could not run (exit {})", output.exit_code),
                })
            }
            code => {
                let mut message = format!("{label} exited with {code}");
                if matches!(self, Self::ConfigSet) {
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
        match self {
            Self::CliAdp | Self::CliAgent => Ok(json!({"output": combined_output(&output)})),
            Self::ConfigSet | Self::DogstatsdReplay => Ok(json!({})),
            Self::DogstatsdCapture => Ok(json!({"path": capture_path_from_output(&combined_output(&output))?})),
            Self::DogstatsdStats => Ok(json!({"metrics": metric_names(&combined_output(&output))?})),
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
    use panoramic_core::participant::test_support::run_operation;

    use super::*;

    fn output(code: i64, stdout: &[u8], stderr: &[u8]) -> ExecOutput {
        ExecOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code: code,
        }
    }

    #[tokio::test]
    async fn adp_and_agent_cli_operations_run_their_binary_and_return_combined_lossy_output() {
        for (operation, binary) in [(&Command::CliAdp, ADP), (&Command::CliAgent, AGENT)] {
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
        for (operation, label) in [(&Command::CliAdp, "ADP CLI"), (&Command::CliAgent, "Agent CLI")] {
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
        for (operation, valid) in [
            (&Command::CliAdp, json!({"args": ["config"]})),
            (&Command::CliAgent, json!({"args": []})),
            (&Command::ConfigSet, json!({"key": "log_level", "value": "debug"})),
            (&Command::DogstatsdCapture, json!({"duration_ms": 1250})),
            (&Command::DogstatsdReplay, json!({"path": "/capture.dog"})),
            (&Command::DogstatsdStats, json!({"seconds": 3})),
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

    #[tokio::test]
    async fn config_set_runs_the_agent_command_and_bounds_failure_stderr_on_a_character_boundary() {
        let result = run_operation(
            &Command::ConfigSet,
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
        let error = run_operation(&Command::ConfigSet, json!({"key": "some.key", "value": "a b"}), |_| {
            Ok(output(1, b"stdout-secret", stderr.as_bytes()))
        })
        .await
        .unwrap_err();
        assert_eq!(
            error,
            HandleError::Product {
                message: format!("Agent config set exited with 1: {}", "é".repeat(256))
            }
        );
        for code in [126, 127] {
            let error = run_operation(&Command::ConfigSet, json!({"key": "x", "value": "y"}), |_| {
                Ok(output(code, b"stdout-secret", b"stderr-secret"))
            })
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn capture_duration_uses_go_milliseconds_and_capture_returns_the_reported_path() {
        // The CLI's path marker and millisecond duration are also used by actions/dogstatsd_replay.rs.
        let result = run_operation(&Command::DogstatsdCapture, json!({"duration_ms": 1250}), |command| {
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
        })
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
            let error = run_operation(&Command::DogstatsdCapture, json!({"duration_ms": 1000}), |_| {
                Ok(output(0, text.as_bytes(), b""))
            })
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
            &Command::DogstatsdReplay,
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
        // The table shape comes from the CLI summary exercised by actions/dogstatsd_replay.rs.
        let table = concat!(
            "logs\n+------------------+------+-------+-----------+\n",
            "| Metric           | Tags | Count | Last Seen |\n",
            "+------------------+------+-------+-----------+\n",
            "| replay.requests  |      | 1     |           |\n",
            "| other.metric     |      | 2     |           |\n",
            "+------------------+------+-------+-----------+\n",
        );
        let result = run_operation(&Command::DogstatsdStats, json!({"seconds": 3}), |command| {
            assert_eq!(command.argv, [ADP, "dogstatsd", "stats", "--duration-secs", "3"]);
            Ok(output(0, table.as_bytes(), b""))
        })
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
            let error = run_operation(&Command::DogstatsdStats, json!({"seconds": 1}), |_| {
                Ok(output(0, text.as_bytes(), b""))
            })
            .await
            .unwrap_err();
            assert!(matches!(error, HandleError::Product { .. }));
        }
    }

    #[tokio::test]
    async fn capture_replay_and_statistics_classify_exits_and_preserve_mechanism_errors() {
        for (operation, args) in [
            (&Command::DogstatsdCapture, json!({"duration_ms": 1000})),
            (&Command::DogstatsdReplay, json!({"path": "/capture"})),
            (&Command::DogstatsdStats, json!({"seconds": 1})),
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

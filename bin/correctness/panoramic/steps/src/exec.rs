//! Runs a command inside a participant.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::{CaseDuration, StepTemplate},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, ExecCommand, HandleError, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use tokio::time::{timeout, Instant};

use crate::message::truncate_message;

/// Runs once, passing only when the command exits successfully.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exec {
    /// Required participant running the command; has no default.
    on: String,
    /// Required nonempty program and argument list; items may reference participant facts.
    argv: Vec<StepTemplate>,
    /// Required, positive execution limit; has no default.
    timeout: CaseDuration,
}

impl Exec {
    pub(crate) const NAME: &'static str = "exec";
}

#[async_trait]
impl Step for Exec {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        std::iter::once(Need::Handle(self.on.clone()))
            .chain(self.argv.iter().flat_map(StepTemplate::needs))
            .collect()
    }

    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let argv = match self
            .argv
            .iter()
            .map(|argument| cx.resolve(argument))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(argv) => argv,
            Err(message) => return StepReport::new(started, AssertionStatus::Errored, truncate_message(message)),
        };
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                truncate_message(format!("participant '{}' has not started", self.on)),
            );
        };
        let command = ExecCommand {
            argv,
            env: vec![],
            stdin: None,
        };
        // Let the step's timeout classify an abandoned command before the mechanism's limit.
        let result = timeout(
            self.budget(),
            handle.exec(command, self.budget().saturating_add(Duration::from_secs(5))),
        )
        .await;
        let (status, message) = match result {
            Ok(Ok(output)) => {
                let text = [
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ]
                .iter()
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
                let status = if output.exit_code == 0 {
                    AssertionStatus::Passed
                } else {
                    AssertionStatus::Failed
                };
                let mut message = format!("command exited with code {}", output.exit_code);
                if !text.is_empty() {
                    message.push_str(": ");
                    message.push_str(&text);
                }
                (status, message)
            }
            Ok(Err(HandleError::Harness { message })) => (AssertionStatus::Errored, message),
            Ok(Err(HandleError::Product { message })) => (AssertionStatus::Failed, message),
            Err(_) => (
                AssertionStatus::Failed,
                format!("command did not exit within {:?}; abandoned", self.budget()),
            ),
        };
        StepReport::new(started, status, truncate_message(message))
    }
}

/// Builds the command action from its case fields.
pub(crate) struct ExecFactory;

impl StepFactory for ExecFactory {
    fn name(&self) -> &'static str {
        Exec::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["argv"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: Exec = decode_fields(fields)?;
        if step.argv.is_empty() {
            return Err("argv must contain a program".into());
        }
        Ok(Box::new(step))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecOutput,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        message::{MAX_MESSAGE_BYTES, TRUNCATION_NOTICE},
        test_support::valid_fields,
    };

    #[tokio::test]
    async fn exec_passes_on_exit_zero_with_its_output() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(ExecOutput {
            stdout: b"  ready\n".to_vec(),
            stderr: vec![],
            exit_code: 0,
        }));
        let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.message, "command exited with code 0: ready");
        fake.assert_executed(&[(
            ExecCommand {
                argv: vec!["tool".into(), "argument".into()],
                env: vec![],
                stdin: None,
            },
            Duration::from_secs(35),
        )]);
    }

    #[tokio::test]
    async fn exec_reports_an_exit_without_a_separator_when_output_is_empty() {
        for (exit_code, status) in [(0, AssertionStatus::Passed), (7, AssertionStatus::Failed)] {
            for output in [b"".as_slice(), b" \n\t"] {
                let fake = FakeParticipant::default();
                fake.push_exec(Ok(ExecOutput {
                    stdout: output.to_vec(),
                    stderr: output.to_vec(),
                    exit_code,
                }));
                let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
                assert_eq!(report.status, status);
                assert_eq!(report.message, format!("command exited with code {exit_code}"));
            }
        }
    }

    #[tokio::test]
    async fn exec_fails_on_a_nonzero_exit_with_code_and_output() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(ExecOutput {
            stdout: b"output\n".to_vec(),
            stderr: b"error\xff\n".to_vec(),
            exit_code: 7,
        }));
        let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(report.message, "command exited with code 7: output\nerror\u{fffd}");
    }

    #[tokio::test(start_paused = true)]
    async fn exec_fails_when_the_command_outlives_its_timeout() {
        let mut fake = FakeParticipant::default();
        fake.delay_exec(Duration::from_secs(31));
        let started = Instant::now();
        let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(report.message, "command did not exit within 30s; abandoned");
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn exec_errors_when_the_mechanism_fails() {
        let fake = FakeParticipant::default();
        fake.push_exec(Err(HandleError::Harness {
            message: "participant stopped".into(),
        }));
        let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "participant stopped");
    }

    #[tokio::test]
    async fn exec_resolves_fact_references_in_argv() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
        }));
        let mut fields = valid_fields(Exec::NAME);
        fields.insert("argv".into(), json!(["tool", "{{ subject.facts.file }}"]));
        assert_eq!(
            ExecFactory.build(&fields).unwrap().needs(),
            [
                Need::Handle("subject".into()),
                Need::Fact("subject".into(), "file".into())
            ]
        );
        let report = run_step_with(&ExecFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        fake.assert_executed(&[(
            ExecCommand {
                argv: vec!["tool".into(), "/logs/output.log".into()],
                env: vec![],
                stdin: None,
            },
            Duration::from_secs(35),
        )]);
        fields.insert("argv".into(), json!(["tool", "{{ subject.late.ip }}"]));
        let report = run_step_with(&ExecFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "participant 'subject' has no late fact 'ip'");
    }

    #[test]
    fn exec_rejects_an_empty_argv_and_non_string_items() {
        let mut fields = valid_fields(Exec::NAME);
        fields.insert("argv".into(), json!([]));
        assert_eq!(ExecFactory.build(&fields).unwrap_err(), "argv must contain a program");
        for value in [json!([true]), json!([3]), json!("tool"), json!({})] {
            fields.insert("argv".into(), value);
            assert!(ExecFactory.build(&fields).is_err());
        }
    }

    #[test]
    fn exec_requires_its_fields_and_a_positive_timeout() {
        for field in ["on", "argv", "timeout"] {
            let mut fields = valid_fields(Exec::NAME);
            fields.remove(field);
            assert!(ExecFactory
                .build(&fields)
                .unwrap_err()
                .contains(&format!("missing field `{field}`")));
        }
        let mut fields = valid_fields(Exec::NAME);
        fields.insert("timeout".into(), json!("0s"));
        assert!(ExecFactory.build(&fields).is_err());
        assert_eq!(ExecFactory.kind(), StepKind::Action);
        assert_eq!(ExecFactory.template_fields(), &["argv"]);
    }

    #[tokio::test]
    async fn exec_truncates_output_and_failure_messages() {
        for answer in [
            Ok(ExecOutput {
                stdout: "é".repeat(MAX_MESSAGE_BYTES).into_bytes(),
                stderr: vec![],
                exit_code: 1,
            }),
            Err(HandleError::Harness {
                message: "é".repeat(MAX_MESSAGE_BYTES),
            }),
            Err(HandleError::Product {
                message: "é".repeat(MAX_MESSAGE_BYTES),
            }),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(answer);
            let report = run_step_with(&ExecFactory, &valid_fields(Exec::NAME), Some(&fake)).await;
            assert!(report.message.len() <= MAX_MESSAGE_BYTES);
            assert!(report.message.ends_with(TRUNCATION_NOTICE));
        }
    }
}

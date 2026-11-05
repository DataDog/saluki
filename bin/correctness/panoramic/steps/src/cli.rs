//! Runs a process's CLI through a participant operation.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::message::truncate_message;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cli {
    on: String,
    process: String,
    args: Vec<String>,
    output_contains: Option<String>,
    timeout: CaseDuration,
}

impl Cli {
    pub(crate) const NAME: &'static str = "cli";
}

#[async_trait]
impl Step for Cli {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), format!("cli_{}", self.process))]
    }

    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let operation = format!("cli_{}", self.process);
        let result = handle.op(&operation, json!({"args": self.args}), self.budget()).await;
        let (status, message) = match result {
            Ok(value) => match value.get("output").and_then(|output| output.as_str()) {
                Some(output) => {
                    let (status, summary) = if self
                        .output_contains
                        .as_ref()
                        .is_some_and(|expected| !output.contains(expected))
                    {
                        (AssertionStatus::Failed, "output did not contain the configured value")
                    } else {
                        (AssertionStatus::Passed, "completed")
                    };
                    (status, format!("{operation} {summary}; output: {}", output.trim()))
                }
                None => (
                    AssertionStatus::Errored,
                    format!("operation '{operation}' returned no string output"),
                ),
            },
            Err(HandleError::Product { message }) => (AssertionStatus::Failed, message),
            Err(HandleError::Harness { message }) => (AssertionStatus::Errored, message),
        };
        StepReport::new(started, status, truncate_message(message))
    }
}

pub(crate) struct CliFactory;

impl StepFactory for CliFactory {
    fn name(&self) -> &'static str {
        Cli::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Cli>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecCommand,
    };

    use super::*;
    use crate::test_support::{output, valid_fields, OPERATIONS};

    #[tokio::test]
    async fn cli_passes_with_matching_output_and_fails_with_missing_output() {
        for (expected, status) in [
            (None, AssertionStatus::Passed),
            (Some("ready"), AssertionStatus::Passed),
            (Some("missing"), AssertionStatus::Failed),
        ] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            fake.push_exec(Ok(output(json!({"output": "  ready\n"}))));
            let mut fields = valid_fields(Cli::NAME);
            if let Some(expected) = expected {
                fields.insert("output_contains".into(), expected.into());
            }
            fields.insert("args".into(), json!(["status", "a b"]));
            let report = run_step_with(&CliFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, status, "{report:?}");
            assert!(report.message.ends_with("output: ready"), "{report:?}");
            fake.assert_executed(&[(
                ExecCommand {
                    argv: vec!["cli_main".into(), json!({"args": ["status", "a b"]}).to_string()],
                    env: vec![],
                    stdin: None,
                },
                Duration::from_secs(30),
            )]);
        }
    }

    #[tokio::test]
    async fn cli_quotes_trimmed_output_verbatim_including_newlines_and_quotes() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        fake.push_exec(Ok(output(json!({"output": "  first\n\"second\"\tthird  "}))));
        let report = run_step_with(&CliFactory, &valid_fields(Cli::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.message, "cli_main completed; output: first\n\"second\"\tthird");
    }

    #[tokio::test]
    async fn cli_fails_on_product_errors_and_errors_on_harness_errors_including_abandoned_commands() {
        for (error, status) in [
            (
                HandleError::Product {
                    message: "CLI exited with 7".into(),
                },
                AssertionStatus::Failed,
            ),
            (
                HandleError::Harness {
                    message: "exec exceeded 30s; abandoned".into(),
                },
                AssertionStatus::Errored,
            ),
        ] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            fake.push_exec(Err(error.clone()));
            let report = run_step_with(&CliFactory, &valid_fields(Cli::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert_eq!(report.message, error.to_string());
        }
    }

    #[tokio::test]
    async fn cli_checks_the_full_output_before_bounding_its_diagnostic() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        fake.push_exec(Ok(output(json!({"output": format!("{}ready", "é".repeat(5000))}))));
        let mut fields = valid_fields(Cli::NAME);
        fields.insert("output_contains".into(), "ready".into());
        let report = run_step_with(&CliFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert!(report.message.len() <= crate::message::MAX_MESSAGE_BYTES);
        assert!(report.message.ends_with(crate::message::TRUNCATION_NOTICE));
    }

    #[tokio::test]
    async fn cli_errors_when_its_operation_returns_no_string_output() {
        for value in [json!({}), json!({"output": 7}), json!({"output": null})] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            fake.push_exec(Ok(output(value)));
            let report = run_step_with(&CliFactory, &valid_fields(Cli::NAME), Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(report.message, "operation 'cli_main' returned no string output");
        }
    }

    #[test]
    fn cli_needs_the_operation_named_for_its_process_and_requires_strict_fields() {
        let mut fields = valid_fields(Cli::NAME);
        fields.insert("process".into(), "other".into());
        assert_eq!(
            CliFactory.build(&fields).unwrap().needs(),
            [Need::Operation("subject".into(), "cli_other".into())]
        );
        for field in ["on", "process", "args", "timeout"] {
            let mut fields = valid_fields(Cli::NAME);
            fields.remove(field);
            assert!(CliFactory
                .build(&fields)
                .unwrap_err()
                .contains(&format!("missing field `{field}`")));
        }
        for args in [
            json!("status"),
            json!([1]),
            json!(["status", false]),
            serde_json::Value::Null,
        ] {
            let mut fields = valid_fields(Cli::NAME);
            fields.insert("args".into(), args);
            assert!(CliFactory.build(&fields).is_err());
        }
        let mut fields = valid_fields(Cli::NAME);
        fields.insert("timeout".into(), "0s".into());
        assert!(CliFactory.build(&fields).is_err());
        assert_eq!(CliFactory.kind(), StepKind::Action);
        assert!(CliFactory.template_fields().is_empty());
    }
}

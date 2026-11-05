//! Sets a participant's runtime configuration through its named operation.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    probe::POLL_INTERVAL,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep_until, Instant};

use crate::attempts::Attempts;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigSet {
    on: String,
    key: String,
    value: Value,
    timeout: CaseDuration,
}

impl ConfigSet {
    pub(crate) const NAME: &'static str = "config_set";
}

#[async_trait]
impl Step for ConfigSet {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), "config_set".into())]
    }

    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.budget();
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let value = match &self.value {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        let args = json!({"key": self.key, "value": value});
        let mut attempts = Attempts::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match handle.op("config_set", args.clone(), remaining).await {
                Ok(_) => {
                    return StepReport::new(
                        started,
                        AssertionStatus::Passed,
                        format!("set configuration key '{}'", self.key),
                    )
                }
                Err(HandleError::Product { message }) => attempts.observed(message),
                Err(HandleError::Harness { message }) => attempts.errored(message),
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        StepReport::new(
            started,
            attempts.status(),
            attempts.explain_expired(&format!(
                "configuration key '{}' was not set within {:?}",
                self.key,
                self.budget()
            )),
        )
    }
}

pub(crate) struct ConfigSetFactory;

impl StepFactory for ConfigSetFactory {
    fn name(&self) -> &'static str {
        ConfigSet::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: ConfigSet = decode_fields(fields)?;
        if step.key.is_empty() {
            return Err("key must not be empty".into());
        }
        Ok(Box::new(step))
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

    #[tokio::test(start_paused = true)]
    async fn config_set_sends_strings_unquoted_and_other_values_as_json() {
        for (value, sent) in [
            (json!("a b"), "a b"),
            (json!(true), "true"),
            (json!(12), "12"),
            (json!(null), "null"),
            (json!([1, 2]), "[1,2]"),
            (json!({"a": 1}), "{\"a\":1}"),
        ] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            fake.push_exec(Ok(output(json!({}))));
            let mut fields = valid_fields(ConfigSet::NAME);
            fields.insert("value".into(), value);
            let report = run_step_with(&ConfigSetFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed, "{report:?}");
            fake.assert_executed(&[(
                ExecCommand {
                    argv: vec![
                        "config_set".into(),
                        json!({"key": "log_level", "value": sent}).to_string(),
                    ],
                    env: vec![],
                    stdin: None,
                },
                Duration::from_secs(30),
            )]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn config_set_retries_until_the_participant_accepts_the_value() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        fake.push_exec(Err(HandleError::Product {
            message: "not ready".into(),
        }));
        fake.push_exec(Ok(output(json!({}))));
        let started = Instant::now();
        let report = run_step_with(&ConfigSetFactory, &valid_fields(ConfigSet::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL);
        let command = || ExecCommand {
            argv: vec![
                "config_set".into(),
                json!({"key": "log_level", "value": "debug"}).to_string(),
            ],
            env: vec![],
            stdin: None,
        };
        fake.assert_executed(&[
            (command(), Duration::from_secs(30)),
            (command(), Duration::from_secs(30) - POLL_INTERVAL),
        ]);
    }

    #[tokio::test(start_paused = true)]
    async fn config_set_fails_after_a_product_error_and_errors_after_only_harness_errors() {
        for product in [false, true] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            if product {
                fake.push_exec(Err(HandleError::Product {
                    message: "refused".into(),
                }));
            }
            fake.push_exec(Err(HandleError::Harness {
                message: "execution unavailable".into(),
            }));
            let mut fields = valid_fields(ConfigSet::NAME);
            fields.insert("timeout".into(), "1s".into());
            let started = Instant::now();
            let report = run_step_with(&ConfigSetFactory, &fields, Some(&fake)).await;
            assert_eq!(
                report.status,
                if product {
                    AssertionStatus::Failed
                } else {
                    AssertionStatus::Errored
                }
            );
            assert_eq!(started.elapsed(), Duration::from_secs(1));
            assert!(
                report
                    .message
                    .contains("was not set within 1s; last attempt: execution unavailable"),
                "{report:?}"
            );
        }
    }

    #[test]
    fn config_set_requires_a_nonempty_key_a_value_and_a_positive_timeout() {
        for field in ["on", "key", "value", "timeout"] {
            let mut fields = valid_fields(ConfigSet::NAME);
            fields.remove(field);
            assert!(ConfigSetFactory
                .build(&fields)
                .unwrap_err()
                .contains(&format!("missing field `{field}`")));
        }
        let mut fields = valid_fields(ConfigSet::NAME);
        fields.insert("key".into(), "".into());
        assert_eq!(ConfigSetFactory.build(&fields).unwrap_err(), "key must not be empty");
        let mut fields = valid_fields(ConfigSet::NAME);
        fields.insert("timeout".into(), "0s".into());
        assert!(ConfigSetFactory.build(&fields).is_err());
        assert_eq!(ConfigSetFactory.kind(), StepKind::Action);
        assert!(ConfigSetFactory.template_fields().is_empty());
    }
}

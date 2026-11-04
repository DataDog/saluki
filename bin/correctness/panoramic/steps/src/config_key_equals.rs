//! Polls a participant's configuration until a dotted key equals an expected JSON value.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    probe::POLL_INTERVAL,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::{de::IntoDeserializer, Deserialize, Deserializer};
use serde_json::{json, Value};
use tokio::time::{sleep_until, Instant};

use crate::attempts::Attempts;

/// Waits for a configuration key to equal the expected JSON value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigKeyEquals {
    /// Required participant whose configuration is read.
    on: String,
    /// Required dotted object path within the configuration.
    key: String,
    /// Required JSON value to match, including null.
    value: Value,
    /// Required configuration view, either source or runtime.
    #[serde(deserialize_with = "View::deserialize_string")]
    view: View,
    /// Required, positive time to wait for a match.
    timeout: CaseDuration,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum View {
    Source,
    Runtime,
}

impl View {
    fn deserialize_string<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::deserialize(String::deserialize(deserializer)?.into_deserializer())
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Runtime => "runtime",
        }
    }
}

impl ConfigKeyEquals {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "config_key_equals";
}

#[async_trait]
impl Step for ConfigKeyEquals {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), "config_dump".into())]
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
        let view = self.view.as_str();
        let args = json!({"view": view});
        let subject = format!(
            "configuration key '{}' of participant '{}' in {} view",
            self.key, self.on, view
        );
        let mut attempts = Attempts::default();
        let mut last_observed = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match handle.op("config_dump", args.clone(), remaining).await {
                Ok(config) => {
                    let observed = self.key.split('.').try_fold(&config, |value, part| value.get(part));
                    if observed == Some(&self.value) {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            format!("{subject} equals {}", self.value),
                        );
                    }
                    match observed {
                        Some(value) => attempts.observed(format!("observed {value}")),
                        None => attempts.observed("key is missing"),
                    }
                    last_observed = observed.cloned();
                }
                Err(HandleError::Product { message }) => attempts.observed(message),
                Err(HandleError::Harness { message }) => attempts.errored(message),
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        let summary = attempts.explain_expired(&format!(
            "{subject} did not equal {} within {:?}",
            self.value,
            self.budget()
        ));
        let last = last_observed.map_or_else(|| "<missing>".to_owned(), |value| value.to_string());
        StepReport::new(
            started,
            attempts.status(),
            format!("{summary}. Last observed value: {last}"),
        )
    }
}

/// Builds `config_key_equals` from its case-file fields.
pub(crate) struct ConfigKeyEqualsFactory;

impl StepFactory for ConfigKeyEqualsFactory {
    fn name(&self) -> &'static str {
        ConfigKeyEquals::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<ConfigKeyEquals>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecOutput,
    };

    use super::*;
    use crate::test_support::valid_fields;

    fn dump(config: Value) -> ExecOutput {
        ExecOutput {
            stdout: serde_json::to_vec(&config).unwrap(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_configuration_key_matches_any_json_value_in_either_view() {
        for view in ["source", "runtime"] {
            for value in [
                Value::Null,
                json!(false),
                json!(7),
                json!(1.5),
                json!("ready"),
                json!([true, null]),
                json!({"enabled": true}),
            ] {
                let fake = FakeParticipant::default();
                fake.push_exec(Ok(dump(json!({"domains": {"enabled": value}}))));
                let mut fields = valid_fields(ConfigKeyEquals::NAME);
                fields.insert("view".into(), json!(view));
                fields.insert("value".into(), value.clone());
                let started = Instant::now();
                let report = run_step_with(&ConfigKeyEqualsFactory, &fields, Some(&fake)).await;
                assert_eq!(report.status, AssertionStatus::Passed, "{view}: {value}");
                assert_eq!(started.elapsed(), Duration::ZERO);
                assert_eq!(
                    report.message,
                    format!(
                        "configuration key 'domains.enabled' of participant 'subject' in {view} view equals {value}"
                    )
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_changing_configuration_value_is_retried_until_it_matches() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!({}))));
        fake.push_exec(Ok(dump(json!({"domains": {"enabled": false}}))));
        fake.push_exec(Ok(dump(json!({"domains": {"enabled": true}}))));
        let started = Instant::now();
        let report = run_step_with(
            &ConfigKeyEqualsFactory,
            &valid_fields(ConfigKeyEquals::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL * 2);
        assert_eq!(
            report.message,
            "configuration key 'domains.enabled' of participant 'subject' in runtime view equals true"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn operation_errors_are_retried_until_a_configuration_matches() {
        let fake = FakeParticipant::default();
        fake.push_exec(Err(HandleError::Harness {
            message: "execution connection lost".into(),
        }));
        fake.push_exec(Err(HandleError::Product {
            message: "configuration unavailable".into(),
        }));
        fake.push_exec(Ok(dump(json!({"domains": {"enabled": true}}))));
        let started = Instant::now();
        let report = run_step_with(
            &ConfigKeyEqualsFactory,
            &valid_fields(ConfigKeyEquals::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_keeps_the_last_observed_value_or_reports_a_missing_key() {
        for (config, expected, diagnostic) in [
            (json!({"domains": {"enabled": false}}), "false", "observed false"),
            (json!({}), "<missing>", "key is missing"),
            (json!({"domains": {"enabled": null}}), "null", "observed null"),
            (json!({"domains": {"enabled": "true"}}), "\"true\"", "observed \"true\""),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(config)));
            let started = Instant::now();
            let report = run_step_with(
                &ConfigKeyEqualsFactory,
                &valid_fields(ConfigKeyEquals::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert_eq!(report.message, format!(
                "configuration key 'domains.enabled' of participant 'subject' in runtime view did not equal true within 30s; last attempt: {diagnostic}. Last observed value: {expected}"
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn later_operation_errors_keep_the_last_value_and_failed_status() {
        for error in [
            HandleError::Harness {
                message: "execution connection lost".into(),
            },
            HandleError::Product {
                message: "configuration unavailable".into(),
            },
        ] {
            let diagnostic = match &error {
                HandleError::Harness { message } | HandleError::Product { message } => message.clone(),
            };
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!({"domains": {"enabled": 1}}))));
            fake.push_exec(Ok(dump(json!({"domains": {"enabled": 2}}))));
            fake.push_exec(Err(error));
            let started = Instant::now();
            let report = run_step_with(
                &ConfigKeyEqualsFactory,
                &valid_fields(ConfigKeyEquals::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert!(
                report
                    .message
                    .ends_with(&format!("last attempt: {diagnostic}. Last observed value: 2")),
                "{}",
                report.message
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_key_replaces_an_earlier_observed_value_and_does_not_match_null() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!({"domains": {"enabled": false}}))));
        fake.push_exec(Ok(dump(json!({}))));
        let mut fields = valid_fields(ConfigKeyEquals::NAME);
        fields.insert("value".into(), Value::Null);
        let report = run_step_with(&ConfigKeyEqualsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(
            report
                .message
                .ends_with("last attempt: key is missing. Last observed value: <missing>"),
            "{}",
            report.message
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_after_only_operation_errors_uses_attempt_classification() {
        for (error, status, diagnostic) in [
            (
                HandleError::Harness {
                    message: "execution connection lost".into(),
                },
                AssertionStatus::Errored,
                "execution connection lost",
            ),
            (
                HandleError::Product {
                    message: "configuration unavailable".into(),
                },
                AssertionStatus::Failed,
                "configuration unavailable",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Err(error));
            let started = Instant::now();
            let report = run_step_with(
                &ConfigKeyEqualsFactory,
                &valid_fields(ConfigKeyEquals::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, status);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert!(
                report
                    .message
                    .ends_with(&format!("last attempt: {diagnostic}. Last observed value: <missing>")),
                "{}",
                report.message
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_participant_exit_keeps_retrying_until_the_timeout() {
        let fake = FakeParticipant::default();
        fake.complete();
        fake.push_exec(Err(HandleError::Harness {
            message: "participant exited".into(),
        }));
        let started = Instant::now();
        let report = run_step_with(
            &ConfigKeyEqualsFactory,
            &valid_fields(ConfigKeyEquals::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(report
            .message
            .ends_with("last attempt: participant exited. Last observed value: <missing>"));
    }

    #[test]
    fn the_view_must_be_source_or_runtime() {
        for view in ["source", "runtime"] {
            let mut fields = valid_fields(ConfigKeyEquals::NAME);
            fields.insert("view".into(), json!(view));
            assert!(ConfigKeyEqualsFactory.build(&fields).is_ok());
        }
        for view in [
            json!("combined"),
            json!("Runtime"),
            json!(""),
            Value::Null,
            json!(true),
            json!(3),
            json!(["source"]),
            json!({"source": null}),
        ] {
            let mut fields = valid_fields(ConfigKeyEquals::NAME);
            fields.insert("view".into(), view.clone());
            let message = ConfigKeyEqualsFactory.build(&fields).unwrap_err();
            if let Some(view) = view.as_str() {
                assert_eq!(
                    message,
                    format!("view: unknown variant `{view}`, expected `source` or `runtime`")
                );
            } else {
                assert!(message.starts_with("view: invalid type:"), "{view}: {message}");
                assert!(message.ends_with("expected a string"), "{view}: {message}");
            }
        }
    }

    #[test]
    fn the_configuration_step_requires_every_field_and_needs_the_dump_operation() {
        for field in ["on", "key", "value", "view", "timeout"] {
            let mut fields = valid_fields(ConfigKeyEquals::NAME);
            fields.remove(field).unwrap();
            let message = ConfigKeyEqualsFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let step = ConfigKeyEqualsFactory
            .build(&valid_fields(ConfigKeyEquals::NAME))
            .unwrap();
        assert_eq!(
            step.needs(),
            vec![Need::Operation("subject".into(), "config_dump".into())]
        );
        assert!(ConfigKeyEqualsFactory.template_fields().is_empty());
    }
}

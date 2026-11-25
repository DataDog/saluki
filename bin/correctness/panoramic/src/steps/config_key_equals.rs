//! `config_key_equals`: a key of a participant's configuration holds a value.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep_until, timeout_at, Instant};

use super::{stopped, EXEC_SLACK, POLL};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};

/// The operation that answers with the participant's configuration.
const CONFIG_DUMP: &str = "config_dump";

pub(super) struct ConfigKeyEqualsFactory;

impl StepFactory for ConfigKeyEqualsFactory {
    fn name(&self) -> &'static str {
        "config_key_equals"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<ConfigKeyEquals>()?))
    }
}

/// Which view of the configuration to read.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum View {
    /// The configuration as read from its sources.
    Source,
    /// The configuration the software runs with, after it has changed.
    Runtime,
}

impl View {
    fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Runtime => "runtime",
        }
    }
}

/// Passes once the configuration key `key`, with its parts joined by `.`, equals `value` in `view` within `timeout`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigKeyEquals {
    on: String,
    key: String,
    value: Value,
    view: View,
    timeout: Timeout,
}

#[async_trait]
impl Step for ConfigKeyEquals {
    fn name(&self) -> &'static str {
        "config_key_equals"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), CONFIG_DUMP.to_string())]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let looked = async {
            let handle = cx.handle(&self.on)?;
            let mut last = None;
            let last = loop {
                if Instant::now() >= cx.deadline {
                    break last;
                }
                let started = Instant::now();
                // The dump may take the step's remaining time, so a slow dump during startup still answers. It gets
                // room past that time, so that the timeout here ends it and a dump that outlives the step is reported
                // by the step and not as a problem of the mechanism.
                let limit = cx.deadline.saturating_duration_since(started) + EXEC_SLACK;
                let dump = handle.op(CONFIG_DUMP, json!({ "view": self.view.as_str() }), limit);
                // An attempt that is cut short leaves `last` as it was.
                if let Ok(dumped) = timeout_at(cx.deadline, dump).await {
                    match dumped {
                        Ok(config) => match self.key.split('.').try_fold(&config, |node, part| node.get(part)) {
                            Some(value) if *value == self.value => {
                                let message = format!(
                                    "Config key '{}' of '{}' ({} view) equals {}.",
                                    self.key,
                                    self.on,
                                    self.view.as_str(),
                                    self.value
                                );
                                return Ok(StepReport {
                                    status: StepStatus::Passed,
                                    message,
                                });
                            }
                            Some(value) => last = Some(value.to_string()),
                            None => last = Some("<missing>".to_string()),
                        },
                        Err(HandleError::Software(message)) => last = Some(message),
                        Err(e) => return Err(e),
                    }
                }
                sleep_until((started + POLL).min(cx.deadline)).await;
            };
            let message = format!(
                "Config key '{}' of '{}' ({} view) did not equal {} within {}: last {}",
                self.key,
                self.on,
                self.view.as_str(),
                self.value,
                self.timeout.text,
                last.as_deref().unwrap_or("<no answer>")
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Failed,
                message,
            })
        };
        looked.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted, CONFIG_FILE};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_key_that_differs_fails_with_the_last_value() {
        let fields = "{on: app, key: a.b, value: true, view: runtime, timeout: 1s}";
        let configured = Scripted {
            files: [(CONFIG_FILE, Ok(br#"{"a": {"b": false}}"#.to_vec()))].into(),
            ..Scripted::default()
        };
        let report = run(&ConfigKeyEqualsFactory, fields, &configured, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("false"), "{}", report.message);
    }
}

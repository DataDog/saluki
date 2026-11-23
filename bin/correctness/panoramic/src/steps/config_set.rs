//! `config_set`: sets a key of a participant's runtime configuration through its Agent.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::time::{sleep_until, timeout_at, Instant};

use super::{stopped, ATTEMPT, EXEC_SLACK, POLL};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepPurpose, StepReport};

/// The operation that sets the key.
const CONFIG_SET: &str = "config_set";

pub(super) struct ConfigSetFactory;

impl StepFactory for ConfigSetFactory {
    fn name(&self) -> &'static str {
        "config_set"
    }

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step = fields.decode::<ConfigSet>()?;
        if let Some(bad) = step
            .key
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        {
            return Err(format!(
                "key '{}' holds '{}', and a key holds only ASCII letters, digits, '_', '-' and '.'",
                step.key, bad
            ));
        }
        Ok(Box::new(step))
    }
}

/// Sets the configuration key `key` to `value`, trying again until the participant's Agent accepts it or `timeout`
/// ends.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSet {
    on: String,
    key: String,
    value: String,
    timeout: Timeout,
}

#[async_trait]
impl Step for ConfigSet {
    fn name(&self) -> &'static str {
        "config_set"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), CONFIG_SET.to_string())]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let set = async {
            let handle = cx.handle(&self.on)?;
            let mut last = None;
            let last = loop {
                if Instant::now() >= cx.deadline {
                    break last;
                }
                let started = Instant::now();
                // The attempt is cut short by the timeout here, and not by the command, which gets room past it. A
                // command that outlives the attempt is a refusal, not a problem of the mechanism.
                let attempt_end = (started + ATTEMPT).min(cx.deadline);
                let limit = attempt_end.saturating_duration_since(started) + EXEC_SLACK;
                let args = json!({ "key": self.key, "value": self.value });
                match timeout_at(attempt_end, handle.op(CONFIG_SET, args, limit)).await {
                    Ok(Ok(_)) => {
                        let message = format!(
                            "Set '{}' to '{}' on '{}' through the Agent.",
                            self.key, self.value, self.on
                        );
                        return Ok(StepReport {
                            status: StepStatus::Passed,
                            message,
                        });
                    }
                    // The Agent's CLI does not tell a refusal from an Agent that is not ready yet, so every software
                    // error is tried again.
                    Ok(Err(HandleError::Software(message))) => last = Some(message),
                    Ok(Err(e)) => return Err(e),
                    // An attempt that the step's deadline cuts short leaves `last` as it was.
                    Err(_) if attempt_end < cx.deadline => {
                        last = Some(format!("no answer within {}s", ATTEMPT.as_secs()));
                    }
                    Err(_) => {}
                }
                sleep_until((started + POLL).min(cx.deadline)).await;
            };
            let message = format!(
                "The Agent of '{}' did not set '{}' to '{}' within {}: last {}",
                self.on,
                self.key,
                self.value,
                self.timeout.text,
                last.as_deref().unwrap_or("<no answer>")
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Failed,
                message,
            })
        };
        set.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_agent_that_never_sets_the_key_fails_at_the_deadline() {
        let fields = "{on: app, key: log_level, value: debug, timeout: 1s}";
        let report = run(&ConfigSetFactory, fields, &Scripted::default(), &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("log_level"), "{}", report.message);
    }
}

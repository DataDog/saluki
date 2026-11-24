//! `exits_with`: a process exits with the expected code.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use super::stopped;
use crate::core::case::Timeout;
use crate::core::handle::RECORD_READ_TIME;
use crate::core::observe::ProcessExit;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};

pub(super) struct ExitsWithFactory;

impl StepFactory for ExitsWithFactory {
    fn name(&self) -> &'static str {
        "exits_with"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<ExitsWith>()?))
    }
}

/// Passes when the process exits with `code` within `timeout`. A death by signal has code 256, so it fails any other
/// `code`, and the message names the signal. Errors when the exit cannot be determined.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitsWith {
    on: String,
    process: String,
    code: i64,
    timeout: Timeout,
}

#[async_trait]
impl Step for ExitsWith {
    fn name(&self) -> &'static str {
        "exits_with"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Process(self.on.clone(), self.process.clone())]
    }

    /// The wait lasts `timeout`, and `wait_for_exit` then reads the exit record once more, for up to
    /// `RECORD_READ_TIME`.
    fn budget(&self) -> Duration {
        self.timeout.duration + RECORD_READ_TIME
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let waited = async {
            let handle = cx.handle(&self.on)?;
            handle.wait_for_exit(&self.process, self.timeout.duration).await
        };
        let who = format!("Process '{}' of '{}'", self.process, self.on);
        let (status, message) = match waited.await {
            Ok(ProcessExit::Running) => (
                StepStatus::Failed,
                format!("{} did not exit within {}.", who, self.timeout.text),
            ),
            Ok(ProcessExit::Exited(status)) if status.code == self.code => {
                (StepStatus::Passed, format!("{} exited with code {}.", who, self.code))
            }
            Ok(ProcessExit::Exited(status)) => (
                StepStatus::Failed,
                format!("{} exited with {}, expected code {}.", who, status, self.code),
            ),
            Ok(ProcessExit::Undetermined(why)) => (
                StepStatus::Errored,
                format!(
                    "The exit of process '{}' of '{}' cannot be determined: {}",
                    self.process, self.on, why
                ),
            ),
            Err(e) => return stopped(e),
        };
        StepReport { status, message }
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_wrong_code_fails_and_an_undetermined_exit_errors() {
        let fields = "{on: app, process: job, code: 1, timeout: 1s}";
        let exited = Scripted {
            files: [(
                "/airlock/app/exit/job",
                Ok(b"{\"code\":2,\"signal\":0,\"time\":\"1794511601.5\"}\n".to_vec()),
            )]
            .into(),
            ..Scripted::default()
        };
        let report = run(&ExitsWithFactory, fields, &exited, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("code 2"), "{}", report.message);

        // The participant has exited and the record holds no exit.
        let observation = observation();
        observation.exited(Ok(0));
        let report = run(&ExitsWithFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
    }
}

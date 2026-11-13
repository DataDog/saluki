//! `process_stable_for`: a process, or every process of a participant, keeps running for a while.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt as _};
use serde::Deserialize;
use tokio::time::Instant;

use super::{described, stopped};
use crate::core::case::Timeout;
use crate::core::handle::{HandleError, RECORD_READ_TIME};
use crate::core::observe::ProcessExit;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};

pub(super) struct ProcessStableForFactory;

impl StepFactory for ProcessStableForFactory {
    fn name(&self) -> &'static str {
        "process_stable_for"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<ProcessStableFor>()?))
    }
}

/// Passes once the process, or every process of the participant, has kept running for `duration`. Fails when one
/// exits, and errors when an exit cannot be determined.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessStableFor {
    on: String,
    #[serde(default)]
    process: Option<String>,
    duration: Timeout,
}

#[async_trait]
impl Step for ProcessStableFor {
    fn name(&self) -> &'static str {
        "process_stable_for"
    }

    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Participant(self.on.clone())],
        }
    }

    /// The watch lasts `duration`, and `wait_for_exit` then reads each exit record once more, for up to
    /// `RECORD_READ_TIME`.
    fn budget(&self) -> Duration {
        self.duration.duration + RECORD_READ_TIME
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let duration = self.duration.duration;
        let watched = async {
            let handle = cx.handle(&self.on)?;
            let processes = match &self.process {
                Some(process) => vec![process.as_str()],
                None => handle.processes(),
            };
            let mut exits: FuturesUnordered<_> = processes
                .into_iter()
                .map(|process| async move { (process, handle.wait_for_exit(process, duration).await) })
                .collect();
            while let Some((process, exit)) = exits.next().await {
                let message = match exit? {
                    ProcessExit::Running => continue,
                    ProcessExit::Exited(status) => format!(
                        "Process '{}' of '{}' exited with {} after {:.1?}, within {}.",
                        process,
                        self.on,
                        status,
                        started.elapsed(),
                        self.duration.text
                    ),
                    ProcessExit::Undetermined(why) => {
                        let message = format!(
                            "The exit of process '{}' of '{}' cannot be determined: {}",
                            process, self.on, why
                        );
                        return Ok(StepReport {
                            status: StepStatus::Errored,
                            message,
                        });
                    }
                };
                return Ok(StepReport {
                    status: StepStatus::Failed,
                    message,
                });
            }
            let message = format!(
                "{} kept running for {}.",
                described(&self.on, self.process.as_deref()),
                self.duration.text
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Passed,
                message,
            })
        };
        watched.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_exit_fails_and_an_undetermined_exit_errors() {
        let fields = "{on: app, process: job, duration: 10s}";
        let exited = Scripted {
            files: [("/airlock/app/exit/job", Ok(b"1 0 1794511601.5\n".to_vec()))].into(),
            ..Scripted::default()
        };
        let report = run(&ProcessStableForFactory, fields, &exited, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);

        // The participant has exited and the record holds no exit.
        let observation = observation();
        observation.exited(Ok(0));
        let report = run(&ProcessStableForFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
    }
}

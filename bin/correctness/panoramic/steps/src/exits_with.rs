//! Waits for a process's first exit and compares its code exactly.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, ExitObservation, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

/// Requires a named process to exit with exactly the expected code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExitsWith {
    /// Required participant whose process is observed.
    on: String,
    /// Required process whose first exit is checked.
    process: String,
    /// Required exit code, compared exactly.
    code: i32,
    /// Required, positive time to wait for the exit.
    timeout: CaseDuration,
}

impl ExitsWith {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "exits_with";
}

#[async_trait]
impl Step for ExitsWith {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Process(self.on.clone(), self.process.clone())]
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
        let observation = tokio::select! {
            biased;
            observation = handle.wait_for_exit(&self.process) => observation,
            _ = sleep_until(deadline) => handle.refresh_exit(&self.process).await,
        };
        let subject = format!("process '{}' of participant '{}'", self.process, self.on);
        let (status, message) = match observation {
            ExitObservation::Exited(exit) if exit.code == Some(self.code) => (
                AssertionStatus::Passed,
                format!("{subject} exited with expected code {}", self.code),
            ),
            ExitObservation::Exited(exit) => (
                AssertionStatus::Failed,
                format!("{subject} exited with {exit}, expected code {}", self.code),
            ),
            ExitObservation::Undetermined(reason) => (
                AssertionStatus::Errored,
                format!("exit status of {subject} is undetermined: {reason}"),
            ),
            ExitObservation::Running => (
                AssertionStatus::Failed,
                format!("{subject} still running after {:?}", self.budget()),
            ),
        };
        StepReport::new(started, status, message)
    }
}

/// Builds `exits_with` from its case-file fields.
pub(crate) struct ExitsWithFactory;

impl StepFactory for ExitsWithFactory {
    fn name(&self) -> &'static str {
        ExitsWith::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<ExitsWith>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::Value;

    use super::*;
    use crate::test_support::{exited, valid_fields};

    #[tokio::test(start_paused = true)]
    async fn an_exact_exit_code_passes_and_another_code_or_a_signal_fails() {
        for (code, signal, status, expected) in [
            (Some(1), None, AssertionStatus::Passed, "exited with expected code 1"),
            (
                Some(0),
                None,
                AssertionStatus::Failed,
                "exited with code 0, expected code 1",
            ),
            (
                Some(10),
                None,
                AssertionStatus::Failed,
                "exited with code 10, expected code 1",
            ),
            (
                None,
                Some(9),
                AssertionStatus::Failed,
                "exited with signal 9, expected code 1",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.set_exit("main", exited(code, signal));
            let started = Instant::now();
            let report = run_step_with(&ExitsWithFactory, &valid_fields(ExitsWith::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert_eq!(started.elapsed(), Duration::ZERO);
            assert_eq!(
                report.message,
                format!("process 'main' of participant 'subject' {expected}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_exit_step_waits_for_the_named_process() {
        let fake = FakeParticipant::default();
        let fields = valid_fields(ExitsWith::NAME);
        let started = Instant::now();
        let (report, ()) = tokio::join!(run_step_with(&ExitsWithFactory, &fields, Some(&fake)), async {
            fake.set_exit("worker", exited(Some(9), None));
            tokio::time::sleep(Duration::from_secs(2)).await;
            fake.set_exit("main", exited(Some(1), None));
        });
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(
            report.message,
            "process 'main' of participant 'subject' exited with expected code 1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_process_still_running_after_the_timeout_fails() {
        let fake = FakeParticipant::default();
        let started = Instant::now();
        let report = run_step_with(&ExitsWithFactory, &valid_fields(ExitsWith::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(
            report.message,
            "process 'main' of participant 'subject' still running after 30s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_exit_step_reads_once_more_at_the_timeout() {
        for (observation, status, expected) in [
            (
                exited(Some(1), None),
                AssertionStatus::Passed,
                "exited with expected code 1",
            ),
            (
                exited(Some(10), None),
                AssertionStatus::Failed,
                "exited with code 10, expected code 1",
            ),
            (
                ExitObservation::Undetermined("exit record unreadable".into()),
                AssertionStatus::Errored,
                "exit status of process 'main' of participant 'subject' is undetermined: exit record unreadable",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.reveal_on_refresh("main", observation);
            let started = Instant::now();
            let report = run_step_with(&ExitsWithFactory, &valid_fields(ExitsWith::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert!(report.message.ends_with(expected), "{}", report.message);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_undetermined_exit_errors_the_exit_step() {
        for after in [Duration::ZERO, Duration::from_secs(2)] {
            let fake = FakeParticipant::default();
            let fields = valid_fields(ExitsWith::NAME);
            if after.is_zero() {
                fake.set_exit("main", ExitObservation::Undetermined("exit observation lost".into()));
            }
            let started = Instant::now();
            let (report, ()) = tokio::join!(run_step_with(&ExitsWithFactory, &fields, Some(&fake)), async {
                if !after.is_zero() {
                    tokio::time::sleep(after).await;
                    fake.set_exit("main", ExitObservation::Undetermined("exit observation lost".into()));
                }
            });
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(started.elapsed(), after);
            assert_eq!(
                report.message,
                "exit status of process 'main' of participant 'subject' is undetermined: exit observation lost"
            );
        }
    }

    #[test]
    fn the_exit_step_requires_its_participant_process_code_and_timeout() {
        for field in ["on", "process", "code", "timeout"] {
            let mut fields = valid_fields(ExitsWith::NAME);
            fields.remove(field).unwrap();
            let message = ExitsWithFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let mut fields = valid_fields(ExitsWith::NAME);
        fields.insert("process".into(), Value::Null);
        assert!(ExitsWithFactory
            .build(&fields)
            .unwrap_err()
            .starts_with("process: invalid type:"));
        let step = ExitsWithFactory.build(&valid_fields(ExitsWith::NAME)).unwrap();
        assert_eq!(step.needs(), vec![Need::Process("subject".into(), "main".into())]);
    }

    #[test]
    fn the_exit_code_must_be_an_i32() {
        for value in [
            Value::String("1".into()),
            Value::Bool(true),
            Value::from(i64::from(i32::MAX) + 1),
        ] {
            let mut fields = valid_fields(ExitsWith::NAME);
            fields.insert("code".into(), value);
            let message = ExitsWithFactory.build(&fields).unwrap_err();
            assert!(message.starts_with("code:"), "{message}");
        }
    }
}

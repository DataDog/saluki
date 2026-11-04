//! Checks that named processes stay running through a time window.

use std::{
    future::{poll_fn, Future as _},
    task::Poll,
    time::Duration,
};

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, ExitObservation, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

/// Requires every selected process to stay running for a whole window.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessStableFor {
    /// Required participant whose processes are observed.
    on: String,
    /// Process to observe. Defaults to every declared process of the participant.
    #[serde(default)]
    process: Option<String>,
    /// Required, positive stability window.
    duration: CaseDuration,
}

impl ProcessStableFor {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "process_stable_for";

    fn exit_report(
        &self, process: &str, observation: ExitObservation, started: Instant, before_window: bool,
    ) -> StepReport {
        let subject = format!("process '{process}' of participant '{}'", self.on);
        let (status, message) = match observation {
            ExitObservation::Exited(exit) => {
                let when = if before_window {
                    "before the window".to_owned()
                } else {
                    format!("after {:?}", started.elapsed())
                };
                (AssertionStatus::Failed, format!("{subject} exited {when}: {exit}"))
            }
            ExitObservation::Undetermined(reason) => (
                AssertionStatus::Errored,
                format!("exit status of {subject} is undetermined: {reason}"),
            ),
            ExitObservation::Running => (
                AssertionStatus::Errored,
                format!("exit wait for {subject} returned while it was still running"),
            ),
        };
        StepReport::new(started, status, message)
    }
}

#[async_trait]
impl Step for ProcessStableFor {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
    }

    fn budget(&self) -> Duration {
        self.duration.into()
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
        let processes = match &self.process {
            Some(process) => vec![process.as_str()],
            None => handle.processes().collect(),
        };
        if processes.is_empty() {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' declares no processes to watch", self.on),
            );
        }
        for process in &processes {
            match handle.exit(process) {
                ExitObservation::Running => {}
                observation => return self.exit_report(process, observation, started, true),
            }
        }
        let mut exits = processes
            .iter()
            .map(|&process| {
                let handle = &handle;
                Box::pin(async move { (process, handle.wait_for_exit(process).await) })
            })
            .collect::<Vec<_>>();
        tokio::select! {
            biased;
            (process, observation) = poll_fn(|cx| {
                for exit in &mut exits {
                    if let Poll::Ready(observation) = exit.as_mut().poll(cx) {
                        return Poll::Ready(observation);
                    }
                }
                Poll::Pending
            }) => return self.exit_report(process, observation, started, false),
            _ = sleep_until(deadline) => {}
        }
        let finals = futures::future::join_all(processes.iter().map(|process| handle.refresh_exit(process))).await;
        for (process, observation) in processes.iter().zip(finals) {
            match observation {
                ExitObservation::Running => {}
                observation => return self.exit_report(process, observation, started, false),
            }
        }
        StepReport::new(
            started,
            AssertionStatus::Passed,
            format!("processes {} stayed up for {:?}", processes.join(", "), self.budget()),
        )
    }
}

/// Builds `process_stable_for` from its case-file fields.
pub(crate) struct ProcessStableForFactory;

impl StepFactory for ProcessStableForFactory {
    fn name(&self) -> &'static str {
        ProcessStableFor::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<ProcessStableFor>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::Value;

    use super::*;
    use crate::test_support::{exited, valid_fields};

    #[tokio::test(start_paused = true)]
    async fn every_process_stays_running_through_the_window() {
        let fake = FakeParticipant::default();
        let started = Instant::now();
        let report = run_step_with(
            &ProcessStableForFactory,
            &valid_fields(ProcessStableFor::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(report.message, "processes main, worker stayed up for 10s");
    }

    #[tokio::test(start_paused = true)]
    async fn stability_watches_only_the_named_process() {
        let fake = FakeParticipant::default();
        let mut fields = valid_fields(ProcessStableFor::NAME);
        fields.insert("process".into(), Value::String("worker".into()));
        let started = Instant::now();
        let (report, ()) = tokio::join!(run_step_with(&ProcessStableForFactory, &fields, Some(&fake)), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            fake.set_exit("main", exited(Some(1), None));
        });
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(report.message, "processes worker stayed up for 10s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_process_exit_during_the_window_fails_with_its_name_status_and_elapsed_time() {
        for (process, code, signal, status) in
            [("main", Some(1), None, "code 1"), ("worker", None, Some(9), "signal 9")]
        {
            let fake = FakeParticipant::default();
            let fields = valid_fields(ProcessStableFor::NAME);
            let started = Instant::now();
            let (report, ()) = tokio::join!(run_step_with(&ProcessStableForFactory, &fields, Some(&fake)), async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                fake.set_exit(process, exited(code, signal));
            });
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(started.elapsed(), Duration::from_secs(2));
            assert_eq!(
                report.message,
                format!("process '{process}' of participant 'subject' exited after 2s: {status}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_exit_before_the_window_fails_stability_immediately() {
        let fake = FakeParticipant::default();
        fake.set_exit("worker", exited(Some(1), None));
        let started = Instant::now();
        let report = run_step_with(
            &ProcessStableForFactory,
            &valid_fields(ProcessStableFor::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(
            report.message,
            "process 'worker' of participant 'subject' exited before the window: code 1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_exit_revealed_only_by_the_final_read_fails_stability() {
        let fake = FakeParticipant::default();
        fake.reveal_on_refresh("worker", exited(None, Some(9)));
        let started = Instant::now();
        let report = run_step_with(
            &ProcessStableForFactory,
            &valid_fields(ProcessStableFor::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(
            report.message,
            "process 'worker' of participant 'subject' exited after 10s: signal 9"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_undetermined_exit_errors_stability_before_during_or_after_the_window() {
        for after in [Duration::ZERO, Duration::from_secs(2), Duration::from_secs(10)] {
            let fake = FakeParticipant::default();
            let fields = valid_fields(ProcessStableFor::NAME);
            let observation = ExitObservation::Undetermined("exit record unreadable".into());
            if after.is_zero() {
                fake.set_exit("worker", observation.clone());
            } else if after == Duration::from_secs(10) {
                fake.reveal_on_refresh("worker", observation.clone());
            }
            let started = Instant::now();
            let (report, ()) = tokio::join!(run_step_with(&ProcessStableForFactory, &fields, Some(&fake)), async {
                if after == Duration::from_secs(2) {
                    tokio::time::sleep(after).await;
                    fake.set_exit("worker", observation);
                }
            });
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(started.elapsed(), after);
            assert_eq!(
                report.message,
                "exit status of process 'worker' of participant 'subject' is undetermined: exit record unreadable"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn final_reads_share_one_refresh_window_and_report_in_process_order() {
        // Concurrent reads keep the total refresh time within the driver's slack, even with a slow first process.
        for (main_delay, worker_delay, expected_status, elapsed, message) in [
            (None, None, AssertionStatus::Errored, 15,
             "exit status of process 'main' of participant 'subject' is undetermined: exit record of process 'main' was not re-read within 5s"),
            (Some(4), None, AssertionStatus::Errored, 15,
             "exit status of process 'worker' of participant 'subject' is undetermined: exit record of process 'worker' was not re-read within 5s"),
            (Some(4), Some(4), AssertionStatus::Passed, 14,
             "processes main, worker stayed up for 10s"),
        ] {
            let mut fake = FakeParticipant::default();
            fake.delay_refresh("main", main_delay.map(Duration::from_secs));
            fake.delay_refresh("worker", worker_delay.map(Duration::from_secs));
            let started = Instant::now();
            let report = run_step_with(
                &ProcessStableForFactory,
                &valid_fields(ProcessStableFor::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, expected_status);
            assert_eq!(started.elapsed(), Duration::from_secs(elapsed));
            assert_eq!(report.message, message);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn final_exit_reports_follow_process_order_instead_of_read_completion_order() {
        let mut fake = FakeParticipant::default();
        fake.delay_refresh("main", Some(Duration::from_secs(3)));
        fake.reveal_on_refresh("main", exited(Some(1), None));
        fake.reveal_on_refresh("worker", exited(Some(2), None));
        let report = run_step_with(
            &ProcessStableForFactory,
            &valid_fields(ProcessStableFor::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(
            report.message,
            "process 'main' of participant 'subject' exited after 13s: code 1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_participant_without_processes_errors_immediately() {
        let fake = FakeParticipant::new(&[]);
        let started = Instant::now();
        let report = run_step_with(
            &ProcessStableForFactory,
            &valid_fields(ProcessStableFor::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "participant 'subject' declares no processes to watch");
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[test]
    fn stability_requires_a_participant_and_duration_and_needs_the_selected_processes() {
        let mut fields = valid_fields(ProcessStableFor::NAME);
        for field in ["on", "duration"] {
            let mut missing = fields.clone();
            missing.remove(field).unwrap();
            let message = ProcessStableForFactory.build(&missing).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let step: ProcessStableFor = decode_fields(&fields).unwrap();
        assert_eq!(step.process, None);
        assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
        fields.insert("process".into(), Value::String("worker".into()));
        let step = ProcessStableForFactory.build(&fields).unwrap();
        assert_eq!(step.needs(), vec![Need::Process("subject".into(), "worker".into())]);
    }
}

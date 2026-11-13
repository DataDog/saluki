//! Steps: the assertions and actions a case's procedure runs.
//!
//! Each step decodes its own fields strictly and reaches a participant only through the handle its context gives it.
//! A problem of the mechanism, a [`HandleError::Harness`], makes the step `errored`; a condition that does not hold, or
//! a [`HandleError::Software`], makes it `failed`.

use std::time::Duration;

use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::registry::Registry;
use crate::core::step::StepReport;

pub(crate) mod analysis;
mod file_contains;
pub(crate) mod intake;
mod intake_has_metric;
mod log;
mod process_stable_for;
mod send;

/// How often a step that waits for something looks again.
const POLL: Duration = Duration::from_millis(100);

/// Registers every step.
pub(crate) fn register(registry: &mut Registry) {
    registry.add_step(Box::new(file_contains::FileContainsFactory));
    registry.add_step(Box::new(intake_has_metric::IntakeHasMetricFactory));
    registry.add_step(Box::new(log::LogContainsFactory));
    registry.add_step(Box::new(log::LogNotContainsFactory));
    registry.add_step(Box::new(process_stable_for::ProcessStableForFactory));
    registry.add_step(Box::new(send::SendFactory));
}

/// The report of a step stopped by `error`: `errored` when the mechanism failed, `failed` when the software answered
/// badly.
fn stopped(error: HandleError) -> StepReport {
    match error {
        HandleError::Harness(message) => StepReport {
            status: StepStatus::Errored,
            message,
        },
        HandleError::Software(message) => StepReport {
            status: StepStatus::Failed,
            message,
        },
    }
}

/// The processes a step watches, for a message: the one named, or every process of the participant.
fn described(on: &str, process: Option<&str>) -> String {
    match process {
        Some(process) => format!("process '{}' of '{}'", process, on),
        None => format!("every process of '{}'", on),
    }
}

/// One participant, `app`, for the steps' failing-check tests: a process `main` observed through its container, a
/// supervised process `job` with an exit record, a UDP endpoint `statsd`, and an operation `dump` that answers with
/// what [`DUMP_FILE`] holds.
#[cfg(test)]
mod scripted {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio::io::{AsyncWrite, AsyncWriteExt as _};
    use tokio::time::Instant;

    use crate::core::backend::{Exec, Realized};
    use crate::core::case::{self, CASE_FILE};
    use crate::core::handle::{Handle, HandleError};
    use crate::core::observe::Observation;
    use crate::core::participant::Os;
    use crate::core::plan;
    use crate::core::reference::Values;
    use crate::core::registry::Registry;
    use crate::core::step::{Operation, StepContext, StepFactory, StepFields, StepReport, Uses};

    /// The file whose contents `app`'s `dump` answers with.
    pub(super) const DUMP_FILE: &str = "/scripted/dump.json";

    /// What the participant's files read as, by path, and how every command run in it exits, with what it writes to
    /// standard error. A path not here holds no file; with no exit, a command cannot run.
    #[derive(Debug, Default)]
    pub(super) struct Scripted {
        pub(super) files: BTreeMap<&'static str, Result<Vec<u8>, HandleError>>,
        pub(super) exit: Option<(i64, &'static str)>,
    }

    #[async_trait]
    impl Realized for Scripted {
        async fn exec(
            &self, _exec: Exec, _stdout: &mut (dyn AsyncWrite + Send + Unpin),
            stderr: &mut (dyn AsyncWrite + Send + Unpin), _deadline: Instant,
        ) -> Result<i64, HandleError> {
            let (code, written) = self
                .exit
                .ok_or_else(|| HandleError::Harness("not scripted".to_string()))?;
            stderr.write_all(written.as_bytes()).await.unwrap();
            Ok(code)
        }

        async fn has_exited(&self) -> bool {
            false
        }

        async fn read_file(&self, path: &str, _deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
            self.files.get(path).cloned().transpose()
        }

        fn host_port(&self, _endpoint: &str) -> Option<u16> {
            None
        }
    }

    /// `app`'s `dump`: the JSON in [`DUMP_FILE`], or the error reading it.
    #[derive(Debug)]
    struct Dump;

    #[async_trait]
    impl Operation for Dump {
        fn name(&self) -> &'static str {
            "dump"
        }

        fn uses(&self) -> &'static [Uses] {
            &[]
        }

        async fn run(&self, on: &Handle<'_>, _args: Value, limit: Duration) -> Result<Value, HandleError> {
            let answer = on.read_file(DUMP_FILE, limit).await?.unwrap_or_default();
            serde_json::from_slice(&answer).map_err(|e| HandleError::Software(e.to_string()))
        }
    }

    /// Builds the step `factory` makes from `fields`, written as YAML, and runs it against `app`, scripted by
    /// `realized` and `observation`, within the step's own budget.
    pub(super) async fn run(
        factory: &dyn StepFactory, fields: &str, realized: &Scripted, observation: &Observation,
    ) -> StepReport {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("scripted");
        std::fs::create_dir(&dir).unwrap();
        let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: alpine:3.20
    processes:
      - {name: main, exit: container, output: container}
      - {name: job, exit: {record: job}, output: {file: job.log}}
    endpoints:
      statsd: {udp: 8125}
procedure: []
"#;
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let registry = Registry::default();
        let case = case::load(&dir, &registry).unwrap();
        let mut plan = plan::plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap();
        // A participant with no participant type offers no operation, so the test gives `app` one.
        plan.participants
            .get_mut("app")
            .unwrap()
            .operations
            .insert("dump", &Dump);
        let values = Values::of_plan(&plan.participants);
        let handle = Handle::new("app", &plan.participants["app"], realized, &values, observation);
        let handles = BTreeMap::from([("app".to_string(), handle)]);

        let fields = StepFields(serde_saphyr::from_str(fields).unwrap());
        let step = factory.build(&fields).unwrap();
        let cx = StepContext::new(&handles, &values, Instant::now() + step.budget());
        step.run(&cx).await
    }

    /// The observation of `app` while it runs, before it writes anything.
    pub(super) fn observation() -> Observation {
        Observation::new(["main", "job"])
    }
}

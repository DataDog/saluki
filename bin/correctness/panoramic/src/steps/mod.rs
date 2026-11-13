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

mod file_contains;
pub(crate) mod intake;
mod log;
mod process_stable_for;

/// How often a step that waits for something looks again.
const POLL: Duration = Duration::from_millis(100);

/// Registers every step.
pub(crate) fn register(registry: &mut Registry) {
    registry.add_step(Box::new(file_contains::FileContainsFactory));
    registry.add_step(Box::new(log::LogContainsFactory));
    registry.add_step(Box::new(log::LogNotContainsFactory));
    registry.add_step(Box::new(process_stable_for::ProcessStableForFactory));
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

/// One participant, `app`, for the steps' failing-check tests: a process `main` observed through its container, and a
/// supervised process `job` with an exit record.
#[cfg(test)]
mod scripted {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use tokio::io::AsyncWrite;
    use tokio::time::Instant;

    use crate::core::backend::{Exec, Realized};
    use crate::core::case::{self, CASE_FILE};
    use crate::core::handle::{Handle, HandleError};
    use crate::core::observe::Observation;
    use crate::core::participant::Os;
    use crate::core::plan;
    use crate::core::reference::Values;
    use crate::core::registry::Registry;
    use crate::core::step::{StepContext, StepFactory, StepFields, StepReport};

    /// What the participant's files read as, by path. A path not here holds no file.
    #[derive(Debug, Default)]
    pub(super) struct Scripted {
        pub(super) files: BTreeMap<&'static str, Result<Vec<u8>, HandleError>>,
    }

    #[async_trait]
    impl Realized for Scripted {
        async fn exec(
            &self, _exec: Exec, _stdout: &mut (dyn AsyncWrite + Send + Unpin),
            _stderr: &mut (dyn AsyncWrite + Send + Unpin), _deadline: Instant,
        ) -> Result<i64, HandleError> {
            Err(HandleError::Harness("not scripted".to_string()))
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
procedure: []
"#;
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let registry = Registry::default();
        let case = case::load(&dir, &registry).unwrap();
        let plan = plan::plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap();
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

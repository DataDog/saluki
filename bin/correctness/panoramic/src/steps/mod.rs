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
mod compare;
mod config_key_equals;
mod config_set;
mod dogstatsd_replay;
mod exec;
mod exits_with;
mod file_contains;
mod http_check;
mod http_request;
pub(crate) mod intake;
mod intake_has_metric;
mod log;
mod port_listening;
mod process_stable_for;
mod send;

/// How often a step that waits for something looks again.
const POLL: Duration = Duration::from_millis(100);

/// How long one attempt of a step that probes or requests an endpoint may take.
const ATTEMPT: Duration = Duration::from_secs(5);

/// How long past the step's deadline the participant is given to end a command, so that a command that outlives the
/// step is reported by the step and not as a problem of the mechanism.
const EXEC_SLACK: Duration = Duration::from_secs(5);

/// Registers every step.
pub(crate) fn register(registry: &mut Registry) {
    registry.add_step(Box::new(compare::CompareFactory));
    registry.add_step(Box::new(config_key_equals::ConfigKeyEqualsFactory));
    registry.add_step(Box::new(config_set::ConfigSetFactory));
    registry.add_step(Box::new(dogstatsd_replay::DogstatsdReplayFactory));
    registry.add_step(Box::new(exec::ExecFactory));
    registry.add_step(Box::new(exits_with::ExitsWithFactory));
    registry.add_step(Box::new(file_contains::FileContainsFactory));
    registry.add_step(Box::new(http_check::HttpCheckFactory));
    registry.add_step(Box::new(http_request::HttpRequestFactory));
    registry.add_step(Box::new(intake::IntakeFromFactory));
    registry.add_step(Box::new(intake_has_metric::IntakeHasMetricFactory));
    registry.add_step(Box::new(log::LogContainsFactory));
    registry.add_step(Box::new(log::LogNotContainsFactory));
    registry.add_step(Box::new(port_listening::PortListeningFactory));
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
/// supervised process `job` with an exit record, a UDP endpoint `statsd`, and operations that answer with what a file
/// holds: `dump` with [`DUMP_FILE`], `config_dump` with [`CONFIG_FILE`], `config_set` with [`CONFIG_SET_FILE`], and
/// `dogstatsd_capture`, `dogstatsd_replay` and `dogstatsd_stats` with [`CAPTURE_FILE`], [`REPLAY_FILE`] and
/// [`STATS_FILE`]. A second participant, `peer`, offers the same operations, for steps that read two.
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
    use crate::core::observe::{Kept, Observation};
    use crate::core::participant::Os;
    use crate::core::plan;
    use crate::core::reference::Values;
    use crate::core::registry::Registry;
    use crate::core::step::{Operation, StepContext, StepFactory, StepFields, StepReport, Uses};

    /// The file whose contents `app`'s `dump` answers with.
    pub(super) const DUMP_FILE: &str = "/scripted/dump.json";

    /// The file whose contents `app`'s `config_dump` answers with.
    pub(super) const CONFIG_FILE: &str = "/scripted/config.json";

    /// The file whose contents `app`'s `config_set` answers with.
    pub(super) const CONFIG_SET_FILE: &str = "/scripted/config_set.json";

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

    /// The file whose contents `app`'s `dogstatsd_capture` answers with.
    pub(super) const CAPTURE_FILE: &str = "/scripted/capture.json";

    /// The file whose contents `app`'s `dogstatsd_replay` answers with.
    pub(super) const REPLAY_FILE: &str = "/scripted/replay.json";

    /// The file whose contents `app`'s `dogstatsd_stats` answers with.
    pub(super) const STATS_FILE: &str = "/scripted/stats.json";

    /// An operation that answers with the JSON in `file`, or the error reading it.
    #[derive(Debug)]
    struct Answers {
        name: &'static str,
        file: &'static str,
    }

    #[async_trait]
    impl Operation for Answers {
        fn name(&self) -> &'static str {
            self.name
        }

        fn uses(&self) -> &'static [Uses] {
            &[]
        }

        async fn run(&self, on: &Handle<'_>, _args: Value, limit: Duration) -> Result<Value, HandleError> {
            let answer = on.read_file(self.file, limit).await?.unwrap_or_default();
            serde_json::from_slice(&answer).map_err(|e| HandleError::Software(e.to_string()))
        }
    }

    /// Builds the step `factory` makes from `fields`, written as YAML, and runs it against `app`, scripted by
    /// `realized` and `observation`, within the step's own budget.
    pub(super) async fn run(
        factory: &dyn StepFactory, fields: &str, realized: &Scripted, observation: &Observation,
    ) -> StepReport {
        run_with_peer(factory, fields, realized, &Scripted::default(), observation).await
    }

    /// As [`run`], with `peer` scripted by `peer`, observed as `app` is.
    pub(super) async fn run_with_peer(
        factory: &dyn StepFactory, fields: &str, realized: &Scripted, peer: &Scripted, observation: &Observation,
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
      statsd: {protocol: udp, port: 8125}
      api: {protocol: tcp, port: 8080}
  peer:
    image: alpine:3.20
procedure: []
"#;
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let registry = Registry::default();
        let case = case::load(&dir, &registry).unwrap();
        let mut plan = plan::plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap();
        // A participant with no participant type offers no operation, so the test gives each its operations.
        for planned in plan.participants.values_mut() {
            for operation in [
                &Answers {
                    name: "dump",
                    file: DUMP_FILE,
                },
                &Answers {
                    name: "config_dump",
                    file: CONFIG_FILE,
                },
                &Answers {
                    name: "config_set",
                    file: CONFIG_SET_FILE,
                },
                &Answers {
                    name: "dogstatsd_capture",
                    file: CAPTURE_FILE,
                },
                &Answers {
                    name: "dogstatsd_replay",
                    file: REPLAY_FILE,
                },
                &Answers {
                    name: "dogstatsd_stats",
                    file: STATS_FILE,
                },
            ] {
                planned.operations.insert(operation.name, operation);
            }
        }
        let values = Values::of_plan(&plan.participants);
        let handles = BTreeMap::from([
            (
                "app".to_string(),
                Handle::new("app", &plan.participants["app"], realized, &values, observation),
            ),
            (
                "peer".to_string(),
                Handle::new("peer", &plan.participants["peer"], peer, &values, observation),
            ),
        ]);

        let fields = StepFields::of(serde_saphyr::from_str(fields).unwrap());
        let step = factory.build(&fields).unwrap();
        let kept = Kept::new(&suite.path().join("logs"));
        // A step whose budget is what remains of the case gets the case's.
        let budget = step.budget().min(Duration::from_secs(60));
        let cx = StepContext::new(&handles, &values, &kept, Instant::now() + budget);
        step.run(&cx).await
    }

    /// The observation of `app` while it runs, before it writes anything.
    pub(super) fn observation() -> Observation {
        Observation::new(["main", "job"])
    }
}

//! The handle a step holds: what a step or an operation acts on a started participant through.
//!
//! Every method works the same on every realization and says whose problem a failure is, with [`HandleError`]. Named
//! commands and operations are the plan's data, so nothing looks up a participant type. An endpoint is reached by the
//! route the plan's rule gives for what is done, the endpoint's protocol and the participant's realization: through the
//! courier inside the participant, or from the runner through a published port, with the runner's own HTTP client.
//! A process's exit and output come from what the lifecycle observes of the participant, through the process's own
//! channels.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use saluki_error::GenericError;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::{timeout_at, Instant};

use super::backend::{Exec, Realized};
use super::courier::{Courier, HttpResponse};
use super::observe::{first_exit, ExitStatus, Observation, ProcessExit};
use super::plan::PlannedParticipant;
use super::reference::Values;
use super::spec::{ExitChannel, ProcessSpec};
use super::step::EndpointOperation;
use super::wiring::{reach, PlannedEndpoint, Reach};

/// How often [`Handle::wait_for_exit`] reads an exit record again, unless the participant's exit wakes it first.
const EXIT_POLL: Duration = Duration::from_millis(250);

/// How long [`Handle::wait_for_exit`] gives each read of an exit record.
pub(crate) const RECORD_READ_TIME: Duration = Duration::from_secs(5);

/// Why a handle operation failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HandleError {
    /// The mechanism failed. The step is `errored`.
    Harness(String),
    /// The mechanism worked and the software answered badly. The step is `failed`.
    Software(String),
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Harness(message) | Self::Software(message) => f.write_str(message),
        }
    }
}

/// How a command run in a participant ended.
// TODO: remove the dead_code allowance once a step runs a command.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Exited {
    pub(crate) code: i64,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// What a step acts on one started participant through.
pub(crate) struct Handle<'a> {
    name: &'a str,
    planned: &'a PlannedParticipant,
    realized: &'a dyn Realized,
    /// What fills the references of named commands when they run.
    // TODO: remove the dead_code allowance once a step runs a command.
    #[allow(dead_code)]
    values: &'a Values,
    observation: &'a Observation,
}

impl<'a> Handle<'a> {
    pub(crate) fn new(
        name: &'a str, planned: &'a PlannedParticipant, realized: &'a dyn Realized, values: &'a Values,
        observation: &'a Observation,
    ) -> Self {
        Self {
            name,
            planned,
            realized,
            values,
            observation,
        }
    }

    /// The names of the participant's processes.
    pub(crate) fn processes(&self) -> Vec<&'a str> {
        self.planned.processes.iter().map(ProcessSpec::name).collect()
    }

    /// The output `process` has written so far, line by line, which grows as more is captured.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such process.
    pub(crate) fn output(&self, process: &str) -> Result<watch::Receiver<Vec<String>>, HandleError> {
        self.observation.output(process).ok_or_else(|| self.no_process(process))
    }

    /// How `process` exited, or that it is still running, or why that cannot be determined.
    ///
    /// The only process of a container exits when the container does. A supervised process has exited once its exit
    /// record holds a complete line, read within `limit`. Its status is undetermined when the record cannot be read or
    /// does not parse, and when the participant has exited and the record holds no exit.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such process.
    pub(crate) async fn exit(&self, process: &str, limit: Duration) -> Result<ProcessExit, HandleError> {
        let spec = self.process(process)?;
        // Taken before the record is read: an empty record read after the participant exited is final.
        let reported = self.observation.exit();
        let ExitChannel::Record { file } = spec.exit() else {
            return Ok(match reported {
                None => ProcessExit::Running,
                Some(Ok(code)) => ProcessExit::Exited(ExitStatus { code, signal: None }),
                Some(Err(why)) => ProcessExit::Undetermined(why),
            });
        };
        let facts = self.planned.realization.enclosure.facts();
        let Some(dir) = facts.exit_dir(self.name) else {
            return Ok(ProcessExit::Undetermined(format!(
                "a {} has no exit directory",
                self.planned.realization.enclosure
            )));
        };
        let path = format!("{}{}{}", dir, facts.separator, file);
        let record = match self.realized.read_file(&path, Instant::now() + limit).await {
            Ok(record) => record.unwrap_or_default(),
            Err(e) => {
                return Ok(ProcessExit::Undetermined(format!(
                    "exit record {} cannot be read: {}",
                    path, e
                )))
            }
        };
        Ok(match (first_exit(&record), reported) {
            (Ok(Some(status)), _) => ProcessExit::Exited(status),
            (Ok(None), None) => ProcessExit::Running,
            (Ok(None), Some(_)) => ProcessExit::Undetermined(format!(
                "the participant has exited and exit record {} holds no exit",
                path
            )),
            (Err(e), _) => ProcessExit::Undetermined(format!("exit record {}: {}", path, e)),
        })
    }

    /// Waits up to `limit` for `process` to exit, and returns what [`Self::exit`] says then: running, if it has not
    /// exited by the end of `limit`. Each read of an exit record has a few seconds of its own, so the wait may end up
    /// to that much after `limit`.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such process.
    pub(crate) async fn wait_for_exit(&self, process: &str, limit: Duration) -> Result<ProcessExit, HandleError> {
        let deadline = Instant::now() + limit;
        loop {
            let exit = self.exit(process, RECORD_READ_TIME).await?;
            if exit != ProcessExit::Running || Instant::now() >= deadline {
                return Ok(exit);
            }
            // A record may gain its line at any time; the participant's exit ends the wait early.
            let next = (Instant::now() + EXIT_POLL).min(deadline);
            let _ = timeout_at(next, self.observation.wait_for_exit()).await;
        }
    }

    /// Reads the file at `path` in the participant, or returns `None` if nothing is there.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the file cannot be read within `limit`.
    pub(crate) async fn read_file(&self, path: &str, limit: Duration) -> Result<Option<Vec<u8>>, HandleError> {
        self.realized.read_file(path, Instant::now() + limit).await
    }

    /// Runs `argv` in the participant with `env` added. A nonzero exit code is a result.
    ///
    /// # Errors
    ///
    /// Returns a software error if the participant has exited, and a harness error if the command cannot be run
    /// otherwise, or does not exit within `limit`.
    // TODO: remove the dead_code allowance once a step runs a command.
    #[allow(dead_code)]
    pub(crate) async fn exec(
        &self, argv: Vec<String>, env: BTreeMap<String, String>, limit: Duration,
    ) -> Result<Exited, HandleError> {
        let exec = Exec { argv, env, stdin: None };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = self
            .realized
            .exec(exec, &mut stdout, &mut stderr, Instant::now() + limit)
            .await?;
        Ok(Exited { code, stdout, stderr })
    }

    /// Runs the participant's command `name` with `args` appended and `env` added. A nonzero exit code is a result.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such command or a reference in it has no value yet, and the
    /// errors of [`Self::exec`].
    // TODO: remove the dead_code allowance once a step runs a command.
    #[allow(dead_code)]
    pub(crate) async fn command(
        &self, name: &str, args: &[String], env: &BTreeMap<String, String>, limit: Duration,
    ) -> Result<Exited, HandleError> {
        let command =
            self.planned.commands.get(name).ok_or_else(|| {
                HandleError::Harness(format!("participant '{}' has no command '{}'", self.name, name))
            })?;
        if let Some(program) = &command.program {
            // TODO: run a host program's command with the host process backend.
            return Err(HandleError::Harness(format!(
                "command '{}' runs program '{}' of host processes, which are not built",
                name, program
            )));
        }
        let fill = |template| {
            self.values
                .fill(self.name, template)
                .map_err(|e| HandleError::Harness(format!("participant '{}' command '{}': {}", self.name, name, e)))
        };
        let mut argv = command.argv.iter().map(fill).collect::<Result<Vec<_>, _>>()?;
        argv.extend(args.iter().cloned());
        let mut filled = BTreeMap::new();
        for (key, value) in &command.env {
            filled.insert(key.clone(), fill(value)?);
        }
        filled.extend(env.iter().map(|(key, value)| (key.clone(), value.clone())));
        self.exec(argv, filled, limit).await
    }

    /// Runs the participant's operation `name` with `args`.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant offers no such operation, or the operation's own error.
    // TODO: remove the dead_code allowance once a step calls an operation.
    #[allow(dead_code)]
    pub(crate) async fn op(&self, name: &str, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let operation = self.planned.operations.get(name).ok_or_else(|| {
            HandleError::Harness(format!("participant '{}' offers no operation '{}'", self.name, name))
        })?;
        operation.run(self, args, limit).await
    }

    /// Checks that the endpoint accepts a connection, or that its socket exists.
    ///
    /// # Errors
    ///
    /// Returns a software error if nothing accepts within `limit`, and a harness error if the endpoint cannot be
    /// probed.
    pub(crate) async fn probe(&self, endpoint: &str, limit: Duration) -> Result<(), HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Probe)?;
        match route {
            Reach::CourierInServer => {
                let address = planned.courier_address(self.name, EndpointOperation::Probe);
                self.courier()?.probe(&address, limit).await
            }
            route => Err(not_built(route)),
        }
    }

    /// Opens one connection to the endpoint, or sends one datagram, and writes `payload` as given.
    ///
    /// # Errors
    ///
    /// Returns a software error if the endpoint refuses the bytes within `limit`, and a harness error if nothing can be
    /// sent to it.
    // TODO: remove the dead_code allowance once a step sends to an endpoint.
    #[allow(dead_code)]
    pub(crate) async fn send(&self, endpoint: &str, payload: Bytes, limit: Duration) -> Result<(), HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Send)?;
        match route {
            Reach::CourierInServer => {
                let address = planned.courier_address(self.name, EndpointOperation::Send);
                self.courier()?.send(&address, payload, limit).await
            }
            route => Err(not_built(route)),
        }
    }

    /// Makes one HTTP `GET` request for `path` at the endpoint. Any status is a response.
    ///
    /// # Errors
    ///
    /// Returns a software error if no response arrives within `limit`, and a harness error if the endpoint cannot be
    /// requested.
    pub(crate) async fn http_get(
        &self, endpoint: &str, path: &str, limit: Duration,
    ) -> Result<HttpResponse, HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Request)?;
        if !path.starts_with('/') {
            return Err(HandleError::Harness(format!(
                "the path '{}' does not start with '/'",
                path
            )));
        }
        match route {
            Reach::PublishedPort => self.published_get(endpoint, path, limit).await,
            Reach::CourierInServer => {
                let address = planned.courier_address(self.name, EndpointOperation::Request);
                self.courier()?.http_get(&address, path, limit).await
            }
            route => Err(not_built(route)),
        }
    }

    /// The endpoint `name`, and how the runner reaches it to do `operation`.
    fn route(&self, name: &str, operation: EndpointOperation) -> Result<(&'a PlannedEndpoint, Reach), HandleError> {
        let endpoint =
            self.planned.endpoints.get(name).ok_or_else(|| {
                HandleError::Harness(format!("participant '{}' has no endpoint '{}'", self.name, name))
            })?;
        if let Some(unwired) = &endpoint.unwired {
            return Err(HandleError::Harness(format!(
                "endpoint '{}' of participant '{}' is not wired: {}",
                name, self.name, unwired.why
            )));
        }
        let route = reach(
            operation,
            endpoint.protocol,
            endpoint.tls,
            self.planned.realization.enclosure,
        )
        .map_err(|e| HandleError::Harness(format!("endpoint '{}' of participant '{}': {}", name, self.name, e)))?;
        Ok((endpoint, route))
    }

    fn process(&self, process: &str) -> Result<&'a ProcessSpec, HandleError> {
        self.planned
            .processes
            .iter()
            .find(|candidate| candidate.name() == process)
            .ok_or_else(|| self.no_process(process))
    }

    fn no_process(&self, process: &str) -> HandleError {
        HandleError::Harness(format!("participant '{}' has no process '{}'", self.name, process))
    }

    fn courier(&self) -> Result<Courier<'a>, HandleError> {
        let enclosure = self.planned.realization.enclosure;
        let path = enclosure
            .facts()
            .courier
            .ok_or_else(|| HandleError::Harness(format!("a {} has no courier", enclosure)))?;
        Ok(Courier::new(self.realized, path))
    }

    /// Requests `path` from the runner, through the port published for `endpoint` on the runner's loopback address. A
    /// redirect is a response, as through the courier.
    async fn published_get(&self, endpoint: &str, path: &str, limit: Duration) -> Result<HttpResponse, HandleError> {
        let port = self.realized.host_port(endpoint).ok_or_else(|| {
            HandleError::Software(format!(
                "endpoint '{}' of participant '{}' has no published port: the participant had exited by the time its \
                 ports were read",
                endpoint, self.name
            ))
        })?;
        // One client for the process, so requests share its connection pool.
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let client = match CLIENT.get() {
            Some(client) => client,
            None => {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(harness)?;
                CLIENT.get_or_init(|| client)
            }
        };
        let failed = |e: reqwest::Error| {
            if e.is_builder() {
                harness(e)
            } else {
                HandleError::Software(format!("{:#}", GenericError::from(e)))
            }
        };
        let response = client
            .get(format!("http://127.0.0.1:{}{}", port, path))
            .timeout(limit)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(failed)?.to_vec();
        Ok(HttpResponse { status, body })
    }
}

fn harness(error: impl Into<GenericError>) -> HandleError {
    HandleError::Harness(format!("{:#}", error.into()))
}

fn not_built(route: Reach) -> HandleError {
    // TODO: reach pods through forwarded ports with the pod backend, and host processes from the runner with the host
    // process backend.
    let through = match route {
        Reach::CourierInServer => "the courier in the participant",
        Reach::PublishedPort => "a published port",
        Reach::ForwardedPort => "a forwarded port",
        Reach::InRunner => "the runner",
    };
    HandleError::Harness(format!(
        "reaching an endpoint through {} is not built for this",
        through
    ))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio::io::AsyncWrite;

    use super::*;
    use crate::core::case::{self, CASE_FILE};
    use crate::core::participant::Os;
    use crate::core::plan::{plan, CasePlan};
    use crate::core::registry::Registry;

    /// A participant whose exit record reads as scripted.
    #[derive(Debug)]
    struct Scripted {
        record: Result<Option<Vec<u8>>, HandleError>,
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
            assert_eq!(path, "/airlock/app/exit/job");
            self.record.clone()
        }

        fn host_port(&self, _endpoint: &str) -> Option<u16> {
            None
        }
    }

    fn planned() -> CasePlan {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("exits");
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
        plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap()
    }

    #[tokio::test]
    async fn an_exit_is_running_exited_or_undetermined_and_never_invented() {
        let plan = planned();
        let values = Values::of_plan(&plan.participants);
        let exited = |code, signal| Ok(ProcessExit::Exited(ExitStatus { code, signal }));
        let running = Ok(ProcessExit::Running);

        // What the realization reports of the participant, the exit record as read, the process, and its exit: the
        // state, or a part of why it is undetermined.
        type Case<'a> = (
            Option<Result<i64, &'a str>>,
            Result<Option<&'a [u8]>, &'a str>,
            &'a str,
            Result<ProcessExit, &'a str>,
        );
        let cases: Vec<Case<'_>> = vec![
            (None, Ok(None), "main", running.clone()),
            (Some(Ok(137)), Ok(None), "main", exited(137, None)),
            (Some(Err("Docker lost it")), Ok(None), "main", Err("Docker lost it")),
            (None, Ok(None), "job", running.clone()),
            (None, Ok(Some(b"")), "job", running.clone()),
            (None, Ok(Some(b"3 0 1794511601.5\n")), "job", exited(3, None)),
            (
                Some(Ok(0)),
                Ok(Some(b"256 9 1794511601.5\n")),
                "job",
                exited(256, Some(9)),
            ),
            (Some(Ok(0)), Ok(None), "job", Err("has exited and exit record")),
            (None, Ok(Some(b"3 0\n")), "job", Err("is not '<code>")),
            (None, Err("copy failed"), "job", Err("cannot be read: copy failed")),
        ];
        for (reported, record, process, expected) in cases {
            let observation = Observation::new(["main", "job"]);
            if let Some(reported) = reported {
                observation.exited(reported.map_err(str::to_string));
            }
            let realized = Scripted {
                record: record
                    .map(|record| record.map(<[u8]>::to_vec))
                    .map_err(|e| HandleError::Harness(e.to_string())),
            };
            let handle = Handle::new("app", &plan.participants["app"], &realized, &values, &observation);
            let exit = handle.exit(process, Duration::from_secs(1)).await.unwrap();
            let case = format!("{reported:?}, {record:?}, {process}: {exit:?}");
            match (&exit, expected) {
                (ProcessExit::Undetermined(why), Err(part)) => assert!(why.contains(part), "{case}"),
                (exit, Ok(expected)) => assert_eq!(*exit, expected, "{case}"),
                _ => panic!("{case}"),
            }
        }
    }
}

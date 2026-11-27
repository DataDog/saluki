//! The handle a step holds: what a step or an operation acts on a started participant through.
//!
//! Every method works the same on every realization and says whose problem a failure is, with [`HandleError`]. Named
//! commands and operations are the plan's data, so nothing looks up a participant type. An endpoint is reached by the
//! route the plan's rule gives for what is done, the endpoint's protocol and the participant's realization: through the
//! courier inside the participant, from the runner through a published port, with the runner's own HTTP client, or, for
//! host processes, from the runner on the endpoint's own port.
//! A process's exit and output come from what the lifecycle observes of the participant, through the process's own
//! channels.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::HeaderMap;
use reqwest::Method;
use saluki_error::GenericError;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::{timeout_at, Instant};

use super::backend::{Exec, Realized};
use super::courier::{Courier, HttpResponse};
use super::observe::{first_exit, ExitStatus, Observation, ProcessExit, CONTAINER_OUTPUT};
use super::plan::{Launch, PlannedParticipant, PlannedProgram};
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Exited {
    pub(crate) code: i64,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// An HTTP request as [`Handle::http_request`] sends it.
#[derive(Clone, Debug)]
pub(crate) struct HttpRequestSpec {
    pub(crate) method: Method,
    /// The path and query, which start with `/`.
    pub(crate) path: String,
    /// The headers sent in addition to the client's own `Host`, `Accept` and framing headers.
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

/// A reply to an HTTP request, as it arrived: the body is not decoded.
#[derive(Clone, Debug)]
pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

/// What a step acts on one started participant through.
pub(crate) struct Handle<'a> {
    name: &'a str,
    planned: &'a PlannedParticipant,
    realized: &'a dyn Realized,
    /// What fills the references of named commands when they run.
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

    /// The processes a step watches: `process`, or every process of the participant when `process` is `None`.
    ///
    /// # Errors
    ///
    /// Returns a harness error if `process` is `None` and the participant declares no processes, so a step that
    /// watches every process would watch none and pass on nothing.
    pub(crate) fn watched(&self, process: Option<&'a str>) -> Result<Vec<&'a str>, HandleError> {
        if let Some(process) = process {
            return Ok(vec![process]);
        }
        let processes: Vec<&'a str> = self.planned.processes.iter().map(ProcessSpec::name).collect();
        if processes.is_empty() {
            return Err(HandleError::Harness(format!(
                "participant '{}' has no processes to observe",
                self.name
            )));
        }
        Ok(processes)
    }

    /// The outputs a log step searches: those of [`Self::watched`], and the container log too when `process` is `None`
    /// and the participant's container log is observed.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::watched`].
    pub(crate) fn logged(&self, process: Option<&'a str>) -> Result<Vec<&'a str>, HandleError> {
        let mut logged = self.watched(process)?;
        if process.is_none() && self.observation.output(CONTAINER_OUTPUT).is_some() {
            logged.push(CONTAINER_OUTPUT);
        }
        Ok(logged)
    }

    /// The output `process` has written so far, line by line, which grows as more is captured.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such process.
    pub(crate) fn output(&self, process: &str) -> Result<watch::Receiver<Vec<String>>, HandleError> {
        self.observation.output(process).ok_or_else(|| self.no_process(process))
    }

    /// What `process` has written since its last newline, one entry for each of its streams that has some. `process`
    /// is one of the participant's, from [`Handle::output`] or [`Handle::watched`].
    pub(crate) fn output_unterminated(&self, process: &str) -> Vec<String> {
        self.observation.unterminated(process)
    }

    /// Checks that the output of `process` is whole so far: its capture has not broken.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the capture broke before the output ended.
    pub(crate) fn output_whole(&self, process: &str) -> Result<(), HandleError> {
        match self.observation.capture_failure(process) {
            None => Ok(()),
            Some(why) => Err(HandleError::Harness(format!(
                "the output of process '{}' of '{}' stopped being captured: {}",
                process, self.name, why
            ))),
        }
    }

    /// How `process` exited, or that it is still running, or why that cannot be determined.
    ///
    /// The only process of a container exits when the container does, and a process the realization reports apart, such
    /// as a host program, exits as it says. A supervised process has exited once its exit record holds a complete line,
    /// read within `limit`. Its status is undetermined when the record cannot be read or does not parse, and when the
    /// participant has exited and the record holds no exit.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the participant has no such process.
    pub(crate) async fn exit(&self, process: &str, limit: Duration) -> Result<ProcessExit, HandleError> {
        let spec = self.process(process)?;
        // Taken before the record is read: an empty record read after the participant exited is final.
        let reported = self.observation.exit();
        let ExitChannel::Record { file } = spec.exit() else {
            if let Some(exit) = self.realized.reported_exit(process) {
                return Ok(exit);
            }
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
    pub(crate) async fn command(
        &self, name: &str, args: &[String], env: &BTreeMap<String, String>, limit: Duration,
    ) -> Result<Exited, HandleError> {
        let command =
            self.planned.commands.get(name).ok_or_else(|| {
                HandleError::Harness(format!("participant '{}' has no command '{}'", self.name, name))
            })?;
        let fill = |template| {
            self.values
                .fill(self.name, template)
                .map_err(|e| HandleError::Harness(format!("participant '{}' command '{}': {}", self.name, name, e)))
        };
        let mut argv = Vec::new();
        let mut filled = BTreeMap::new();
        if let Some(program) = &command.program {
            let program = self.program(program)?;
            argv.push(program.path.used().to_string());
            for (key, value) in &program.env {
                filled.insert(key.clone(), fill(value)?);
            }
        }
        for template in &command.argv {
            argv.push(fill(template)?);
        }
        argv.extend(args.iter().cloned());
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
            Reach::InRunner => {
                let address = self.in_runner(planned, EndpointOperation::Probe)?;
                panoramic_courier::probe(&address, limit)
                    .await
                    .map_err(HandleError::from)
            }
            route => Err(no_route(route)),
        }
    }

    /// Opens one connection to the endpoint, or sends one datagram, and writes `payload` as given.
    ///
    /// # Errors
    ///
    /// Returns a software error if the endpoint refuses the bytes within `limit`, and a harness error if nothing can be
    /// sent to it.
    pub(crate) async fn send(&self, endpoint: &str, payload: Bytes, limit: Duration) -> Result<(), HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Send)?;
        match route {
            Reach::CourierInServer => {
                let address = planned.courier_address(self.name, EndpointOperation::Send);
                self.courier()?.send(&address, payload, limit).await
            }
            Reach::InRunner => {
                let address = self.in_runner(planned, EndpointOperation::Send)?;
                panoramic_courier::send(&address, &payload, limit)
                    .await
                    .map_err(HandleError::from)
            }
            route => Err(no_route(route)),
        }
    }

    /// Makes one HTTP `GET` request for `path` at the endpoint, over TLS if the endpoint serves it, and without
    /// checking the server's certificate if `skip_verify` is set. Any status is a response.
    ///
    /// With `read_body` unset, a request through a published or forwarded port returns once the response head
    /// arrives, with an empty body, so a body that is slow or never ends does not matter. The courier always reads
    /// the whole body.
    ///
    /// # Errors
    ///
    /// Returns a software error if no response arrives within `limit`, and a harness error if the endpoint cannot be
    /// requested.
    pub(crate) async fn http_get(
        &self, endpoint: &str, path: &str, skip_verify: bool, read_body: bool, limit: Duration,
    ) -> Result<HttpResponse, HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Request)?;
        if !path.starts_with('/') {
            return Err(HandleError::Harness(format!(
                "the path '{}' does not start with '/'",
                path
            )));
        }
        match route {
            Reach::PublishedPort | Reach::ForwardedPort | Reach::InRunner => {
                self.published_get(endpoint, planned.tls, path, skip_verify, read_body, limit)
                    .await
            }
            // The plan refuses a request to a TLS endpoint through the courier, which does not speak TLS.
            Reach::CourierInServer if planned.tls => Err(HandleError::Harness(format!(
                "endpoint '{}' of participant '{}' serves TLS, which the courier does not speak",
                endpoint, self.name
            ))),
            Reach::CourierInServer => {
                let address = planned.courier_address(self.name, EndpointOperation::Request);
                self.courier()?.http_get(&address, path, limit).await
            }
        }
    }

    /// Sends `request` to the endpoint once, over TLS if the endpoint serves it, and reads the whole reply within
    /// `limit`. Any status is a reply.
    ///
    /// The client adds no `Content-Type`, `Content-Encoding` or `Accept-Encoding` header, and it does not decode the
    /// reply body. It follows no redirect and keeps no connection open, so each request goes on a new connection and is
    /// never sent again.
    ///
    /// # Errors
    ///
    /// Returns a software error if no whole reply arrives within `limit`, and a harness error if the endpoint cannot be
    /// requested.
    pub(crate) async fn http_request(
        &self, endpoint: &str, request: &HttpRequestSpec, limit: Duration,
    ) -> Result<HttpReply, HandleError> {
        let (planned, route) = self.route(endpoint, EndpointOperation::Exchange)?;
        if !request.path.starts_with('/') {
            return Err(HandleError::Harness(format!(
                "the path '{}' does not start with '/'",
                request.path
            )));
        }
        if route == Reach::CourierInServer {
            return Err(HandleError::Harness(format!(
                "no HTTP request is routed through {:?}",
                route
            )));
        }
        let port = self.runner_port(endpoint)?;
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let client = match CLIENT.get() {
            Some(client) => client,
            None => {
                // Panoramic turns on the `zstd` feature of reqwest, and other crates in the build can turn on more.
                // Each one would add `Accept-Encoding` and decode the reply body, so the client turns them all off.
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .no_gzip()
                    .no_brotli()
                    .no_zstd()
                    .no_deflate()
                    .pool_max_idle_per_host(0)
                    .build()
                    .map_err(harness)?;
                CLIENT.get_or_init(|| client)
            }
        };
        let scheme = if planned.tls { "https" } else { "http" };
        let response = client
            .request(
                request.method.clone(),
                format!("{}://127.0.0.1:{}{}", scheme, port, request.path),
            )
            .headers(request.headers.clone())
            .body(request.body.clone())
            .timeout(limit)
            .send()
            .await
            .map_err(request_failed)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(request_failed)?.to_vec();
        Ok(HttpReply { status, headers, body })
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

    /// The participant's program `name`.
    fn program(&self, name: &str) -> Result<&'a PlannedProgram, HandleError> {
        let programs: &[PlannedProgram] = match &self.planned.launch {
            Launch::Programs(programs) => programs,
            Launch::Image { .. } => &[],
        };
        programs
            .iter()
            .find(|program| program.name == name)
            .ok_or_else(|| HandleError::Harness(format!("participant '{}' has no program '{}'", self.name, name)))
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

    /// The address of `endpoint` as the courier library, which runs in the harness, reaches it: on the loopback
    /// address of the runner host, where host processes listen.
    fn in_runner(
        &self, endpoint: &PlannedEndpoint, operation: EndpointOperation,
    ) -> Result<panoramic_courier::Address, HandleError> {
        let address = endpoint.courier_address("127.0.0.1", operation);
        address.parse().map_err(|e| {
            HandleError::Harness(format!(
                "the address '{}' is not one the courier reaches: {}",
                address, e
            ))
        })
    }

    fn courier(&self) -> Result<Courier<'a>, HandleError> {
        let enclosure = self.planned.realization.enclosure;
        let facts = enclosure.facts();
        let path = facts
            .courier
            .ok_or_else(|| HandleError::Harness(format!("a {} has no courier", enclosure)))?;
        let courier = Courier::new(self.realized, path);
        Ok(if facts.exec_input_ends {
            courier
        } else {
            courier.without_stdin()
        })
    }

    /// Requests `path` from the runner, through the port published or forwarded for `endpoint` on the runner's
    /// loopback address, over HTTPS if `tls`. A redirect is a response, as through the courier.
    async fn published_get(
        &self, endpoint: &str, tls: bool, path: &str, skip_verify: bool, read_body: bool, limit: Duration,
    ) -> Result<HttpResponse, HandleError> {
        let port = self.runner_port(endpoint)?;
        // One client of each kind for the process, so requests share their connection pools.
        static VERIFYING: OnceLock<reqwest::Client> = OnceLock::new();
        static NOT_VERIFYING: OnceLock<reqwest::Client> = OnceLock::new();
        let cell = if skip_verify { &NOT_VERIFYING } else { &VERIFYING };
        let client = match cell.get() {
            Some(client) => client,
            None => {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .danger_accept_invalid_certs(skip_verify)
                    .build()
                    .map_err(harness)?;
                cell.get_or_init(|| client)
            }
        };
        let scheme = if tls { "https" } else { "http" };
        let response = client
            .get(format!("{}://127.0.0.1:{}{}", scheme, port, path))
            .timeout(limit)
            .send()
            .await
            .map_err(request_failed)?;
        let status = response.status().as_u16();
        let body = if read_body {
            response.bytes().await.map_err(request_failed)?.to_vec()
        } else {
            Vec::new()
        };
        Ok(HttpResponse { status, body })
    }

    /// The port on the runner's loopback address that reaches `endpoint`: published, forwarded, or the endpoint's own.
    fn runner_port(&self, endpoint: &str) -> Result<u16, HandleError> {
        self.realized.host_port(endpoint).ok_or_else(|| {
            HandleError::Software(format!(
                "endpoint '{}' of participant '{}' has no published port: the participant had exited by the time its \
                 ports were read",
                endpoint, self.name
            ))
        })
    }
}

/// Blames a request that failed on the client's setup, or else on the software.
fn request_failed(error: reqwest::Error) -> HandleError {
    if error.is_builder() {
        harness(error)
    } else {
        HandleError::Software(format!("{:#}", GenericError::from(error)))
    }
}

fn harness(error: impl Into<GenericError>) -> HandleError {
    HandleError::Harness(format!("{:#}", error.into()))
}

/// The error for a route that [`reach`] never gives for a probe or a send.
fn no_route(route: Reach) -> HandleError {
    HandleError::Harness(format!("no probe or send is routed through {:?}", route))
}

impl From<panoramic_courier::Error> for HandleError {
    /// Blames who the courier library says is to blame.
    fn from(error: panoramic_courier::Error) -> Self {
        match error.fault() {
            panoramic_courier::Fault::Software => Self::Software(error.to_string()),
            panoramic_courier::Fault::Harness => Self::Harness(error.to_string()),
        }
    }
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
            (
                None,
                Ok(Some(b"{\"code\":3,\"signal\":0,\"time\":\"1794511601.5\"}\n")),
                "job",
                exited(3, None),
            ),
            (
                Some(Ok(0)),
                Ok(Some(b"{\"code\":256,\"signal\":9,\"time\":\"1794511601.5\"}\n")),
                "job",
                exited(256, Some(9)),
            ),
            (Some(Ok(0)), Ok(None), "job", Err("has exited and exit record")),
            (None, Ok(Some(b"{\"code\":3,\"signal\":0}\n")), "job", Err("is not '{")),
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

//! The handle a step holds: what a step or an operation acts on a started participant through.
//!
//! Every method works the same on every realization and says whose problem a failure is, with [`HandleError`]. Named
//! commands and operations are the plan's data, so nothing looks up a participant type. An endpoint is reached by the
//! route the plan's rule gives for what is done, the endpoint's protocol and the participant's realization: through the
//! courier inside the participant, or from the runner through a published port, with the runner's own HTTP client.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use saluki_error::GenericError;
use serde_json::Value;
use tokio::time::Instant;

use super::backend::{Exec, Realized};
use super::courier::{Courier, HttpResponse};
use super::plan::PlannedParticipant;
use super::reference::Values;
use super::spec::Protocol;
use super::step::EndpointOperation;
use super::wiring::{reach, PlannedEndpoint, Reach};

/// Why a handle operation failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HandleError {
    /// The mechanism failed. The step is `errored`.
    Harness(String),
    /// The mechanism worked and the software answered badly. The step is `failed`.
    Software(String),
}

/// How a command run in a participant ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Exited {
    pub(crate) code: i64,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// What a step acts on one started participant through.
// TODO: add `output`, `exit` and `wait_for_exit` with observation in the lifecycle.
pub(crate) struct Handle<'a> {
    name: &'a str,
    planned: &'a PlannedParticipant,
    realized: &'a dyn Realized,
    /// What fills the references of named commands when they run.
    values: &'a Values,
}

impl<'a> Handle<'a> {
    pub(crate) fn new(
        name: &'a str, planned: &'a PlannedParticipant, realized: &'a dyn Realized, values: &'a Values,
    ) -> Self {
        Self {
            name,
            planned,
            realized,
            values,
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
        if planned.protocol != Protocol::Http {
            return Err(HandleError::Harness(format!(
                "endpoint '{}' is {}, and an HTTP GET goes to an http endpoint",
                endpoint,
                planned.protocol.as_str()
            )));
        }
        if !path.starts_with('/') {
            return Err(HandleError::Harness(format!(
                "the path '{}' does not start with '/'",
                path
            )));
        }
        match route {
            Reach::PublishedPort => self.published_get(endpoint, planned, path, limit).await,
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
    async fn published_get(
        &self, endpoint: &str, planned: &PlannedEndpoint, path: &str, limit: Duration,
    ) -> Result<HttpResponse, HandleError> {
        if planned.tls {
            // TODO: request TLS endpoints, with `http_check`'s `insecure_skip_verify`, with the first case that does.
            return Err(HandleError::Harness(format!(
                "endpoint '{}' serves TLS, and requests to TLS endpoints are not built",
                endpoint
            )));
        }
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

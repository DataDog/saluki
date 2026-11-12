//! The courier client: probes, sends, HTTP requests and file follows, run inside a started participant.
//!
//! The backend places the courier in each container with the participant's files, and the harness runs it there
//! through exec, so it reaches a listener the way a peer would. A probe from outside proves nothing: the Docker port
//! proxy accepts a connection even when nothing listens inside. Following a log file with the courier, rather than
//! `sh` and `tail`, keeps both from being a requirement of every image.
//!
//! The courier's exit code says who to blame: 0 when the operation completed, 1 when the software refused or is not
//! there yet, and anything else when the courier could not do what was asked. A participant that has exited runs no
//! courier, or kills the one running, and that is the software's fault too.

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncWrite;
use tokio::time::Instant;

use super::backend::{Exec, Realized};
use super::handle::HandleError;

/// The courier's exit code when the software refused or is not there yet.
const EXIT_SOFTWARE: i64 = 1;

/// How long past the courier's own deadline the harness waits for it to exit.
const EXIT_MARGIN: Duration = Duration::from_secs(5);

/// A response to an HTTP `GET` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// Runs the courier inside one started participant.
pub(crate) struct Courier<'a> {
    on: &'a dyn Realized,
    /// Where the courier is in the participant.
    path: &'a str,
}

impl<'a> Courier<'a> {
    pub(crate) fn new(on: &'a dyn Realized, path: &'a str) -> Self {
        Self { on, path }
    }

    /// Checks that something accepts a connection at `address`, or that a socket or pipe exists there.
    ///
    /// # Errors
    ///
    /// Returns a software error if nothing accepts within `limit`, and a harness error if the courier cannot run.
    pub(crate) async fn probe(&self, address: &str, limit: Duration) -> Result<(), HandleError> {
        self.run("probe", &[address], None, limit, &mut tokio::io::sink()).await
    }

    /// Opens one connection to `address`, or sends one datagram, and writes `payload` as given.
    ///
    /// # Errors
    ///
    /// Returns a software error if the address refuses the bytes within `limit`, and a harness error if the courier
    /// cannot run.
    // TODO: remove the dead_code allowance once a step sends to an endpoint.
    #[allow(dead_code)]
    pub(crate) async fn send(&self, address: &str, payload: Bytes, limit: Duration) -> Result<(), HandleError> {
        self.run("send", &[address], Some(payload), limit, &mut tokio::io::sink())
            .await
    }

    /// Makes one plain-HTTP `GET` request for `path` at `address`. Any status is a response.
    ///
    /// # Errors
    ///
    /// Returns a software error if no well-formed response arrives within `limit`, and a harness error if the courier
    /// cannot run or its output is not a response.
    pub(crate) async fn http_get(
        &self, address: &str, path: &str, limit: Duration,
    ) -> Result<HttpResponse, HandleError> {
        let mut out = Vec::new();
        self.run("http", &[address, "--path", path], None, limit, &mut out)
            .await?;
        let (status, body) = out
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|end| (&out[..end], &out[end + 1..]))
            .ok_or_else(|| HandleError::Harness("the courier's http output has no status line".to_string()))?;
        let status = String::from_utf8_lossy(status)
            .parse()
            .map_err(|_| HandleError::Harness("the courier's http output starts with no status code".to_string()))?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }

    /// Writes the contents of the file at `path` to `out` as they are written, from the start, for `limit`.
    ///
    /// # Errors
    ///
    /// Returns a software error if the file does not appear within `limit`, and a harness error if the courier cannot
    /// run or `out` cannot be written.
    pub(crate) async fn follow(
        &self, path: &str, limit: Duration, out: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<(), HandleError> {
        self.run("follow", &[path], None, limit, out).await
    }

    async fn run(
        &self, operation: &str, args: &[&str], stdin: Option<Bytes>, limit: Duration,
        stdout: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<(), HandleError> {
        let mut argv = vec![
            self.path.to_string(),
            "--deadline".to_string(),
            format!("{}ms", limit.as_millis().max(1)),
            operation.to_string(),
        ];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let exec = Exec {
            argv,
            env: BTreeMap::new(),
            stdin,
        };
        let mut stderr = Vec::new();
        let deadline = Instant::now() + limit + EXIT_MARGIN;
        let code = self.on.exec(exec, stdout, &mut stderr, deadline).await?;
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        match code {
            0 => Ok(()),
            EXIT_SOFTWARE => Err(HandleError::Software(stderr)),
            // A participant that exits takes the courier with it.
            code if self.on.has_exited().await => Err(HandleError::Software(format!(
                "the participant exited during the courier's {}, which exited with code {}: {}",
                operation, code, stderr
            ))),
            code => Err(HandleError::Harness(format!(
                "the courier's {} exited with code {}: {}",
                operation, code, stderr
            ))),
        }
    }
}

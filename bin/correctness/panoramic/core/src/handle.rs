//! Operations on one participant through its backend, and their failures.

use std::time::Duration;

use airlock::{exec::ExecOutput, files::FileStat};
use async_trait::async_trait;
use snafu::Snafu;

use crate::{
    endpoint::ResolvedAddress,
    probe::{AttemptLimits, ProbeAnswer},
};

/// One command for a participant to run.
pub(crate) struct ExecCommand {
    /// The program followed by its arguments.
    pub(crate) argv: Vec<String>,
    /// Variables added to the participant's environment.
    pub(crate) env: Vec<(String, String)>,
    /// Bytes written to stdin, which is then closed; `None` runs with no stdin.
    ///
    /// The backend writes all bytes and closes stdin, even for an empty payload.
    pub(crate) stdin: Option<Vec<u8>>,
}

/// Operations on one participant, implemented by its backend.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Handles reach participants through their backend's primitives.")
)]
#[async_trait]
pub(crate) trait Primitives: Send + Sync {
    /// Runs a command, abandoning it after `limit`. Any exit code is data.
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError>;
    /// Reads a regular file; `None` when the path is missing.
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError>;
    /// Reports a path's kind, mode and size; `None` when the path is missing.
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, HandleError>;
    /// Probes an address as seen from inside this participant.
    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError>;
    /// Sends a payload to an address as seen from inside this participant.
    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError>;
}

/// Why an operation on a participant failed.
#[derive(Debug, PartialEq, Eq, Snafu)]
pub(crate) enum HandleError {
    /// The execution mechanism failed.
    #[snafu(display("{message}"))]
    Harness { message: String },
    /// The mechanism worked and the endpoint answered badly.
    #[snafu(display("{message}"))]
    Product { message: String },
}

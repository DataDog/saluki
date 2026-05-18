use std::io;

use snafu::Snafu;

/// Secrets resolution error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)), visibility(pub(super)))]
pub enum Error {
    /// The provided secrets backend command was invalid.
    #[snafu(display("the provided backend command is invalid: {source}"))]
    BackendCommandInvalid {
        /// Error source.
        source: io::Error,
    },

    /// Failed to call the secrets backend command.
    #[snafu(display("failed to call secrets backend command: {source}"))]
    FailedToCallBackend {
        /// Error source.
        source: io::Error,
    },

    /// The secrets backend command exited with a non-zero status code.
    #[snafu(display(
        "backend command '{}' failed with exit code {}: {}",
        backend_command,
        exit_code,
        error
    ))]
    BackendFailed {
        /// Backend command path.
        backend_command: String,

        /// Exit code of the backend command.
        exit_code: i32,

        /// Error description.
        error: String,
    },

    /// Timed out waiting for the secrets backend command to return.
    #[snafu(display("secrets backend command failed to return within {timeout} seconds"))]
    TimedOutCallingBackend {
        /// Timeout duration, in seconds.
        timeout: u64,
    },

    /// Failed to deserialize the response from the secrets backend command
    #[snafu(display("failed to deserialize response from backend: {source}"))]
    FailedToDeserializeResponse {
        /// Error source.
        source: serde_json::Error,
    },

    /// Failed to resolve secrets.
    #[snafu(display("encountered an error when resolving secret '{}': {}", secret_ref, error))]
    FailedToResolve {
        /// Secret reference that the error relates to.
        secret_ref: String,

        /// Error description.
        error: String,
    },
}

//! Errors from operations on one participant through its backend.

use snafu::Snafu;

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

//! Calling contract for the courier, which sends to and probes endpoints from inside a container.
//!
//! Argument rendering, parsing and outcome decoding live here so callers and the executable share one wire contract.

#![deny(missing_docs)]

mod args;
mod report;
mod target;

pub use args::{Command, Invocation, UsageError};
pub use report::{Report, ReportError};
pub use target::Target;

//! Runtime introspection: reporting the shape and state of ADP's supervision tree.
//!
//! ADP's runtime is a supervision tree, which makes the tree itself the most complete description of what the process
//! is doing: which subsystems exist, which are running, which have restarted and how often, and how much memory and
//! CPU each accounts for. This module exposes that tree over the API so it can be read without attaching a debugger
//! or reconstructing it from log lines.
//!
//! The route constant and response types are shared with the CLI client in [`crate::cli`], so both sides of the wire
//! are defined once here.

mod api;

pub use self::api::RuntimeProcessesWorker;

/// API route serving a snapshot of the supervision tree.
///
/// Served on the privileged endpoint. See [`RuntimeProcessesWorker`] for why.
pub const RUNTIME_PROCESSES_ROUTE: &str = "/runtime/processes";

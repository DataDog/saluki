//! The contract that a supervised process implements.
//!
//! A supervisor drives a [`Supervisable`]. The [`Supervisable`] names the process, tells how to shut it down, and
//! builds the future of the process each time the process starts. This module also contains the types in its signature.

use std::{future::Future, pin::Pin, time::Duration};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::GenericError;
use snafu::Snafu;

/// A `Future` that represents the execution of a supervised process.
pub type SupervisorFuture = Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;

/// Initialization errors.
///
/// Initialization errors are distinct from runtime errors: they indicate that a process couldn't be started at all
/// (for example, failed to bind a port, missing configuration). These errors don't trigger restart logic; instead, they
/// immediately propagate up and fail the supervisor.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum InitializationError {
    /// The process couldn't be initialized due to an error.
    #[snafu(display("Process failed to initialize: {}", source))]
    Failed {
        /// The underlying error that caused initialization to fail.
        source: GenericError,
    },
}

impl From<GenericError> for InitializationError {
    fn from(source: GenericError) -> Self {
        Self::Failed { source }
    }
}

/// Strategy for shutting down a process.
#[derive(Clone, Copy, Debug)]
pub enum ShutdownStrategy {
    /// Signals the process, then halts it if it hasn't exited within the given duration.
    ///
    /// The duration also covers the children in the process's [scope][crate::runtime::scope], which are halted along
    /// with it. A halt is reported as an unclean shutdown.
    Graceful(Duration),

    /// Halts the process and everything it owns immediately, without signalling it.
    ///
    /// For work with no terminal condition. Not reported as an unclean shutdown.
    Brutal,
}

/// A supervisable process.
#[async_trait]
pub trait Supervisable: Send + Sync {
    /// Returns the name of the process.
    fn name(&self) -> &str;

    /// Returns the shutdown strategy for the process.
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(Duration::from_secs(5))
    }

    /// Returns whether this process observes the shutdown signal it is given.
    ///
    /// The shutdown of a subtree is a _trigger_, not an enforcement. Many workers ignore the signal completely, and
    /// stop only when they reach their own terminal condition, such as when an input channel closes. If this function
    /// returns `false`, the supervisor gives the process a [`ShutdownHandle::noop`] instead. But the
    /// [scope][crate::runtime::scope] of the process still observes the signal for the process, so that it can tell
    /// whether an exit of one of its children was requested.
    ///
    /// It also decides whether a nested supervisor in that scope that
    /// [stops with its owner][crate::runtime::NestedSupervisorBuilder::stops_with_owner] is told to stop together with
    /// the process. If this function returns `false`, the nested supervisor stops only when the scope closes.
    ///
    /// This says nothing about _whether_ the supervisor waits for the process -- that's
    /// [`shutdown_strategy`][Self::shutdown_strategy]. A process that ignores the signal is still waited for, up to its
    /// shutdown timeout.
    ///
    /// Defaults to `true`.
    fn wants_shutdown_signal(&self) -> bool {
        true
    }

    /// Returns whether the process can be started again after it has started once.
    ///
    /// [`initialize`][Self::initialize] builds the future of the process each time the process starts. Most processes
    /// can build it again, so their owner can restart them. A process that consumes something when it starts cannot
    /// build its future a second time. Examples are a connection, or a value that the process received before it ran.
    /// Such a process returns `false` here.
    ///
    /// The owner of a process that returns `false` never restarts it, whatever restart policy the process has. When the
    /// process stops, its owner reaps it, or fails if it needs the process. See the
    /// [runtime documentation][crate::runtime#restart-policy] for how this decides the restart policy.
    ///
    /// Defaults to `true`.
    fn can_restart(&self) -> bool {
        true
    }

    /// Initializes the process asynchronously.
    ///
    /// During initialization, any resources or configuration for the process can be created asynchronously, and the
    /// same runtime that's used for running the process is used for initialization. The resulting future is expected to
    /// complete as soon as reasonably possible after `shutdown` resolves.
    ///
    /// **Important:** The `process_shutdown` signal must be moved into the returned [`SupervisorFuture`] so the worker
    /// can respond to supervisor-initiated shutdown. If `process_shutdown` is dropped during initialization, the worker
    /// will be unable to shut down gracefully and will be halted after the shutdown timeout.
    ///
    /// # Errors
    ///
    /// If the process can't be initialized, an error is returned.
    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest possible implementation, existing only to be erased below.
    struct Noop;

    #[async_trait]
    impl Supervisable for Noop {
        fn name(&self) -> &str {
            "noop"
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            Ok(Box::pin(async move {
                process_shutdown.await;
                Ok(())
            }))
        }
    }

    #[test]
    fn trait_is_object_safe() {
        // Producers hand their background work over as `Vec<Box<dyn Supervisable>>`, so object safety is load-bearing
        // here rather than incidental: a default method taking `self` by value, or a generic one, would break every
        // one of those call sites.
        let worker: Box<dyn Supervisable> = Box::new(Noop);

        assert_eq!(worker.name(), "noop");
        assert!(worker.wants_shutdown_signal());
        assert!(worker.can_restart());
        assert!(matches!(
            worker.shutdown_strategy(),
            ShutdownStrategy::Graceful(timeout) if timeout == Duration::from_secs(5)
        ));
    }
}

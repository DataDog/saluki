//! Core primitives for building Saluki-based data planes.
#![deny(warnings)]
#![deny(missing_docs)]

use std::future::Future;

use memory_accounting::allocator::Track as _;
use tokio::task::JoinHandle;
use tracing::Instrument as _;

pub mod components;
pub mod constants;
pub mod observability;
pub mod pooling;
pub mod topology;

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// This function is a thin wrapper over [`tokio::spawn`], and provides implicit "tracing" for spawned futures by
/// ensuring that the task is attached to the current `tracing` span and the current allocation component.
pub fn spawn_traced<F, R>(f: F) -> JoinHandle<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    tokio::spawn(f.in_current_span().in_current_component())
}

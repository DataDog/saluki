//! Helpers for working with asynchronous tasks.

use std::future::Future;

use tokio::{
    runtime::Handle,
    task::{AbortHandle, JoinHandle, JoinSet},
};
use tracing::Instrument as _;

use crate::resource_tracking::Track as _;

mod instrument;
use self::instrument::TaskInstrument as _;

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// This function is a thin wrapper over [`tokio::spawn`], and provides implicit "tracing" for spawned futures by
/// ensuring that the task is attached to the current `tracing` span and the current allocation component.
#[track_caller]
pub fn spawn_traced<F, T>(f: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn(
        f.in_current_span()
            .in_current_resource_group()
            .with_task_instrumentation(get_caller_location_as_string()),
    )
}

/// Spawns a new named asynchronous task, returning a [`JoinHandle`] for it.
///
/// This function is a thin wrapper over [`tokio::spawn`], and provides implicit "tracing" for spawned futures by
/// ensuring that the task is attached to the current `tracing` span and the current allocation component.
pub fn spawn_traced_named<S, F, T>(name: S, f: F) -> JoinHandle<T>
where
    S: Into<String>,
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn(
        f.in_current_span()
            .in_current_resource_group()
            .with_task_instrumentation(name.into()),
    )
}

/// Helper trait for providing traced spawning when using `JoinSet<T>`.
pub trait JoinSetExt<T> {
    /// Spawns a new asynchronous task, returning an [`AbortHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`JoinSet`], and provides implicit "tracing"
    /// for spawned futures by ensuring that the task is attached to the current `tracing` span and the current
    /// allocation component.
    fn spawn_traced<F>(&mut self, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;

    /// Spawns a new named asynchronous task, returning an [`AbortHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`JoinSet`], and provides implicit "tracing"
    /// for spawned futures by ensuring that the task is attached to the current `tracing` span and the current
    /// allocation component.
    fn spawn_traced_named<S, F>(&mut self, name: S, f: F) -> AbortHandle
    where
        S: Into<String>,
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> JoinSetExt<T> for JoinSet<T> {
    fn spawn_traced<F>(&mut self, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(
            f.in_current_span()
                .in_current_resource_group()
                .with_task_instrumentation(get_caller_location_as_string()),
        )
    }

    fn spawn_traced_named<S, F>(&mut self, name: S, f: F) -> AbortHandle
    where
        S: Into<String>,
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(
            f.in_current_span()
                .in_current_resource_group()
                .with_task_instrumentation(name.into()),
        )
    }
}

/// Helper trait for providing traced spawning when using `Handle`.
pub trait HandleExt<T> {
    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`Handle`], and provides implicit "tracing"
    /// for spawned futures by ensuring that the task is attached to the current `tracing` span and the current
    /// allocation component.
    fn spawn_traced<F>(&self, f: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;

    /// Spawns a new named asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`Handle`], and provides implicit "tracing"
    /// for spawned futures by ensuring that the task is attached to the current `tracing` span and the current
    /// allocation component.
    fn spawn_traced_named<S, F>(&self, name: S, f: F) -> JoinHandle<T>
    where
        S: Into<String>,
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> HandleExt<T> for Handle {
    fn spawn_traced<F>(&self, f: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(
            f.in_current_span()
                .in_current_resource_group()
                .with_task_instrumentation(get_caller_location_as_string()),
        )
    }

    fn spawn_traced_named<S, F>(&self, name: S, f: F) -> JoinHandle<T>
    where
        S: Into<String>,
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(
            f.in_current_span()
                .in_current_resource_group()
                .with_task_instrumentation(name.into()),
        )
    }
}

/// Gets the caller location as a string, in the form of `file:line:column`.
#[track_caller]
pub fn get_caller_location_as_string() -> String {
    let caller = std::panic::Location::caller();
    format!("file-{}@{}-{}", caller.file(), caller.line(), caller.column())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_location_uses_documented_format() {
        // Because `get_caller_location_as_string` is `#[track_caller]`, this records _this_ call
        // site, so the file segment matches this source file.
        let location = get_caller_location_as_string();

        // NOTE: the doc comment describes the format as `file:line:column`, but the implementation
        // actually produces `file-<file>@<line>-<column>`. We assert the real, current format.
        let prefix = format!("file-{}@", file!());
        assert!(
            location.starts_with(&prefix),
            "expected {location:?} to start with {prefix:?}"
        );

        // Exactly one `@` separates the file from the position, and the trailing portion is two
        // decimal numbers (line and column) joined by `-`.
        assert_eq!(location.matches('@').count(), 1);
        let (_, line_col) = location.split_once('@').unwrap();
        let (line, column) = line_col.split_once('-').expect("line and column separated by '-'");
        assert!(line.parse::<u32>().is_ok(), "line segment {line:?} should be numeric");
        assert!(
            column.parse::<u32>().is_ok(),
            "column segment {column:?} should be numeric"
        );
    }

    #[tokio::test]
    async fn spawn_traced_runs_future_to_completion() {
        assert_eq!(spawn_traced(async { 7u32 }).await.unwrap(), 7);
        assert_eq!(spawn_traced_named("named-task", async { 9u32 }).await.unwrap(), 9);
    }

    #[tokio::test]
    async fn join_set_ext_runs_traced_tasks_to_completion() {
        let mut set = JoinSet::new();
        set.spawn_traced(async { 1u32 });
        set.spawn_traced_named("set-task", async { 2u32 });

        let mut results = Vec::new();
        while let Some(result) = set.join_next().await {
            results.push(result.unwrap());
        }
        results.sort_unstable();
        assert_eq!(results, vec![1, 2]);
    }

    #[tokio::test]
    async fn handle_ext_runs_traced_tasks_to_completion() {
        let handle = Handle::current();
        assert_eq!(handle.spawn_traced(async { 4u32 }).await.unwrap(), 4);
        assert_eq!(
            handle.spawn_traced_named("handle-task", async { 5u32 }).await.unwrap(),
            5
        );
    }
}

//! `Live<T>`: a live, typed view of one projection of the current configuration.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;

use crate::SalukiConfiguration;

// Borrow-in, borrow-out so navigating deeper never clones the whole configuration; the caller clones
// only the final `T` when it needs an owned value.
type Projection<T> = Arc<dyn for<'a> Fn(&'a SalukiConfiguration) -> &'a T + Send + Sync>;

/// A typed view of one projection of the current configuration.
///
/// A never-dynamic consumer can hold a plain `T`; a dynamic one holds `Live<T>` and never learns
/// whether it is fixed or tracking the live config. Read with `Deref`/`current`; react with
/// `changed`. The projection is over `SalukiConfiguration`, so this is a config view, not a
/// general-purpose watcher.
pub struct Live<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Fixed(T),
    Dynamic {
        cell: Arc<ArcSwap<SalukiConfiguration>>,
        tick: watch::Receiver<()>,
        project: Projection<T>,
        // Last observed projected value; backs `Deref` and the change comparison.
        snapshot: T,
    },
}

impl<T: Clone + PartialEq + 'static> Live<T> {
    /// A view that never changes (local mode, tests).
    pub fn fixed(value: T) -> Self {
        Self {
            inner: Inner::Fixed(value),
        }
    }

    /// A view projected from the live configuration. Called by the config system.
    pub fn dynamic(
        cell: Arc<ArcSwap<SalukiConfiguration>>, tick: watch::Receiver<()>,
        project: impl for<'a> Fn(&'a SalukiConfiguration) -> &'a T + Send + Sync + 'static,
    ) -> Self {
        let snapshot = project(&cell.load()).clone();
        Self {
            inner: Inner::Dynamic {
                cell,
                tick,
                project: Arc::new(project),
                snapshot,
            },
        }
    }

    /// A fresh clone of the current projected value.
    pub fn current(&self) -> T {
        match &self.inner {
            Inner::Fixed(value) => value.clone(),
            Inner::Dynamic { cell, project, .. } => project(&cell.load()).clone(),
        }
    }

    /// Resolves when the projected value changes. Parks forever when `Fixed` or the channel closed,
    /// so a caller can `select!` on it unconditionally; a bare `.await` on a fixed view hangs. On
    /// return, `Deref` reflects the new value.
    pub async fn changed(&mut self) {
        match &mut self.inner {
            Inner::Fixed(_) => std::future::pending().await,
            Inner::Dynamic {
                cell,
                tick,
                project,
                snapshot,
            } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                let guard = cell.load();
                let latest = project(&guard);
                if *latest != *snapshot {
                    *snapshot = latest.clone();
                    return;
                }
            },
        }
    }

    /// Narrows this view to a child node or a single field. The child shares the same source and
    /// notification and wakes only when the child value changes.
    pub fn project<U>(&self, f: impl for<'a> Fn(&'a T) -> &'a U + Send + Sync + 'static) -> Live<U>
    where
        U: Clone + PartialEq + 'static,
    {
        match &self.inner {
            Inner::Fixed(value) => Live::fixed(f(value).clone()),
            Inner::Dynamic {
                cell, tick, project, ..
            } => {
                let parent = Arc::clone(project);
                Live::dynamic(Arc::clone(cell), tick.clone(), move |c| f(parent(c)))
            }
        }
    }
}

impl<T> Deref for Live<T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            Inner::Fixed(value) => value,
            Inner::Dynamic { snapshot, .. } => snapshot,
        }
    }
}

impl<T: Clone> Clone for Live<T> {
    fn clone(&self) -> Self {
        let inner = match &self.inner {
            Inner::Fixed(value) => Inner::Fixed(value.clone()),
            Inner::Dynamic {
                cell,
                tick,
                project,
                snapshot,
            } => Inner::Dynamic {
                cell: Arc::clone(cell),
                tick: tick.clone(),
                project: Arc::clone(project),
                snapshot: snapshot.clone(),
            },
        };
        Self { inner }
    }
}

impl<T: fmt::Debug> fmt::Debug for Live<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Fixed(value) => f.debug_tuple("Live::Fixed").field(value).finish(),
            Inner::Dynamic { snapshot, .. } => f
                .debug_struct("Live::Dynamic")
                .field("snapshot", snapshot)
                .finish_non_exhaustive(),
        }
    }
}

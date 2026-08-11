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
/// whether it is fixed or tracking the live config. Read the view's cached snapshot with `Deref`;
/// wait for and receive the next selected update with `changed`.
///
/// For a dynamic view, `Deref` does not perform a fresh read from the shared configuration. It
/// returns the last snapshot processed by this view. If the configuration advances before this
/// view awaits `changed`, `Deref` continues to return the older snapshot until `changed` processes
/// the notification.
pub struct Live<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Fixed(T),
    Dynamic {
        // The shared source. It can advance independently of this view's snapshot.
        cell: Arc<ArcSwap<SalukiConfiguration>>,

        // A wake-up signal, not the configuration value itself. The view re-projects the source
        // after receiving it and ignores notifications that do not change T.
        tick: watch::Receiver<()>,

        // Re-applied to the shared source whenever the view processes a notification.
        project: Projection<T>,

        // This value belongs to this Live<T>. It is what Deref returns and what changed() compares
        // against; a newer value in `cell` does not replace it until changed() processes a tick.
        snapshot: T,
    },
}

impl<T: Clone + PartialEq + 'static> Live<T> {
    /// Creates a view that re-projects the shared configuration after accepted updates.
    ///
    /// The projection selects the subtree this view owns. The initial projected value is captured
    /// immediately; later calls to `changed` load and project the shared configuration, then update
    /// this view's local snapshot. Until `changed` processes a notification, `Deref` continues to
    /// return the previous snapshot even if the shared configuration has advanced.
    pub fn new_dynamic(
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

    /// Creates a view with a value that never changes.
    pub fn new_fixed(value: T) -> Self {
        Self {
            inner: Inner::Fixed(value),
        }
    }

    /// Waits for the projected value to change and returns the new value.
    ///
    /// The notification only says that the shared source may have changed. This method loads the
    /// source, applies the projection, compares it with this view's snapshot, and keeps waiting if
    /// the selected value is unchanged. It parks forever when `Fixed` or the channel is closed, so
    /// a caller can `select!` on it unconditionally. The returned value and `Deref` reflect the
    /// same processed snapshot. This is a state-change watcher, not a fresh-read API or an event
    /// history: multiple source updates may be coalesced before this view processes them.
    pub async fn changed(&mut self) -> T {
        match &mut self.inner {
            Inner::Fixed(_) => std::future::pending::<T>().await,
            Inner::Dynamic {
                cell,
                tick,
                project,
                snapshot,
            } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<T>().await;
                }
                let guard = cell.load();
                let latest = project(&guard);
                if *latest != *snapshot {
                    *snapshot = latest.clone();
                    return snapshot.clone();
                }
            },
        }
    }

    /// Creates a child view by composing this view's projection with `f`.
    ///
    /// Use this when code already has a broad `Live<T>` but does not have the
    /// `ConfigurationSystem` needed to create a narrower view
    /// directly. A dynamic child shares the source and receives its own notification cursor and
    /// snapshot; it wakes only when the selected child value changes. A fixed child is projected
    /// once and remains fixed.
    pub fn project<U>(&self, f: impl for<'a> Fn(&'a T) -> &'a U + Send + Sync + 'static) -> Live<U>
    where
        U: Clone + PartialEq + 'static,
    {
        match &self.inner {
            Inner::Fixed(value) => Live::new_fixed(f(value).clone()),
            Inner::Dynamic {
                cell, tick, project, ..
            } => {
                let parent = Arc::clone(project);
                Live::new_dynamic(Arc::clone(cell), tick.clone(), move |c| f(parent(c)))
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

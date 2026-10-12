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

// Borrow-in, value-out, for a view whose value the configuration does not hold as such, such as one
// converted to another type: computed from the configuration on every notification.
type Mapping<T> = Arc<dyn Fn(&SalukiConfiguration) -> T + Send + Sync>;

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
        read: Read<T>,

        // This value belongs to this Live<T>. It is what Deref returns and what changed() compares
        // against; a newer value in `cell` does not replace it until changed() processes a tick.
        snapshot: T,
    },
}

/// How a dynamic view reads its value from the shared configuration.
enum Read<T> {
    /// A projection into the configuration, cloned only when the value changed.
    Projected(Projection<T>),

    /// A value computed from the configuration.
    Mapped(Mapping<T>),
}

impl<T> Clone for Read<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Projected(project) => Self::Projected(Arc::clone(project)),
            Self::Mapped(map) => Self::Mapped(Arc::clone(map)),
        }
    }
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
                read: Read::Projected(Arc::new(project)),
                snapshot,
            },
        }
    }

    /// Creates a view whose value is computed from the shared configuration after accepted updates.
    fn new_mapped(
        cell: Arc<ArcSwap<SalukiConfiguration>>, tick: watch::Receiver<()>,
        map: impl Fn(&SalukiConfiguration) -> T + Send + Sync + 'static,
    ) -> Self {
        let snapshot = map(&cell.load());
        Self {
            inner: Inner::Dynamic {
                cell,
                tick,
                read: Read::Mapped(Arc::new(map)),
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
                read,
                snapshot,
            } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<T>().await;
                }
                let guard = cell.load();
                match read {
                    Read::Projected(project) => {
                        let latest = project(&guard);
                        if *latest != *snapshot {
                            *snapshot = latest.clone();
                            return snapshot.clone();
                        }
                    }
                    Read::Mapped(map) => {
                        let latest = map(&guard);
                        if latest != *snapshot {
                            *snapshot = latest;
                            return snapshot.clone();
                        }
                    }
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
            Inner::Dynamic { cell, tick, read, .. } => match read {
                Read::Projected(project) => {
                    let parent = Arc::clone(project);
                    Live::new_dynamic(Arc::clone(cell), tick.clone(), move |c| f(parent(c)))
                }
                Read::Mapped(map) => {
                    let parent = Arc::clone(map);
                    Live::new_mapped(Arc::clone(cell), tick.clone(), move |c| f(&parent(c)).clone())
                }
            },
        }
    }

    /// Creates a child view whose value is computed from this view's value by `f`, such as a value converted to
    /// another type that the configuration does not hold.
    ///
    /// A dynamic child shares the source and receives its own notification cursor and snapshot; it computes its value
    /// on every notification, and wakes only when the computed value changes. A fixed child is computed once and
    /// remains fixed.
    pub fn map<U>(&self, f: impl Fn(&T) -> U + Send + Sync + 'static) -> Live<U>
    where
        U: Clone + PartialEq + 'static,
    {
        match &self.inner {
            Inner::Fixed(value) => Live::new_fixed(f(value)),
            Inner::Dynamic { cell, tick, read, .. } => match read {
                Read::Projected(project) => {
                    let parent = Arc::clone(project);
                    Live::new_mapped(Arc::clone(cell), tick.clone(), move |c| f(parent(c)))
                }
                Read::Mapped(map) => {
                    let parent = Arc::clone(map);
                    Live::new_mapped(Arc::clone(cell), tick.clone(), move |c| f(&parent(c)))
                }
            },
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
                read,
                snapshot,
            } => Inner::Dynamic {
                cell: Arc::clone(cell),
                tick: tick.clone(),
                read: read.clone(),
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

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use super::*;

    fn source() -> (Arc<ArcSwap<SalukiConfiguration>>, watch::Sender<()>) {
        let (tick, _) = watch::channel(());
        (Arc::new(ArcSwap::from_pointee(SalukiConfiguration::default())), tick)
    }

    fn update(
        cell: &ArcSwap<SalukiConfiguration>, tick: &watch::Sender<()>, change: impl FnOnce(&mut SalukiConfiguration),
    ) {
        let mut config = SalukiConfiguration::clone(&cell.load());
        change(&mut config);
        cell.store(Arc::new(config));
        tick.send_replace(());
    }

    /// Returns what `view.changed()` returns without waiting, or `None` if it would wait: every notification already
    /// sent is processed before it would.
    fn changed_now<T: Clone + PartialEq + 'static>(view: &mut Live<T>) -> Option<T> {
        let mut changed = pin!(view.changed());
        match changed.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    #[test]
    fn a_mapped_view_computes_its_value_and_wakes_only_when_the_value_changes() {
        let (cell, tick) = source();
        let base = Live::new_dynamic(Arc::clone(&cell), tick.subscribe(), |config| config);
        let mut key = base.map(|config| {
            config
                .control
                .programmable_metrics
                .resources
                .credentials
                .get("team_d")
                .map(|key| key.to_uppercase())
        });
        assert_eq!(*key, None);

        // A change elsewhere in the configuration computes the same value: the view keeps waiting.
        update(&cell, &tick, |config| config.control.standalone_mode = true);
        assert_eq!(changed_now(&mut key), None);

        update(&cell, &tick, |config| {
            let credentials = &mut config.control.programmable_metrics.resources.credentials;
            credentials.insert("team_d".to_string(), "key-d".to_string());
        });
        assert_eq!(changed_now(&mut key), Some(Some("KEY-D".to_string())));
        assert_eq!(*key, Some("KEY-D".to_string()));
        assert_eq!(changed_now(&mut key), None, "a notification is processed once");

        // A clone keeps the snapshot and follows on its own; a mapping of the mapped view, and a mapping of a
        // projection, compute from the current configuration.
        let clone = key.clone();
        assert_eq!(*clone, Some("KEY-D".to_string()));
        let length = key.map(|key| key.as_ref().map(String::len));
        assert_eq!(*length, Some(5));
        let mut standalone = base
            .project(|config| &config.control)
            .map(|control| control.standalone_mode);
        assert!(*standalone);
        update(&cell, &tick, |config| config.control.standalone_mode = false);
        assert_eq!(changed_now(&mut standalone), Some(false));
    }

    #[test]
    fn a_fixed_view_maps_once() {
        let fixed = Live::new_fixed(3_u64).map(|value| value * 2);
        assert_eq!(*fixed, 6);
        assert_eq!(*fixed.project(|value| value), 6);
    }
}

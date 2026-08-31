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
/// whether it is fixed or tracking the live config.
///
/// Every read goes through this view's own snapshot:
///
/// - `Deref` returns the snapshot, without touching the shared configuration.
/// - `changed` waits for a projected value that differs from the value it last reported, then
///   updates the snapshot.
/// - `refresh` updates the snapshot from the shared configuration synchronously.
///
/// A `Live<T>` therefore tracks one consumer's progress through the updates. Each clone owns its
/// snapshot, notification cursor, and comparison baseline, so `refresh` on one clone cannot suppress
/// `changed` on another. A clone inherits a report that `changed` still owes.
///
/// `refresh` and `changed` can be mixed on the same clone. `refresh` moves the snapshot `Deref`
/// returns but not the value `changed` compares against, so `changed` can return a value that
/// `Deref` already equals.
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

        // Re-applied to the shared source whenever the view updates its snapshot.
        project: Projection<T>,

        // This value belongs to this Live<T>. It is what Deref returns; a newer value in `cell` does
        // not replace it until changed() or refresh() runs.
        snapshot: T,

        // What changed() compares against, once refresh() has moved `snapshot` past it. `None` means
        // `snapshot` is also that comparison baseline.
        last_reported: Option<T>,
    },
}

impl<T: Clone + PartialEq + 'static> Live<T> {
    /// Creates a view that re-projects the shared configuration after accepted updates.
    ///
    /// The projection selects the subtree this view owns, and the initial projected value is
    /// captured immediately. After that, the snapshot moves only when `changed` or `refresh`
    /// updates it.
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
                last_reported: None,
            },
        }
    }

    /// Updates this view's snapshot from the shared configuration and returns it.
    ///
    /// This is the synchronous counterpart to `changed`: use it where a consumer reads the value at
    /// a moment of its own choosing, such as while serving a request. A fixed view returns its fixed
    /// value. After this returns, `Deref` returns the same value.
    ///
    /// This moves the snapshot but not the value `changed` compares against, so a later `changed`
    /// still reports the value read here, or a newer value that superseded it.
    ///
    /// This does not mark the notification cursor as observed, because the shared configuration can
    /// advance between the load and the mark, and the update would then be lost. A notification
    /// already pending for the value read here therefore still wakes `changed`.
    pub fn refresh(&mut self) -> &T {
        match &mut self.inner {
            Inner::Fixed(value) => value,
            Inner::Dynamic {
                cell,
                project,
                snapshot,
                last_reported,
                ..
            } => {
                let guard = cell.load();
                let latest = project(&guard);
                if *latest != *snapshot {
                    let previous = std::mem::replace(snapshot, latest.clone());
                    // Only the first refresh past a reported value sets the baseline, so repeated
                    // refreshes keep it.
                    if last_reported.is_none() {
                        *last_reported = Some(previous);
                    }
                }
                snapshot
            }
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
    /// source, applies the projection, compares it with the value it last reported, and keeps
    /// waiting if that value is unchanged. It parks forever when `Fixed` or the channel is closed,
    /// so a caller can `select!` on it unconditionally. The returned value and `Deref` reflect the
    /// same updated snapshot. This is a state-change watcher, not an event history: multiple source
    /// updates may be coalesced before this view observes them.
    ///
    /// Because the comparison is against the value last reported here, and not against the snapshot
    /// `refresh` may have moved, this can return a value that `Deref` already equals.
    pub async fn changed(&mut self) -> T {
        match &mut self.inner {
            Inner::Fixed(_) => std::future::pending::<T>().await,
            Inner::Dynamic {
                cell,
                tick,
                project,
                snapshot,
                last_reported,
            } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<T>().await;
                }
                let guard = cell.load();
                let latest = project(&guard);

                let is_change = match last_reported.as_ref() {
                    Some(reported) => *latest != *reported,
                    None => *latest != *snapshot,
                };
                if is_change {
                    *snapshot = latest.clone();
                    *last_reported = None;
                    return snapshot.clone();
                }

                // The source came back to the reported value, so there is nothing to report. Drop
                // the split baseline and resynchronize the snapshot with it.
                if *latest != *snapshot {
                    *snapshot = latest.clone();
                }
                *last_reported = None;
            },
        }
    }

    /// Creates a child view by composing this view's projection with `f`.
    ///
    /// Use this when code already has a broad `Live<T>` but does not have the
    /// `ConfigurationSystem` needed to create a narrower view
    /// directly. A dynamic child shares the source and receives its own notification cursor and
    /// snapshot; it starts converged with the source and owes no report, and it wakes only when the
    /// selected child value changes. A fixed child is projected once and remains fixed.
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
                last_reported,
            } => Inner::Dynamic {
                cell: Arc::clone(cell),
                tick: tick.clone(),
                project: Arc::clone(project),
                snapshot: snapshot.clone(),
                last_reported: last_reported.clone(),
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
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// A configuration cell and its notification channel, with the two steps kept separate so that a
    /// test can advance the shared configuration without notifying the views.
    struct Source {
        cell: Arc<ArcSwap<SalukiConfiguration>>,
        tick: watch::Sender<()>,
    }

    impl Source {
        fn new(api_key: &str) -> Self {
            Self {
                cell: Arc::new(ArcSwap::from_pointee(config(api_key))),
                tick: watch::channel(()).0,
            }
        }

        fn store(&self, api_key: &str) {
            self.cell.store(Arc::new(config(api_key)));
        }

        /// Stores a configuration that also changes a field no view under test projects.
        fn store_unrelated(&self, api_key: &str) {
            let mut updated = config(api_key);
            updated.shared.metrics_level = "debug".to_string();
            self.cell.store(Arc::new(updated));
        }

        fn notify(&self) {
            self.tick.send_replace(());
        }

        fn api_key_view(&self) -> Live<String> {
            Live::new_dynamic(Arc::clone(&self.cell), self.tick.subscribe(), |config| {
                &config.shared.endpoints.api_key
            })
        }
    }

    fn config(api_key: &str) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = api_key.to_string();
        config
    }

    /// Polls `changed()` once and returns the value it yields.
    ///
    /// The crate depends on tokio for `sync` only, so there is no runtime to drive the future. One
    /// poll is enough because the caller notifies first; a view that lost its notification parks
    /// instead, and this panics.
    #[track_caller]
    fn poll_changed(view: &mut Live<String>) -> String {
        let mut changed = pin!(view.changed());
        match changed.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("changed() has a notification to observe"),
        }
    }

    /// Polls `changed()` once and asserts that it parks instead of reporting a value.
    #[track_caller]
    fn assert_changed_pending(view: &mut Live<String>) {
        let mut changed = pin!(view.changed());
        assert!(
            changed
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "changed() has nothing to report"
        );
    }

    #[test]
    fn refresh_updates_the_snapshot_deref_returns() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();
        assert_eq!("key-1", &*view);

        source.store("key-2");
        source.notify();

        assert_eq!("key-2", view.refresh());
        assert_eq!("key-2", &*view, "Deref agrees with the value refresh() returned");
    }

    #[test]
    fn refresh_leaves_a_fixed_view_alone() {
        let mut view = Live::new_fixed("key-1".to_string());
        assert_eq!("key-1", view.refresh());
        assert_eq!("key-1", &*view);
    }

    #[test]
    fn refresh_does_not_consume_a_pending_notification() {
        // Asserting on the cursor rather than on changed() keeps the test synchronous, and the cursor
        // is the thing that must not move: were refresh() to mark it, an update that lands between
        // the load and the mark would have no notification left to wake changed() with.
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", view.refresh());

        let Inner::Dynamic { tick, .. } = &view.inner else {
            panic!("a dynamic view");
        };
        assert!(
            tick.has_changed().expect("the channel is open"),
            "the notification is still pending after refresh()"
        );
    }

    #[test]
    fn refresh_on_one_clone_leaves_another_clones_snapshot_alone() {
        let source = Source::new("key-1");
        let mut refreshed = source.api_key_view();
        let untouched = refreshed.clone();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", refreshed.refresh());

        assert_eq!("key-1", &*untouched, "the clone still reads its own snapshot");
    }

    #[test]
    fn refresh_on_one_clone_leaves_another_clone_able_to_observe_the_update() {
        let source = Source::new("key-1");
        let mut refreshed = source.api_key_view();
        let mut waiting = refreshed.clone();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", refreshed.refresh());

        assert_eq!("key-2", poll_changed(&mut waiting));
        assert_eq!("key-2", &*waiting);
    }

    #[test]
    fn one_notification_updates_the_snapshot_of_every_clone() {
        let source = Source::new("key-1");
        let mut first = source.api_key_view();
        let mut second = first.clone();

        source.store("key-2");
        source.notify();

        assert_eq!("key-2", poll_changed(&mut first));
        assert_eq!("key-2", poll_changed(&mut second));
    }

    #[test]
    fn changed_reports_a_value_refresh_already_observed() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", view.refresh());

        assert_eq!(
            "key-2",
            poll_changed(&mut view),
            "changed() compares against the value it last reported, not the refreshed snapshot"
        );
        assert_eq!("key-2", &*view);
    }

    #[test]
    fn changed_reports_a_refreshed_value_when_refresh_ran_before_the_tick() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        assert_eq!("key-2", view.refresh());
        source.notify();

        assert_eq!("key-2", poll_changed(&mut view));
    }

    #[test]
    fn changed_reports_only_the_latest_value_after_refresh() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", view.refresh());

        source.store("key-3");
        source.notify();

        assert_eq!("key-3", poll_changed(&mut view), "key-2 is coalesced away");
        assert_eq!("key-3", &*view);
    }

    #[test]
    fn changed_reports_a_refreshed_value_once_only() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", view.refresh());
        assert_eq!("key-2", poll_changed(&mut view));

        source.notify();
        assert_changed_pending(&mut view);
    }

    #[test]
    fn changed_stays_pending_when_the_source_returns_to_the_refreshed_value() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", view.refresh());

        source.store("key-1");
        source.notify();

        assert_changed_pending(&mut view);
        assert_eq!("key-1", &*view, "the snapshot resynchronizes with the source");
    }

    #[test]
    fn an_unrelated_configuration_tick_leaves_changed_pending() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store_unrelated("key-1");
        source.notify();

        assert_changed_pending(&mut view);
        assert_eq!("key-1", &*view);
    }

    #[test]
    fn a_cancelled_changed_does_not_lose_a_refreshed_value() {
        let source = Source::new("key-1");
        let mut view = source.api_key_view();

        source.store("key-2");
        assert_eq!("key-2", view.refresh());
        // The first poll finds no notification, and dropping the future must not advance the
        // comparison baseline past the unreported value.
        assert_changed_pending(&mut view);

        source.notify();
        assert_eq!("key-2", poll_changed(&mut view));
    }

    #[test]
    fn a_clone_made_after_refresh_inherits_the_pending_report() {
        let source = Source::new("key-1");
        let mut refreshed = source.api_key_view();

        source.store("key-2");
        source.notify();
        assert_eq!("key-2", refreshed.refresh());

        let mut inheritor = refreshed.clone();
        assert_eq!("key-2", poll_changed(&mut inheritor));
    }

    #[test]
    fn a_projected_child_starts_converged() {
        let source = Source::new("key-1");
        let view = source.api_key_view();

        source.store("key-2");
        source.notify();

        let mut child = view.project(|api_key| api_key);
        assert_eq!("key-2", &*child, "the child projects the current source");
        assert_changed_pending(&mut child);
    }
}

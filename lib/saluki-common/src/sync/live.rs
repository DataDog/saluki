//! Live views of a shared value.
//!
//! [`channel`] creates a [`LivePublisher`] and a [`LiveSource`] that share one root value. The publisher replaces the
//! root and notifies views. The source reads the root and creates [`Live`] views of parts of it. Each view caches a
//! snapshot of its selected value and refreshes it only when [`Live::changed`] processes a notification.
//!
//! ```
//! use saluki_common::sync::live;
//!
//! struct Root {
//!     enabled: bool,
//! }
//!
//! let (mut publisher, source) = live::channel(Root { enabled: false });
//! let enabled = source.live(|root| &root.enabled);
//! publisher.publish(Root { enabled: true });
//!
//! // An existing view keeps its snapshot until `changed` processes the update; a new view reads the latest root.
//! assert!(!*enabled);
//! assert!(*source.live(|root| &root.enabled));
//! ```

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::stream::{self, Stream};
use tokio::sync::watch;

/// Borrowed access to the source's projected value, hiding the source type.
///
/// Calling a source must call its callback exactly once.
// Borrowed projections let nested views clone only their selected `T`, not the shared source.
type Source<T> = Arc<dyn Fn(&mut dyn FnMut(&T)) + Send + Sync>;

/// Creates a publisher and a source that share `initial` as their root value.
pub fn channel<S>(initial: S) -> (LivePublisher<S>, LiveSource<S>)
where
    S: Send + Sync + 'static,
{
    let cell = Arc::new(ArcSwap::from_pointee(initial));
    let (tick, tick_rx) = watch::channel(());
    let publisher = LivePublisher {
        cell: Arc::clone(&cell),
        tick,
    };
    (publisher, LiveSource { cell, tick: tick_rx })
}

/// The handle that replaces a channel's root value.
///
/// Each channel has one publisher: it isn't `Clone`, and publishing takes `&mut self`. Dropping it closes the channel.
/// Neither of these compiles:
///
/// ```compile_fail,E0599
/// let (publisher, _source) = saluki_common::sync::live::channel(0_u32);
/// let _second = publisher.clone();
/// ```
///
/// ```compile_fail,E0596
/// fn publish(publisher: &saluki_common::sync::live::LivePublisher<u32>) {
///     publisher.publish(1);
/// }
/// ```
pub struct LivePublisher<S> {
    cell: Arc<ArcSwap<S>>,
    tick: watch::Sender<()>,
}

impl<S> LivePublisher<S> {
    /// Replaces the root value with `next` and notifies the views.
    ///
    /// Validate `next` before publishing it. Views created after this call start from `next`; existing views see it
    /// once [`Live::changed`] processes the notification.
    ///
    /// # Panics
    ///
    /// If this releases the last reference to the previous root, a panic in its destructor propagates. The views are
    /// notified of `next` first.
    pub fn publish(&mut self, next: S) {
        let next = Arc::new(next);
        // Not `ArcSwap::store`: it drops the previous root, whose destructor could panic before the notification.
        let previous = self.cell.swap(next);
        self.tick.send_replace(());
        drop(previous);
    }
}

/// A read-only handle to a channel's root value, used to load it and to create views of it.
///
/// A source doesn't keep the channel open. After the publisher drops, it still reads the final root.
pub struct LiveSource<S> {
    cell: Arc<ArcSwap<S>>,

    // Never polled: each new view takes a clone.
    tick: watch::Receiver<()>,
}

impl<S> LiveSource<S>
where
    S: Send + Sync + 'static,
{
    /// Returns the current root value.
    pub fn load(&self) -> Arc<S> {
        self.cell.load_full()
    }

    /// Creates a view of one projection of the root value, starting from the current root.
    pub fn live<T>(&self, project: impl for<'a> Fn(&'a S) -> &'a T + Send + Sync + 'static) -> Live<T>
    where
        T: Clone + PartialEq + 'static,
    {
        // Mark before reading the snapshot, so a concurrent publish is either in the snapshot or still unseen.
        let mut tick = self.tick.clone();
        tick.mark_unchanged();

        let cell = Arc::clone(&self.cell);
        let source: Source<T> = Arc::new(move |f: &mut dyn FnMut(&T)| {
            let guard = cell.load();
            f(project(&guard));
        });
        Live::from_source(source, tick)
    }
}

impl<S> Clone for LiveSource<S> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
            tick: self.tick.clone(),
        }
    }
}

/// A typed view of one projection of a shared value.
///
/// Consumers use the same API for fixed values and for dynamic views created by [`LiveSource::live`]. `Deref` returns
/// this view's cached snapshot, not a fresh source read. Only [`changed`](Self::changed) refreshes the snapshot; source
/// updates alone do not.
///
/// Cloning copies the snapshot and notification cursor. Each view then processes updates independently.
pub struct Live<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Fixed(T),
    Dynamic {
        source: Source<T>,

        // Notifications carry no value and may include changes outside this view's projection.
        tick: watch::Receiver<()>,

        snapshot: T,
    },
}

impl<T: Clone + PartialEq + 'static> Live<T> {
    fn from_source(source: Source<T>, tick: watch::Receiver<()>) -> Self {
        let mut snapshot = None;
        source(&mut |value| snapshot = Some(value.clone()));
        let snapshot = snapshot.expect("a source always passes its current value to the callback");
        Self {
            inner: Inner::Dynamic { source, tick, snapshot },
        }
    }

    /// Creates a view with a value that never changes.
    pub fn new_fixed(value: T) -> Self {
        Self {
            inner: Inner::Fixed(value),
        }
    }

    /// Waits until the projected value differs from this view's snapshot, then updates the snapshot and returns it.
    ///
    /// Notifications trigger a source read; equal projected values leave the call pending. The returned value matches
    /// the snapshot exposed by `Deref`. Updates may coalesce, so this is not an event history.
    ///
    /// Fixed views stay pending forever. When the publisher drops, dynamic views process any unseen notification, then
    /// stay pending forever. Callers can therefore use `select!` without handling closure.
    pub async fn changed(&mut self) -> T {
        match &mut self.inner {
            Inner::Fixed(_) => std::future::pending::<T>().await,
            Inner::Dynamic { source, tick, snapshot } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                let mut changed = false;
                source(&mut |latest| {
                    if *latest != *snapshot {
                        *snapshot = latest.clone();
                        changed = true;
                    }
                });
                if changed {
                    return snapshot.clone();
                }
            },
        }
    }

    /// Converts this view into a stream of its changes.
    ///
    /// Each item comes from [`changed`](Self::changed): the stream skips the current snapshot but can yield a change
    /// pending before conversion. Updates may coalesce. The stream never ends, even for fixed views or after the
    /// publisher drops.
    pub fn into_changes(self) -> impl Stream<Item = T> + Send + 'static
    where
        T: Send,
    {
        stream::unfold(self, |mut live| async move {
            let value = live.changed().await;
            Some((value, live))
        })
    }

    /// Creates a child view by composing this view's projection with `f`.
    ///
    /// A dynamic child shares the source and copies this view's notification cursor, but processes updates
    /// independently. Its initial snapshot reads the source, not this view's snapshot, so it may be newer.
    /// A fixed child is projected once and remains fixed.
    pub fn project<U>(&self, f: impl for<'a> Fn(&'a T) -> &'a U + Send + Sync + 'static) -> Live<U>
    where
        U: Clone + PartialEq + 'static,
    {
        match &self.inner {
            Inner::Fixed(value) => Live::new_fixed(f(value).clone()),
            Inner::Dynamic { source, tick, .. } => {
                let parent = Arc::clone(source);
                let source: Source<U> = Arc::new(move |g: &mut dyn FnMut(&U)| parent(&mut |value| g(f(value))));
                Live::from_source(source, tick.clone())
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
            Inner::Dynamic { source, tick, snapshot } => Inner::Dynamic {
                source: Arc::clone(source),
                tick: tick.clone(),
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
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::{channel, Live, LivePublisher, LiveSource};

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Root {
        selected: Selected,
        unrelated: u32,
    }

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Selected {
        value: u32,
        sibling: u32,
    }

    /// A channel whose tests publish edits of the current root.
    struct Shared {
        publisher: LivePublisher<Root>,
        source: LiveSource<Root>,
    }

    impl Shared {
        fn new() -> Self {
            let (publisher, source) = channel(Root::default());
            Self { publisher, source }
        }

        fn live<T>(&self, project: impl for<'a> Fn(&'a Root) -> &'a T + Send + Sync + 'static) -> Live<T>
        where
            T: Clone + PartialEq + 'static,
        {
            self.source.live(project)
        }

        fn update(&mut self, f: impl FnOnce(&mut Root)) {
            let mut next = Root::clone(&self.source.load());
            f(&mut next);
            self.publisher.publish(next);
        }
    }

    fn assert_send_sync<S>(_: &S)
    where
        S: Send + Sync,
    {
    }

    #[test]
    fn fixed_view_never_changes() {
        let mut view = Live::new_fixed(true);
        let mut changed = spawn(view.changed());
        assert_pending!(changed.poll());
        drop(changed);
        assert!(*view);
    }

    #[test]
    fn fixed_live_stream_never_yields_or_ends() {
        let mut values = spawn(Live::new_fixed(true).into_changes());
        assert_pending!(values.poll_next());
    }

    #[test]
    fn fixed_child_holds_the_projected_value_and_never_changes() {
        let parent = Live::new_fixed(Selected { value: 1, sibling: 2 });
        let mut child = parent.project(|selected| &selected.sibling);
        assert_eq!(*child, 2);
        assert_pending!(spawn(child.changed()).poll());
    }

    #[test]
    fn dynamic_view_keeps_its_snapshot_until_changed_processes_the_update() {
        let mut shared = Shared::new();
        let mut view = shared.live(|root| &root.selected.value);

        shared.update(|root| root.selected.value = 1);
        assert_eq!(*view, 0);

        assert_eq!(assert_ready!(spawn(view.changed()).poll()), 1);
        assert_eq!(*view, 1);
    }

    #[test]
    fn project_follows_the_selected_child_while_the_parent_snapshot_stays_unchanged() {
        let mut shared = Shared::new();
        let parent = shared.live(|root| root);
        let mut child = parent
            .project(|root| &root.selected)
            .project(|selected| &selected.value);

        shared.update(|root| root.selected.value = 1);

        assert_eq!(assert_ready!(spawn(child.changed()).poll()), 1);
        assert_eq!(*child, 1);
        assert_eq!(*parent, Root::default());
    }

    #[test]
    fn project_reads_the_source_for_the_child_initial_snapshot() {
        let mut shared = Shared::new();
        let parent = shared.live(|root| &root.selected);

        shared.update(|root| root.selected.value = 1);
        let child = parent.project(|selected| &selected.value);

        assert_eq!(*child, 1);
        assert_eq!(parent.value, 0);
    }

    #[test]
    fn projected_view_ignores_unrelated_and_unchanged_values() {
        let mut shared = Shared::new();
        let mut child = shared.live(|root| &root.selected).project(|selected| &selected.value);
        let mut changed = spawn(child.changed());
        assert_pending!(changed.poll());

        shared.update(|root| root.unrelated = 1);
        assert!(changed.is_woken());
        assert_pending!(changed.poll());

        shared.update(|root| root.selected.sibling = 1);
        assert!(changed.is_woken());
        assert_pending!(changed.poll());

        shared.update(|root| root.selected.value = 0);
        assert!(changed.is_woken());
        assert_pending!(changed.poll());

        shared.update(|root| root.selected.value = 2);
        assert_eq!(assert_ready!(changed.poll()), 2);
    }

    #[test]
    fn projected_stream_ignores_unrelated_and_unchanged_values() {
        let mut shared = Shared::new();
        let child = shared.live(|root| &root.selected).project(|selected| &selected.value);
        let mut values = spawn(child.into_changes());

        shared.update(|root| root.unrelated = 1);
        assert_pending!(values.poll_next());

        shared.update(|root| root.selected.value = 0);
        assert!(values.is_woken());
        assert_pending!(values.poll_next());

        shared.update(|root| root.selected.value = 3);
        assert_eq!(assert_ready!(values.poll_next()), Some(3));
        assert_pending!(values.poll_next());
    }

    #[test]
    fn stream_starts_with_the_next_change_not_the_current_snapshot() {
        let mut shared = Shared::new();
        shared.update(|root| root.selected.value = 1);
        let mut values = spawn(shared.live(|root| &root.selected.value).into_changes());

        assert_pending!(values.poll_next());

        shared.update(|root| root.selected.value = 2);
        assert_eq!(assert_ready!(values.poll_next()), Some(2));
    }

    #[test]
    fn stream_yields_an_update_pending_before_conversion() {
        let mut shared = Shared::new();
        let view = shared.live(|root| &root.selected.value);

        shared.update(|root| root.selected.value = 1);
        let mut values = spawn(view.into_changes());

        assert_eq!(assert_ready!(values.poll_next()), Some(1));
        assert_pending!(values.poll_next());
    }

    #[test]
    fn stream_coalesces_unprocessed_updates_to_the_latest_value() {
        let mut shared = Shared::new();
        let mut values = spawn(shared.live(|root| &root.selected.value).into_changes());

        shared.update(|root| root.selected.value = 1);
        shared.update(|root| root.selected.value = 2);

        assert_eq!(assert_ready!(values.poll_next()), Some(2));
        assert_pending!(values.poll_next());
    }

    #[test]
    fn dynamic_view_and_stream_park_after_the_publisher_drops() {
        let shared = Shared::new();
        let mut view = shared.live(|root| &root.selected.value);
        let mut values = spawn(shared.live(|root| &root.selected.value).into_changes());
        drop(shared);

        assert_pending!(spawn(view.changed()).poll());
        assert_pending!(values.poll_next());
        assert_eq!(*view, 0);
    }

    #[test]
    fn clone_has_its_own_snapshot_and_notification_cursor() {
        let mut shared = Shared::new();
        let mut original = shared.live(|root| &root.selected.value);
        let mut clone = original.clone();

        shared.update(|root| root.selected.value = 1);
        assert_eq!(assert_ready!(spawn(original.changed()).poll()), 1);
        assert_eq!(*clone, 0);

        assert_eq!(assert_ready!(spawn(clone.changed()).poll()), 1);
        assert_pending!(spawn(original.changed()).poll());
    }

    #[test]
    fn debug_shows_the_kind_and_value() {
        let shared = Shared::new();
        assert_eq!(format!("{:?}", Live::new_fixed(1)), "Live::Fixed(1)");
        assert_eq!(
            format!("{:?}", shared.live(|root| &root.selected.value)),
            "Live::Dynamic { snapshot: 0, .. }"
        );
    }

    #[test]
    fn stream_is_send_and_static_for_a_send_value_that_is_not_sync() {
        fn assert_send_static<S>(_: &S)
        where
            S: Send + 'static,
        {
        }

        assert_send_static(&Live::new_fixed(Cell::new(1)).into_changes());
    }

    #[test]
    fn source_clones_and_publishes_a_root_that_is_neither_clone_nor_partial_eq() {
        struct Opaque {
            value: u32,
        }

        let (mut publisher, source) = channel(Opaque { value: 0 });
        let clone = source.clone();
        assert_send_sync(&clone);
        let mut view = clone.live(|root| &root.value);

        publisher.publish(Opaque { value: 1 });

        assert_eq!(source.load().value, 1);
        assert_eq!(assert_ready!(spawn(view.changed()).poll()), 1);
    }

    #[test]
    fn publishing_without_views_or_receivers_replaces_the_root() {
        let mut shared = Shared::new();
        shared.update(|root| root.selected.value = 1);
        assert_eq!(shared.source.load().selected.value, 1);
        assert_eq!(*shared.live(|root| &root.selected.value), 1);

        // With no source left, the channel has no receivers.
        let Shared { mut publisher, source } = shared;
        drop(source);
        publisher.publish(Root::default());
    }

    #[test]
    fn a_view_created_after_a_publish_starts_from_it_without_reprocessing_its_notification() {
        let mut shared = Shared::new();
        shared.update(|root| root.selected.value = 1);

        let reads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reads);
        let mut view = shared.live(move |root| {
            counter.fetch_add(1, Ordering::Relaxed);
            &root.selected.value
        });
        assert_eq!(*view, 1);

        let mut changed = spawn(view.changed());
        assert_pending!(changed.poll());
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "only the initial snapshot reads the source"
        );

        shared.update(|root| root.selected.value = 2);
        assert_eq!(assert_ready!(changed.poll()), 2);
    }

    #[test]
    fn a_panicking_old_root_destructor_runs_after_the_new_root_is_notified() {
        struct PanicsOnDrop {
            value: u32,
            panics: bool,
        }

        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                if self.panics {
                    panic!("old root destructor");
                }
            }
        }

        let (mut publisher, source) = channel(PanicsOnDrop { value: 0, panics: true });
        let mut view = source.live(|root| &root.value);

        let published = catch_unwind(AssertUnwindSafe(|| {
            publisher.publish(PanicsOnDrop {
                value: 1,
                panics: false,
            })
        }));

        assert!(published.is_err(), "publishing drops the old root");
        assert_eq!(assert_ready!(spawn(view.changed()).poll()), 1);
        assert_eq!(source.load().value, 1);
    }

    #[test]
    fn after_the_publisher_drops_views_process_an_unseen_update_then_stay_pending() {
        let mut shared = Shared::new();
        let mut view = shared.live(|root| &root.selected.value);
        let mut values = spawn(shared.live(|root| &root.selected.value).into_changes());
        shared.update(|root| root.selected.value = 1);
        let Shared { publisher, source } = shared;
        drop(publisher);

        assert_eq!(assert_ready!(spawn(view.changed()).poll()), 1);
        assert_pending!(spawn(view.changed()).poll());
        assert_eq!(assert_ready!(values.poll_next()), Some(1));
        assert_pending!(values.poll_next());

        let mut late = source.live(|root| &root.selected.value);
        assert_eq!(*late, 1);
        assert_pending!(spawn(late.changed()).poll());
        assert_eq!(source.load().selected.value, 1);
    }
}

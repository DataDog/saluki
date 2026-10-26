//! Live views of a shared value.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::stream::{self, Stream};
use tokio::sync::watch;

/// Borrowed access to the source's projected value, hiding the source type.
///
/// Implementations must call `f` exactly once per `with` call.
trait Source<T>: Send + Sync {
    fn with(&self, f: &mut dyn FnMut(&T));
}

// Borrowed projections let nested views clone only their selected `T`, not the shared source.
struct Root<S, P> {
    cell: Arc<ArcSwap<S>>,
    project: P,
}

impl<S, T, P> Source<T> for Root<S, P>
where
    S: Send + Sync,
    P: for<'a> Fn(&'a S) -> &'a T + Send + Sync,
{
    fn with(&self, f: &mut dyn FnMut(&T)) {
        let guard = self.cell.load();
        f((self.project)(&guard));
    }
}

struct Child<T, F> {
    parent: Arc<dyn Source<T>>,
    project: F,
}

impl<T, U, F> Source<U> for Child<T, F>
where
    T: 'static,
    F: for<'a> Fn(&'a T) -> &'a U + Send + Sync,
{
    fn with(&self, f: &mut dyn FnMut(&U)) {
        self.parent.with(&mut |value| f((self.project)(value)));
    }
}

/// A typed view of one projection of a shared value.
///
/// Consumers use the same API for fixed values and dynamic sources. `Deref` returns this view's cached snapshot,
/// not a fresh source read. Only [`changed`](Self::changed) refreshes the snapshot; source updates alone do not.
///
/// Cloning copies the snapshot and notification cursor. Each view then processes updates independently.
pub struct Live<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Fixed(T),
    Dynamic {
        source: Arc<dyn Source<T>>,

        // Notifications carry no value and may include changes outside this view's projection.
        tick: watch::Receiver<()>,

        snapshot: T,
    },
}

impl<T: Clone + PartialEq + 'static> Live<T> {
    /// Creates a view with an initial snapshot projected from `cell`.
    ///
    /// The source owner must store each new value in `cell` before notifying the channel behind `tick`.
    pub fn new_dynamic<S>(
        cell: Arc<ArcSwap<S>>, tick: watch::Receiver<()>,
        project: impl for<'a> Fn(&'a S) -> &'a T + Send + Sync + 'static,
    ) -> Self
    where
        S: Send + Sync + 'static,
    {
        Self::from_source(Arc::new(Root { cell, project }), tick)
    }

    fn from_source(source: Arc<dyn Source<T>>, tick: watch::Receiver<()>) -> Self {
        let mut snapshot = None;
        source.with(&mut |value| snapshot = Some(value.clone()));
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
    /// Fixed views stay pending forever. When the channel closes, dynamic views process any unseen notification, then
    /// stay pending forever. Callers can therefore use `select!` without handling closure.
    pub async fn changed(&mut self) -> T {
        match &mut self.inner {
            Inner::Fixed(_) => std::future::pending::<T>().await,
            Inner::Dynamic { source, tick, snapshot } => loop {
                if tick.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                let mut changed = false;
                source.with(&mut |latest| {
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
    /// notification channel closes.
    pub fn into_stream(self) -> impl Stream<Item = T> + Send + 'static
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
            Inner::Dynamic { source, tick, .. } => Live::from_source(
                Arc::new(Child {
                    parent: Arc::clone(source),
                    project: f,
                }),
                tick.clone(),
            ),
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
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use tokio::sync::watch;
    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::Live;

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

    /// Publishes updates using the store-before-notify contract.
    struct Shared {
        cell: Arc<ArcSwap<Root>>,
        tick: watch::Sender<()>,
    }

    impl Shared {
        fn new() -> Self {
            Self {
                cell: Arc::new(ArcSwap::from_pointee(Root::default())),
                tick: watch::channel(()).0,
            }
        }

        fn live<T>(&self, project: impl for<'a> Fn(&'a Root) -> &'a T + Send + Sync + 'static) -> Live<T>
        where
            T: Clone + PartialEq + 'static,
        {
            Live::new_dynamic(Arc::clone(&self.cell), self.tick.subscribe(), project)
        }

        fn update(&self, f: impl FnOnce(&mut Root)) {
            let mut next = Root::clone(&self.cell.load());
            f(&mut next);
            self.cell.store(Arc::new(next));
            self.tick.send_replace(());
        }
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
        let mut values = spawn(Live::new_fixed(true).into_stream());
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
        let shared = Shared::new();
        let mut view = shared.live(|root| &root.selected.value);

        shared.update(|root| root.selected.value = 1);
        assert_eq!(*view, 0);

        assert_eq!(assert_ready!(spawn(view.changed()).poll()), 1);
        assert_eq!(*view, 1);
    }

    #[test]
    fn project_follows_the_selected_child_while_the_parent_snapshot_stays_unchanged() {
        let shared = Shared::new();
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
        let shared = Shared::new();
        let parent = shared.live(|root| &root.selected);

        shared.update(|root| root.selected.value = 1);
        let child = parent.project(|selected| &selected.value);

        assert_eq!(*child, 1);
        assert_eq!(parent.value, 0);
    }

    #[test]
    fn projected_view_ignores_unrelated_and_unchanged_values() {
        let shared = Shared::new();
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
        let shared = Shared::new();
        let child = shared.live(|root| &root.selected).project(|selected| &selected.value);
        let mut values = spawn(child.into_stream());

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
        let shared = Shared::new();
        shared.update(|root| root.selected.value = 1);
        let mut values = spawn(shared.live(|root| &root.selected.value).into_stream());

        assert_pending!(values.poll_next());

        shared.update(|root| root.selected.value = 2);
        assert_eq!(assert_ready!(values.poll_next()), Some(2));
    }

    #[test]
    fn stream_yields_an_update_pending_before_conversion() {
        let shared = Shared::new();
        let view = shared.live(|root| &root.selected.value);

        shared.update(|root| root.selected.value = 1);
        let mut values = spawn(view.into_stream());

        assert_eq!(assert_ready!(values.poll_next()), Some(1));
        assert_pending!(values.poll_next());
    }

    #[test]
    fn stream_coalesces_unprocessed_updates_to_the_latest_value() {
        let shared = Shared::new();
        let mut values = spawn(shared.live(|root| &root.selected.value).into_stream());

        shared.update(|root| root.selected.value = 1);
        shared.update(|root| root.selected.value = 2);

        assert_eq!(assert_ready!(values.poll_next()), Some(2));
        assert_pending!(values.poll_next());
    }

    #[test]
    fn dynamic_view_and_stream_park_after_the_notification_channel_closes() {
        let shared = Shared::new();
        let mut view = shared.live(|root| &root.selected.value);
        let mut values = spawn(shared.live(|root| &root.selected.value).into_stream());
        drop(shared);

        assert_pending!(spawn(view.changed()).poll());
        assert_pending!(values.poll_next());
        assert_eq!(*view, 0);
    }

    #[test]
    fn clone_has_its_own_snapshot_and_notification_cursor() {
        let shared = Shared::new();
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

        assert_send_static(&Live::new_fixed(Cell::new(1)).into_stream());
    }
}

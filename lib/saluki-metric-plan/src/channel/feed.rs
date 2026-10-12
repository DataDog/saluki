//! The in-process channel between a transport (such as the local stub) and the installer.
//!
//! Both directions are [`watch`] channels, which keep only the latest value: the installer needs the latest complete
//! snapshot, never the ones in between, and the transport sends the latest acknowledgments with each poll. Snapshots
//! travel in one slot per [product](ArtifactProduct), so a snapshot of one product published right after one of
//! another never replaces it.

use std::{
    future::{poll_fn, Future as _},
    pin::pin,
    sync::Arc,
    task::Poll,
};

use tokio::sync::watch;

use super::{
    snapshot::{ArtifactProduct, ArtifactSnapshot},
    state::ApplyStates,
};

/// The slot of each product, in [`ArtifactProduct::ALL`] order.
type Slots<T> = [T; ArtifactProduct::ALL.len()];

/// Creates the channel between a transport and the installer, returning the transport's end and the installer's end.
pub fn artifact_channel() -> (TransportEnd, InstallerEnd) {
    let (policy, policy_feed) = watch::channel(None);
    let (resources, resources_feed) = watch::channel(None);
    let (states, reported) = watch::channel(Arc::new(ApplyStates::default()));
    (
        TransportEnd {
            publisher: ArtifactPublisher {
                snapshots: [policy, resources],
            },
            apply_states: ApplyStateReceiver { states: reported },
        },
        InstallerEnd {
            feed: ArtifactFeed {
                snapshots: [policy_feed, resources_feed],
                closed: [false; ArtifactProduct::ALL.len()],
            },
            reporter: ApplyStateReporter { states },
        },
    )
}

/// The transport's end of the channel: it publishes snapshots and reads acknowledgments.
#[derive(Debug)]
pub struct TransportEnd {
    /// Publishes each snapshot received.
    pub publisher: ArtifactPublisher,

    /// Reads the latest acknowledgments and status, to send back.
    pub apply_states: ApplyStateReceiver,
}

/// The installer's end of the channel: it reads snapshots and reports acknowledgments.
#[derive(Debug)]
pub struct InstallerEnd {
    /// Delivers each new snapshot.
    pub feed: ArtifactFeed,

    /// Reports the latest acknowledgments and status.
    pub reporter: ApplyStateReporter,
}

/// Publishes the snapshots a transport receives to the installer.
#[derive(Debug)]
pub struct ArtifactPublisher {
    snapshots: Slots<watch::Sender<Option<Arc<ArtifactSnapshot>>>>,
}

impl ArtifactPublisher {
    /// Publishes a snapshot received from the channel, replacing the previous snapshot of its product.
    ///
    /// A snapshot equal to its product's current one wakes nobody, so a transport can publish every poll's answer, one
    /// snapshot per product. A transport failure publishes nothing: the current snapshots stay in effect.
    ///
    /// Returns `true` if the snapshot differs from its product's current one.
    pub fn publish(&self, snapshot: ArtifactSnapshot) -> bool {
        self.snapshots[snapshot.product.index()].send_if_modified(|current| {
            if current.as_deref() == Some(&snapshot) {
                false
            } else {
                *current = Some(Arc::new(snapshot));
                true
            }
        })
    }
}

/// Delivers the snapshots a transport publishes to the installer, one slot per product.
#[derive(Debug)]
pub struct ArtifactFeed {
    snapshots: Slots<watch::Receiver<Option<Arc<ArtifactSnapshot>>>>,
    /// Whether each slot's publisher is gone and its last snapshot was returned.
    closed: Slots<bool>,
}

impl ArtifactFeed {
    /// Returns the latest snapshot of `product`, or `None` if none has been delivered since the process started, in
    /// which case nothing of the product is in effect: for policy, the default program runs.
    pub fn latest(&self, product: ArtifactProduct) -> Option<Arc<ArtifactSnapshot>> {
        self.snapshots[product.index()].borrow().clone()
    }

    /// Waits for a snapshot newer than the last one this feed returned for its product, and returns it with its
    /// product.
    ///
    /// When several products changed, the first in [`ArtifactProduct::ALL`] order is returned first, and the next call
    /// returns the next at once. Returns `None` once the publisher is gone and every snapshot has been returned:
    /// nothing will be delivered any more, and the last snapshots stay in effect.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe: a snapshot is marked returned only when the call returns it, so an installer can
    /// wait on it in a `select!` beside its other events without losing one.
    pub async fn changed(&mut self) -> Option<(ArtifactProduct, Arc<ArtifactSnapshot>)> {
        loop {
            let (index, changed) = {
                let [policy, resources] = &mut self.snapshots;
                let open = self.closed.map(|closed| !closed);
                let mut policy = pin!(policy.changed());
                let mut resources = pin!(resources.changed());
                // Polled in product order, so the first product that changed is returned first. A slot whose
                // publisher is gone is not waited on again.
                let next = poll_fn(|cx| {
                    if open[0] {
                        if let Poll::Ready(changed) = policy.as_mut().poll(cx) {
                            return Poll::Ready(Some((0, changed.is_ok())));
                        }
                    }
                    if open[1] {
                        if let Poll::Ready(changed) = resources.as_mut().poll(cx) {
                            return Poll::Ready(Some((1, changed.is_ok())));
                        }
                    }
                    if open.iter().any(|open| *open) {
                        Poll::Pending
                    } else {
                        Poll::Ready(None)
                    }
                });
                next.await?
            };
            if !changed {
                self.closed[index] = true;
                continue;
            }
            if let Some(snapshot) = self.snapshots[index].borrow_and_update().clone() {
                return Some((ArtifactProduct::ALL[index], snapshot));
            }
        }
    }

    /// Marks the latest snapshot of `product` as not returned yet, so that the next [`changed`][Self::changed] returns
    /// it at once if there is one.
    ///
    /// A restarted installer worker calls this, so that a snapshot whose handling a panic interrupted is handled again.
    pub fn mark_unseen(&mut self, product: ArtifactProduct) {
        let index = product.index();
        if self.snapshots[index].borrow().is_some() {
            self.snapshots[index].mark_changed();
            self.closed[index] = false;
        }
    }
}

/// Reports the installer's acknowledgments and status to the transport.
#[derive(Debug)]
pub struct ApplyStateReporter {
    states: watch::Sender<Arc<ApplyStates>>,
}

impl ApplyStateReporter {
    /// Reports the latest acknowledgments and status, replacing the previous ones.
    ///
    /// Returns `true` if they differ from the previous ones.
    pub fn report(&self, states: ApplyStates) -> bool {
        self.states.send_if_modified(|current| {
            if **current == states {
                false
            } else {
                *current = Arc::new(states);
                true
            }
        })
    }
}

/// Reads the installer's latest acknowledgments and status, for the transport to send back.
#[derive(Clone, Debug)]
pub struct ApplyStateReceiver {
    states: watch::Receiver<Arc<ApplyStates>>,
}

impl ApplyStateReceiver {
    /// Returns the latest acknowledgments and status: empty until the installer first reports.
    pub fn latest(&self) -> Arc<ApplyStates> {
        Arc::clone(&self.states.borrow())
    }

    /// Waits until the installer reports something new. Returns `false` once the installer's reporter is gone.
    pub async fn changed(&mut self) -> bool {
        self.states.changed().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use super::*;
    use crate::channel::{ArtifactConfig, ArtifactConfigId, ArtifactProduct};

    fn snapshot_of(product: ArtifactProduct, version: u64) -> ArtifactSnapshot {
        ArtifactSnapshot::new(
            product,
            vec![ArtifactConfig {
                id: ArtifactConfigId::new("demo"),
                version,
                sha256: ArtifactConfig::transport_hash("{}"),
                payload: "{}".to_string(),
            }],
        )
    }

    fn snapshot(version: u64) -> ArtifactSnapshot {
        snapshot_of(ArtifactProduct::Policy, version)
    }

    fn run<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    async fn next(feed: &mut ArtifactFeed) -> Option<Arc<ArtifactSnapshot>> {
        next_with_product(feed).await.map(|(product, snapshot)| {
            assert_eq!(product, snapshot.product, "a snapshot is returned with its own product");
            snapshot
        })
    }

    async fn next_with_product(feed: &mut ArtifactFeed) -> Option<(ArtifactProduct, Arc<ArtifactSnapshot>)> {
        tokio::time::timeout(Duration::from_secs(1), feed.changed())
            .await
            .expect("the feed delivers within a second")
    }

    #[test]
    fn the_feed_delivers_the_latest_snapshot_and_skips_repeats() {
        run(async {
            let (transport, mut installer) = artifact_channel();
            assert_eq!(installer.feed.latest(ArtifactProduct::Policy), None);

            assert!(transport.publisher.publish(snapshot(1)));
            assert!(transport.publisher.publish(snapshot(2)));
            // Only the latest snapshot is delivered.
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&snapshot(2)));
            // An equal snapshot wakes nobody.
            assert!(!transport.publisher.publish(snapshot(2)));
            assert!(transport
                .publisher
                .publish(ArtifactSnapshot::revoked(ArtifactProduct::Policy)));
            let revoked = next(&mut installer.feed).await.unwrap();
            assert!(revoked.revokes());

            // A restarted worker sees the latest snapshot again.
            installer.feed.mark_unseen(ArtifactProduct::Policy);
            assert_eq!(next(&mut installer.feed).await, Some(revoked));

            drop(transport);
            assert_eq!(next(&mut installer.feed).await, None);
            assert!(installer.feed.latest(ArtifactProduct::Policy).unwrap().revokes());
        });
    }

    #[test]
    fn a_snapshot_published_before_the_publisher_goes_away_is_still_delivered() {
        run(async {
            let (transport, mut installer) = artifact_channel();
            assert!(transport.publisher.publish(snapshot(1)));
            drop(transport);
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&snapshot(1)));
            assert_eq!(next(&mut installer.feed).await, None);

            // Nothing published: the feed ends without a snapshot, and the default program stays.
            let (transport, mut installer) = artifact_channel();
            drop(transport);
            assert_eq!(next(&mut installer.feed).await, None);
            installer.feed.mark_unseen(ArtifactProduct::Policy);
            assert_eq!(next(&mut installer.feed).await, None);
        });
    }

    #[test]
    fn each_product_has_its_own_slot_so_neither_snapshot_published_back_to_back_is_lost() {
        run(async {
            let (transport, mut installer) = artifact_channel();
            let resources = snapshot_of(ArtifactProduct::Resources, 1);
            assert!(transport.publisher.publish(snapshot(1)));
            assert!(transport.publisher.publish(resources.clone()));
            // A snapshot equal to its own product's current one is a repeat; the other product's does not count.
            assert!(!transport.publisher.publish(resources.clone()));

            // Both are delivered, in product order, each with its product.
            assert_eq!(
                next_with_product(&mut installer.feed).await,
                Some((ArtifactProduct::Policy, Arc::new(snapshot(1))))
            );
            assert_eq!(
                next_with_product(&mut installer.feed).await,
                Some((ArtifactProduct::Resources, Arc::new(resources.clone())))
            );
            assert_eq!(
                installer.feed.latest(ArtifactProduct::Policy).as_deref(),
                Some(&snapshot(1))
            );
            assert_eq!(
                installer.feed.latest(ArtifactProduct::Resources).as_deref(),
                Some(&resources)
            );

            // Re-arming one product re-delivers that product only.
            installer.feed.mark_unseen(ArtifactProduct::Resources);
            assert!(transport.publisher.publish(snapshot(2)));
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&snapshot(2)));
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&resources));

            // A product published after the publisher is gone cannot be, but the last snapshot of each stays.
            drop(transport);
            assert_eq!(next(&mut installer.feed).await, None);
            assert_eq!(
                installer.feed.latest(ArtifactProduct::Resources).as_deref(),
                Some(&resources)
            );
        });
    }

    #[test]
    fn a_cancelled_wait_loses_no_snapshot() {
        run(async {
            let (transport, mut installer) = artifact_channel();
            let mut context = Context::from_waker(Waker::noop());

            // A snapshot arrives before the wait is first polled, and the wait is cancelled after that one poll: either
            // the poll returned the snapshot, or the next wait does.
            assert!(transport.publisher.publish(snapshot(1)));
            let first = {
                let mut wait = pin!(installer.feed.changed());
                wait.as_mut().poll(&mut context)
            };
            match first {
                Poll::Ready(returned) => {
                    assert_eq!(returned.map(|(_, snapshot)| snapshot).as_deref(), Some(&snapshot(1)))
                }
                Poll::Pending => assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&snapshot(1))),
            }

            // A wait that is pending when a snapshot arrives, and is cancelled before it is polled again, as a
            // `select!` whose other branch is ready first cancels it: the next wait returns the snapshot.
            let first = {
                let mut wait = pin!(installer.feed.changed());
                let first = wait.as_mut().poll(&mut context);
                assert!(transport.publisher.publish(snapshot(2)));
                first
            };
            assert!(first.is_pending(), "nothing new had been published");
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&snapshot(2)));

            // The same holds for the second product's slot.
            let resources = snapshot_of(ArtifactProduct::Resources, 3);
            let first = {
                let mut wait = pin!(installer.feed.changed());
                let first = wait.as_mut().poll(&mut context);
                assert!(transport.publisher.publish(resources.clone()));
                first
            };
            assert!(first.is_pending(), "nothing new had been published");
            assert_eq!(next(&mut installer.feed).await.as_deref(), Some(&resources));
        });
    }

    #[test]
    fn acknowledgments_reach_the_transport_once_per_change() {
        run(async {
            let (mut transport, installer) = artifact_channel();
            assert_eq!(*transport.apply_states.latest(), ApplyStates::default());

            let mut states = ApplyStates::default();
            states.status.revision_high_water = Some(3);
            assert!(installer.reporter.report(states.clone()));
            assert!(!installer.reporter.report(states.clone()));
            assert!(transport.apply_states.changed().await);
            assert_eq!(*transport.apply_states.latest(), states);

            drop(installer);
            assert!(!transport.apply_states.changed().await);
        });
    }
}

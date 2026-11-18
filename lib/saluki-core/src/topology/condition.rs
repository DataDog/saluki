//! Conditional connections between components.
//!
//! A connection between two components can be made conditional on an [`EdgeCondition`]. Conditions are resolved in two
//! stages, much like constant folding in a compiler:
//!
//! - fixed conditions are resolved when the topology is built: a connection whose condition is always false is removed
//!   entirely, and any component left without inputs as a result is never built or spawned
//! - dynamic conditions are resolved at runtime: the connection exists, but items are only sent over it while its
//!   condition is true
//!
//! Dynamic conditions only control whether items are sent to the downstream component. The downstream component keeps
//! running while its input is inactive, and simply receives nothing. When a condition becomes true, items dispatched
//! from that point on are sent to the downstream component: items dispatched while the condition was false are never
//! replayed.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use futures::{
    stream::{self, BoxStream},
    Stream, StreamExt as _,
};
use tokio::sync::watch;
use tracing::debug;

use super::{ComponentId, OutputName};

/// A condition controlling whether a connection between two components is active.
pub struct EdgeCondition(pub(crate) ConditionKind);

pub(crate) enum ConditionKind {
    Fixed(bool),
    Dynamic {
        initial: bool,
        updates: BoxStream<'static, bool>,
    },
}

impl EdgeCondition {
    /// Creates a condition that never changes.
    ///
    /// A connection with a condition of `false` is removed when the topology is built.
    pub fn fixed(active: bool) -> Self {
        Self(ConditionKind::Fixed(active))
    }

    /// Creates a condition that starts as `initial` and changes to each value yielded by `updates`.
    ///
    /// If `updates` ends, the condition keeps its last value.
    pub fn dynamic<S>(initial: bool, updates: S) -> Self
    where
        S: Stream<Item = bool> + Send + 'static,
    {
        Self(ConditionKind::Dynamic {
            initial,
            updates: updates.boxed(),
        })
    }

    /// Creates a condition that follows the value of a watch channel.
    ///
    /// If the sender is dropped, the condition keeps its last value.
    pub fn from_watch(mut rx: watch::Receiver<bool>) -> Self {
        let initial = *rx.borrow_and_update();
        let updates = stream::unfold(rx, |mut rx| async move {
            rx.changed().await.ok()?;
            let value = *rx.borrow_and_update();
            Some((value, rx))
        });
        Self::dynamic(initial, updates)
    }
}

/// Identifies a connection from a component output to a downstream component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EdgeKey {
    pub upstream: ComponentId,
    pub output: OutputName,
    pub downstream: ComponentId,
}

impl EdgeKey {
    fn involves(&self, id: &ComponentId) -> bool {
        &self.upstream == id || &self.downstream == id
    }
}

/// The runtime state of a connection with a dynamic condition.
#[derive(Clone, Debug)]
pub(crate) struct EdgeGate(Arc<AtomicBool>);

impl EdgeGate {
    pub(crate) fn new(active: bool) -> Self {
        Self(Arc::new(AtomicBool::new(active)))
    }

    /// Returns `true` if items should be sent over the connection.
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        // Pairs with the release store in `set`, so that an item dispatched after a condition becomes true is sent.
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn set(&self, active: bool) {
        self.0.store(active, Ordering::Release);
    }
}

/// The connections in a topology whose conditions are resolved at runtime.
#[derive(Default)]
pub(crate) struct EdgeConditions {
    conditions: Vec<(EdgeKey, EdgeGate, BoxStream<'static, bool>)>,
}

impl EdgeConditions {
    /// Adds a connection with a dynamic condition.
    pub fn insert(&mut self, key: EdgeKey, initial: bool, updates: BoxStream<'static, bool>) {
        self.conditions.push((key, EdgeGate::new(initial), updates));
    }

    /// Removes any conditions on connections to or from the given component.
    pub fn remove_component(&mut self, id: &ComponentId) {
        self.conditions.retain(|(key, _, _)| !key.involves(id));
    }

    /// Returns the gate for the given connection, if its condition is dynamic.
    pub fn gate(&self, key: &EdgeKey) -> Option<EdgeGate> {
        self.conditions
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, gate, _)| gate.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }

    /// Applies condition updates to their gates until every condition's updates have ended.
    pub async fn run(self) {
        let mut updates = stream::select_all(
            self.conditions
                .into_iter()
                .map(|(key, gate, updates)| updates.map(move |active| (key.clone(), gate.clone(), active))),
        );

        while let Some((key, gate, active)) = updates.next().await {
            debug!(
                upstream = %key.upstream,
                output = %key.output,
                downstream = %key.downstream,
                active,
                "Updated connection condition."
            );
            gate.set(active);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(downstream: &str) -> EdgeKey {
        EdgeKey {
            upstream: ComponentId::try_from("up").unwrap(),
            output: OutputName::Default,
            downstream: ComponentId::try_from(downstream).unwrap(),
        }
    }

    #[tokio::test]
    async fn watch_condition_follows_sender() {
        let (tx, rx) = watch::channel(false);
        let ConditionKind::Dynamic { initial, updates } = EdgeCondition::from_watch(rx).0 else {
            panic!("expected dynamic condition");
        };

        let mut conditions = EdgeConditions::default();
        conditions.insert(key("down"), initial, updates);
        let gate = conditions.gate(&key("down")).unwrap();
        assert!(!gate.is_active());
        let driver = tokio::spawn(conditions.run());

        tx.send(true).unwrap();
        crate::test_support::wait_until("gate becomes active", || gate.is_active()).await;

        tx.send(false).unwrap();
        crate::test_support::wait_until("gate becomes inactive", || !gate.is_active()).await;

        // Dropping the sender ends the updates, and the gate keeps its last value.
        drop(tx);
        driver.await.unwrap();
        assert!(!gate.is_active());
    }

    #[test]
    fn removing_component_drops_its_conditions() {
        let mut conditions = EdgeConditions::default();
        conditions.insert(key("down"), true, stream::empty().boxed());
        assert!(conditions.gate(&key("down")).is_some());

        conditions.remove_component(&ComponentId::try_from("down").unwrap());
        assert!(conditions.gate(&key("down")).is_none());
        assert!(conditions.is_empty());
    }
}

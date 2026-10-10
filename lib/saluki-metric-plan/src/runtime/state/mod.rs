//! What the hosts of state owners share: the time rules every host follows, and the budgets state draws on.
//!
//! A state owner, such as aggregation, runs in an isolation slot: a region host of its own, which owns the state
//! owner's watermark and flush ticks. [`rules`] holds the time rules, pure functions that the region driver and the
//! reference interpreter both decide with. [`StateBudgets`] holds the budgets every lineage of every state owner of a
//! host draws its state from.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use saluki_core::execution::StateBudget;
use saluki_metric_kernels::aggregate::ContextBudget;

use crate::ir::NodeId;

pub mod rules;
pub(crate) mod slab;

/// The budgets of state of one host: one global budget, shared by every state owner, and one budget per state owner
/// node, shared by every lineage of the node.
///
/// The global budget's capacity is the boot plan's context limit of the host's aggregation, so that the contexts held
/// together, by every lineage of every state owner, never exceed what native aggregation holds; with one lineage this
/// is native aggregation's limit exactly. A node's budget is unlimited: it counts the node's contexts across its
/// lineages, which each batch's own context limit caps.
///
/// Clones share the same budgets. A host creates one at boot and hands it to every region that runs state owners and
/// to the installer, which builds their kernels with each node's [`ContextBudget`].
#[derive(Clone)]
pub struct StateBudgets {
    global: StateBudget,
    nodes: Arc<Mutex<BTreeMap<NodeId, StateBudget>>>,
}

impl StateBudgets {
    /// Creates the budgets of a host whose state owners hold at most `capacity` contexts together.
    pub fn new(capacity: u64) -> Self {
        Self::with_global(StateBudget::new(capacity))
    }

    /// Creates the budgets of a host whose state owners together hold as many contexts as their own limits allow: a
    /// host without aggregation in its boot plan, or a test.
    pub fn unlimited() -> Self {
        Self::with_global(StateBudget::unlimited())
    }

    fn with_global(global: StateBudget) -> Self {
        Self {
            global,
            nodes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Returns the global budget, which every state owner of the host draws on.
    pub fn global(&self) -> &StateBudget {
        &self.global
    }

    /// Returns the budget of a state owner node, shared by every lineage of the node, creating it the first time.
    pub fn node(&self, node: &NodeId) -> StateBudget {
        // A panic while the map was locked left it consistent: every insertion is a single call.
        let mut nodes = self.nodes.lock().unwrap_or_else(PoisonError::into_inner);
        nodes.entry(node.clone()).or_insert_with(StateBudget::unlimited).clone()
    }

    /// Returns the budgets a kernel of a state owner node draws on: the global budget and its node's.
    pub fn context_budget(&self, node: &NodeId) -> ContextBudget {
        ContextBudget {
            global: self.global.clone(),
            node: self.node(node),
        }
    }
}

impl Default for StateBudgets {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl fmt::Debug for StateBudgets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nodes = self.nodes.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("StateBudgets")
            .field("global", &self.global)
            .field("nodes", &*nodes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineages_of_one_node_share_its_budget_and_every_node_shares_the_global_one() {
        let budgets = StateBudgets::new(3);
        let agg: NodeId = "dsd_agg".parse().unwrap();
        let policy: NodeId = "policy/agg".parse().unwrap();

        let first = budgets.context_budget(&agg);
        let second = budgets.context_budget(&agg);
        let other = budgets.clone().context_budget(&policy);
        assert!(first.node.shares_with(&second.node));
        assert!(!first.node.shares_with(&other.node));
        assert!(first.global.shares_with(&other.global));
        assert!(first.global.shares_with(budgets.global()));
        assert!(first.node.is_unlimited());
        assert_eq!(budgets.global().capacity(), 3);

        // The global budget bounds every node together.
        assert!(first.global.try_acquire(2));
        assert!(other.global.try_acquire(1));
        assert!(!second.global.try_acquire(1));
        assert_eq!(budgets.global().used(), 3);

        assert!(StateBudgets::unlimited().global().is_unlimited());
        assert!(StateBudgets::default().global().is_unlimited());
    }
}

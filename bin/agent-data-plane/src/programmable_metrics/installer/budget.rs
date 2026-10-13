//! The resource budget: what the resources created after boot may take of the memory limit, and the ledger of the
//! credits the installer holds.
//!
//! Boot verification checks the firm bounds of the topology against the memory grant once (see
//! `saluki_app::accounting`). Everything a generation or a resource transaction adds later (leases, an idle host's
//! queue, declared recipients, new kernel instances) is admitted against what that check left: the budget's capacity
//! is the grant less the boot firm total, plus the boot generation's own resource bounds, which it then holds, so that
//! every release returns what was acquired.
//!
//! The installer core lists the credits each action adds, per resource ([`CreditKey`]); the installer acquires every
//! listed credit it does not hold yet before it performs the action, so an action repeated after a worker restart
//! acquires nothing twice, and releases a credit when the core says the creation it was acquired for ended. The
//! ledger lives in the installer's state, outside the worker, so it survives a restart with the core.
//!
//! Admission follows the boot verification's mode: `Strict` refuses a credit that does not fit, naming it, and
//! acquires nothing of the action's credits; `Permissive` warns and holds the credit without taking from the budget;
//! without a verified limit (`Disabled`, or no limit), nothing is counted.

use std::{collections::BTreeMap, num::NonZeroU8};

use saluki_app::accounting::{MemoryMode, VerifiedMemory};
use saluki_components::{
    encoders::encode_host::lease_settings_bound_bytes,
    forwarders::delivery::{recipient_queue_bound, RecipientPolicy},
};
use saluki_core::execution::StateBudget;
use saluki_metric_plan::{
    install::{CreditKey, ResourceBounds},
    ir::{HostBinding, HostId, OutputDecl, RecipientDecl, ResourceProvenance},
};
use tracing::warn;

use crate::programmable_metrics::resources::{
    params::decode_encoder,
    specs::{host_settings, recipient_policy},
};

/// The memory each resource the installer core admits after boot holds at most, as ADP's hosts bound them (plan
/// 4.12).
///
/// - A lease: its builders' split re-encode buffers and its shard channel's full buffers, from its output's encoder
///   settings, as the encode host bounds a boot lease.
/// - A native recipient: nothing of its own, since its host's bound counts one queue per host, as natively. A declared
///   recipient: its own queues, its policy's retry queue bound (at most its host's) and its high-priority queue.
/// - An idle host's queue, while it has any recipient: the bound a native forwarder declares for its queues, from the
///   host's settings.
///
/// A resource whose parameters cannot be read holds no credit: validation rejected its plan before it is prepared.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AdpResourceBounds;

impl ResourceBounds for AdpResourceBounds {
    fn lease(&self, _: &HostId, output: &OutputDecl) -> u64 {
        output
            .binding
            .as_ref()
            .and_then(|binding| decode_encoder(&binding.encoder).ok())
            .map_or(0, |settings| lease_settings_bound_bytes(&settings, NonZeroU8::MIN))
    }

    fn recipient(&self, recipient: &RecipientDecl, settings: Option<&HostBinding>) -> u64 {
        match recipient.provenance {
            ResourceProvenance::Native => 0,
            ResourceProvenance::Declared(_) => settings
                .and_then(|settings| host_settings(settings).ok())
                .map_or(0, |settings| {
                    recipient_queue_bound(&settings, &recipient_policy(&recipient.delivery))
                }),
        }
    }

    fn host_queue(&self, _: &HostId, settings: Option<&HostBinding>) -> u64 {
        settings
            .and_then(|settings| host_settings(settings).ok())
            .map_or(0, |settings| {
                recipient_queue_bound(&settings, &RecipientPolicy::default())
            })
    }
}

/// How much of the resource budget is granted and used, in bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetUsage {
    /// The budget's capacity, or `None` when nothing is counted.
    pub granted: Option<u64>,

    /// The bytes the credits held take from it.
    pub used: u64,
}

/// The resource budget and the ledger of the credits the installer holds.
#[derive(Debug)]
pub(super) struct ResourceBudget {
    /// The budget credits are acquired from, or `None` when nothing is counted.
    budget: Option<StateBudget>,
    mode: MemoryMode,
    /// Every credit held, with the bytes it took from the budget: zero for one held without being counted.
    held: BTreeMap<CreditKey, u64>,
}

impl ResourceBudget {
    /// Returns a budget that counts nothing: memory bounds were not verified, or not yet.
    pub(super) fn uncounted() -> Self {
        Self {
            budget: None,
            mode: MemoryMode::Disabled,
            held: BTreeMap::new(),
        }
    }

    /// Returns the budget of what boot verification left, `verified`, which holds the boot generation's own resource
    /// credits, `boot`, at once.
    pub(super) fn new(verified: Option<VerifiedMemory>, boot: &[(CreditKey, u64)]) -> Self {
        let Some(verified) = verified else {
            let mut budget = Self::uncounted();
            budget.acquire(boot).ok();
            return budget;
        };
        let boot_bytes = boot.iter().map(|(_, bytes)| *bytes).fold(0u64, u64::saturating_add);
        let mut budget = Self {
            budget: Some(StateBudget::new(verified.headroom_bytes().saturating_add(boot_bytes))),
            mode: verified.mode,
            held: BTreeMap::new(),
        };
        // The capacity covers the boot credits exactly, so acquiring them cannot fail.
        budget.acquire(boot).ok();
        budget
    }

    /// Acquires every credit of `credits` the ledger does not hold yet.
    ///
    /// # Errors
    ///
    /// In `Strict` mode, returns the first credit that does not fit, having acquired none of `credits`.
    pub(super) fn acquire(&mut self, credits: &[(CreditKey, u64)]) -> Result<(), CreditKey> {
        let mut acquired = Vec::new();
        for (key, bytes) in credits {
            if self.held.contains_key(key) || acquired.iter().any(|(held, _)| held == key) {
                continue;
            }
            let taken = match &self.budget {
                None => 0,
                Some(budget) if budget.try_acquire(*bytes) => *bytes,
                Some(budget) => match self.mode {
                    MemoryMode::Strict => {
                        for (_, taken) in acquired {
                            budget.release(taken);
                        }
                        return Err(key.clone());
                    }
                    MemoryMode::Permissive | MemoryMode::Disabled => {
                        warn!(
                            resource = %key,
                            bytes,
                            used = budget.used(),
                            capacity = budget.capacity(),
                            "A resource added after boot does not fit the memory limit's headroom. Memory limiting is \
                             best effort in permissive mode: the resource is created all the same."
                        );
                        0
                    }
                },
            };
            acquired.push((key.clone(), taken));
        }
        self.held.extend(acquired);
        Ok(())
    }

    /// Releases every credit of `credits` the ledger holds; the others are ignored.
    pub(super) fn release(&mut self, credits: &[CreditKey]) {
        for key in credits {
            if let Some(taken) = self.held.remove(key) {
                if let Some(budget) = &self.budget {
                    budget.release(taken);
                }
            }
        }
    }

    /// Returns how much of the budget is granted and used.
    pub(super) fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            granted: self.budget.as_ref().map(StateBudget::capacity),
            used: self.budget.as_ref().map_or(0, StateBudget::used),
        }
    }

    /// Returns the credits held, in key order.
    #[cfg(test)]
    pub(super) fn held(&self) -> Vec<CreditKey> {
        self.held.keys().cloned().collect()
    }

    /// Returns the credits held, in key order, each with the bytes it took from the budget.
    #[cfg(test)]
    pub(super) fn held_bytes(&self) -> Vec<(CreditKey, u64)> {
        self.held.iter().map(|(key, bytes)| (key.clone(), *bytes)).collect()
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::execution::{EntryId, GenerationId};
    use saluki_metric_plan::{compile::SlotId, ir::HostId};

    use super::*;

    fn lease(entry: u16) -> CreditKey {
        CreditKey::Lease(HostId::new("dd_out").unwrap(), EntryId::new(entry), 1)
    }

    fn verified(mode: MemoryMode, headroom: u64) -> Option<VerifiedMemory> {
        Some(VerifiedMemory {
            grant_bytes: 1_000 + headroom,
            firm_bytes: 1_000,
            mode,
        })
    }

    #[test]
    fn the_boot_credits_fill_their_own_share_of_the_capacity() {
        let boot = [
            (lease(0), 300),
            (CreditKey::HostQueue(HostId::new("dd_out").unwrap()), 200),
        ];
        let budget = ResourceBudget::new(verified(MemoryMode::Strict, 100), &boot);
        assert_eq!(
            budget.usage(),
            BudgetUsage {
                granted: Some(600),
                used: 500,
            }
        );
        assert_eq!(budget.held().len(), 2);
    }

    #[test]
    fn credits_are_acquired_once_per_key_and_released_once() {
        let mut budget = ResourceBudget::new(verified(MemoryMode::Strict, 100), &[]);
        budget.acquire(&[(lease(1), 60)]).unwrap();
        // An action repeated after a worker restart acquires nothing twice.
        budget.acquire(&[(lease(1), 60)]).unwrap();
        assert_eq!(budget.usage().used, 60);
        budget.release(&[lease(1)]);
        budget.release(&[lease(1), lease(9)]);
        assert_eq!(budget.usage().used, 0);
        assert!(budget.held().is_empty());
    }

    #[test]
    fn a_strict_refusal_names_the_credit_and_acquires_none_of_the_action() {
        let mut budget = ResourceBudget::new(verified(MemoryMode::Strict, 100), &[]);
        let instances = CreditKey::Instances(GenerationId::new(2), SlotId::new(1));
        assert_eq!(
            budget.acquire(&[(lease(1), 60), (instances.clone(), 60)]),
            Err(instances.clone())
        );
        assert_eq!(budget.usage().used, 0);
        assert!(budget.held().is_empty());
        // What fits is admitted.
        budget.acquire(&[(lease(1), 60), (instances, 40)]).unwrap();
        assert_eq!(budget.usage().used, 100);
    }

    #[test]
    fn permissive_and_unverified_budgets_hold_what_does_not_fit_without_counting_it() {
        let mut permissive = ResourceBudget::new(verified(MemoryMode::Permissive, 100), &[]);
        permissive.acquire(&[(lease(1), 60), (lease(2), 60)]).unwrap();
        assert_eq!(permissive.usage().used, 60);
        assert_eq!(permissive.held(), [lease(1), lease(2)]);
        // Releasing the uncounted credit returns nothing it did not take.
        permissive.release(&[lease(2)]);
        assert_eq!(permissive.usage().used, 60);

        let mut uncounted = ResourceBudget::new(None, &[(lease(0), 1_000_000)]);
        uncounted.acquire(&[(lease(1), u64::MAX)]).unwrap();
        assert_eq!(uncounted.usage(), BudgetUsage { granted: None, used: 0 });
        assert_eq!(uncounted.held().len(), 2);
    }
}

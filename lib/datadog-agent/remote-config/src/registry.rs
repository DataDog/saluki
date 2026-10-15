//! State the client shares with its worker, which outlives any one run of the worker.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;
use uuid::Uuid;

use crate::decoder::{evaluate, Evaluation, Outcome};
use crate::subscription::Publisher;
use crate::{ConfigId, Error, ProductDecoder, Result, Subscription};

/// Identifies one subscription to a product, so the worker can tell a new subscription from the one it replaced.
pub(crate) type Generation = u64;

/// The client's identity and subscriptions, shared by every client handle and the worker.
///
/// A worker restart discards protocol state but not this, so a restart never strands a subscriber or changes the ID the
/// Agent tracks the client by.
pub(crate) struct Shared {
    /// The ID the client reports in every poll, generated once per client.
    pub(crate) client_id: String,

    registry: Mutex<Registry>,

    /// Wakes the worker to poll after a subscribe.
    ///
    /// It holds at most one permit, so several subscribes before the worker next waits wake it once.
    pub(crate) wake: Notify,
}

struct Registry {
    /// Each subscribed product's publisher, keyed by its protocol string so that two decoders naming one product share
    /// its entry.
    ///
    /// An entry whose subscriptions have all been dropped stays until it is replaced by a new subscription or pruned by
    /// the worker.
    products: HashMap<String, Registration>,

    next_generation: Generation,
}

struct Registration {
    generation: Generation,
    product: Arc<dyn Product>,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            client_id: Uuid::new_v4().to_string(),
            registry: Mutex::new(Registry {
                products: HashMap::new(),
                next_generation: 0,
            }),
            wake: Notify::new(),
        }
    }

    /// Locks the registry, recovering it from a panic in another thread.
    ///
    /// Every locked section leaves the registry consistent between statements, and no subscriber code runs while it is
    /// locked, so a poisoned lock holds valid state.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns whether a panic while the registry was locked poisoned its lock.
    #[cfg(test)]
    pub(crate) fn is_poisoned(&self) -> bool {
        self.registry.is_poisoned()
    }

    /// Panics while holding the registry lock, which poisons it.
    #[cfg(test)]
    pub(crate) fn panic_while_locked(&self) {
        let _registry = self.registry.lock().unwrap();
        panic!("poisoning the registry");
    }

    /// Registers a subscription to the product `P` decodes, replacing an entry with no live subscriptions.
    pub(crate) fn subscribe<P: ProductDecoder>(&self) -> Result<Subscription<P::Snapshot, P::Error>> {
        let product = P::PRODUCT;
        let mut registry = self.lock();
        if registry
            .products
            .get(product)
            .is_some_and(|existing| existing.product.is_subscribed())
        {
            return Err(Error::AlreadySubscribed {
                product: product.to_owned(),
            });
        }

        let (publisher, subscription) = Publisher::new();
        let generation = registry.next_generation;
        registry.next_generation += 1;
        // Dropping the replaced entry can run the subscriber's `Drop` for its last snapshot, so it waits for the unlock.
        let replaced = registry.products.insert(
            product.to_owned(),
            Registration {
                generation,
                product: Arc::new(Decoded::<P> {
                    publisher,
                    decoder: PhantomData,
                }),
            },
        );
        drop(registry);
        drop(replaced);

        self.wake.notify_one();
        Ok(subscription)
    }

    /// Forgets products with no live subscriptions and returns the rest.
    pub(crate) fn live_products(&self) -> BTreeMap<String, Generation> {
        let mut registry = self.lock();
        // Dropped after the unlock, like a replaced entry in `subscribe`.
        let pruned: Vec<_> = registry
            .products
            .extract_if(|_, registration| !registration.product.is_subscribed())
            .collect();
        let live = registry
            .products
            .iter()
            .map(|(product, registration)| (product.clone(), registration.generation))
            .collect();
        drop(registry);
        drop(pruned);
        live
    }

    /// Decodes `assignment` with the product's decoder and publishes the outcome to its subscriptions.
    ///
    /// Returns the subscription that received it and the evaluation, or `None` when the product is not registered.
    ///
    /// The decoder runs without the registry locked, so a concurrent subscribe does not wait for it. A subscription
    /// replaced meanwhile had no live clones, so the outcome reaches no one, and the returned generation no longer
    /// matches [`live_products`](Self::live_products), so the worker decodes the replacement on its next poll.
    pub(crate) fn assign(
        &self, product: &str, assignment: Vec<(ConfigId, &[u8])>,
    ) -> Option<(Generation, Evaluation<(), ()>)> {
        let (generation, product) = {
            let registry = self.lock();
            let registration = registry.products.get(product)?;
            (registration.generation, Arc::clone(&registration.product))
        };
        Some((generation, product.assign(assignment)))
    }
}

/// A subscribed product with its decoder type erased, so that products with different decoders share one registry.
trait Product: Send + Sync {
    /// Returns whether any clone of the product's subscription is still alive.
    fn is_subscribed(&self) -> bool;

    /// Decodes and publishes an assignment, returning the outcome with the subscriber's snapshot and error erased.
    fn assign(&self, assignment: Vec<(ConfigId, &[u8])>) -> Evaluation<(), ()>;
}

struct Decoded<P: ProductDecoder> {
    publisher: Publisher<P::Snapshot, P::Error>,
    decoder: PhantomData<fn() -> P>,
}

impl<P: ProductDecoder> Product for Decoded<P> {
    fn is_subscribed(&self) -> bool {
        self.publisher.is_subscribed()
    }

    fn assign(&self, assignment: Vec<(ConfigId, &[u8])>) -> Evaluation<(), ()> {
        let Evaluation { outcome, verdicts } = evaluate::<P>(assignment);
        let reduced = match &outcome {
            Outcome::Accepted(_) => Outcome::Accepted(()),
            Outcome::Rejected { reason, .. } => Outcome::Rejected {
                error: (),
                reason: reason.clone(),
            },
            Outcome::Panicked => Outcome::Panicked,
        };
        self.publisher.publish(outcome);
        Evaluation {
            outcome: reduced,
            verdicts,
        }
    }
}

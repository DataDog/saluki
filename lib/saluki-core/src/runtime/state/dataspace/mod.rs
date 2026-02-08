//! A type-erased, async-aware dataspace registry for inter-process coordination.
//!
//! The [`DataspaceRegistry`] allows processes to assert and retract typed values by handle, and subscribe to receive
//! notifications of those assertions and retractions. Multiple subscribers can observe the same updates.
//!
//! - **Assertion**: a value of type `T` becomes available, associated with a given handle.
//! - **Retraction**: the value of type `T` associated with a given handle is withdrawn.
//!
//! Subscribers can listen for updates on a specific handle, or on all handles for a given type.
//!
//! This enables decoupled coordination where processes don't need to know about each other, only the handle and
//! type of the values they're exchanging.
//!
//! # Example
//!
//! ```
//! use saluki_core::runtime::state::{AssertionUpdate, Handle, DataspaceRegistry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let registry = DataspaceRegistry::new();
//! let handle = Handle::new_global();
//!
//! // Subscribe before asserting
//! let mut sub = registry.subscribe::<u32>(handle);
//!
//! // Assert a value
//! registry.assert(42u32, handle);
//!
//! // Receive the assertion
//! let value = sub.recv().await;
//! assert_eq!(value, Some(AssertionUpdate::Asserted(42)));
//! # }
//! ```

use std::{
    any::{Any, TypeId},
    collections::{HashMap, VecDeque},
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;

use super::Handle;

const DEFAULT_CHANNEL_CAPACITY: usize = 16;

/// An update received by a subscription, indicating that a value was asserted or retracted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssertionUpdate<T> {
    /// A value was asserted (made available).
    Asserted(T),

    /// The value was retracted (withdrawn).
    Retracted,
}

/// Internal key combining type and handle.
#[derive(Clone, PartialEq, Eq, Hash)]
struct StorageKey {
    type_id: TypeId,
    handle: Handle,
}

impl StorageKey {
    fn new<T: 'static>(handle: Handle) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            handle,
        }
    }
}

/// Internal registry state protected by a mutex.
struct RegistryState {
    /// Broadcast senders for each (type, handle) pair, stored type-erased.
    ///
    /// Each entry stores a `broadcast::Sender<AssertionUpdate<T>>`.
    channels: HashMap<StorageKey, Box<dyn Any + Send + Sync>>,

    /// Broadcast senders for wildcard subscriptions, keyed by type only.
    ///
    /// Each entry stores a `broadcast::Sender<(Handle, AssertionUpdate<T>)>`.
    wildcard_channels: HashMap<TypeId, Box<dyn Any + Send + Sync>>,

    /// Current assertion values for each (type, handle) pair, stored type-erased.
    ///
    /// Each entry stores the most recently asserted value as a `Box<T>` (type-erased). Entries are removed on
    /// retraction. Used to replay current state to new subscribers.
    current_values: HashMap<StorageKey, Box<dyn Any + Send + Sync>>,

    /// Default capacity for new broadcast channels.
    channel_capacity: usize,
}

impl RegistryState {
    fn new(channel_capacity: usize) -> Self {
        Self {
            channels: HashMap::new(),
            wildcard_channels: HashMap::new(),
            current_values: HashMap::new(),
            channel_capacity,
        }
    }

    /// Gets or creates a broadcast sender for the given key, returning a new receiver.
    fn get_or_create_sender<T>(&mut self, key: StorageKey) -> broadcast::Receiver<AssertionUpdate<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let sender = self
            .channels
            .entry(key)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel::<AssertionUpdate<T>>(self.channel_capacity);
                Box::new(tx)
            })
            .downcast_ref::<broadcast::Sender<AssertionUpdate<T>>>()
            .expect("type mismatch in dataspace registry");

        sender.subscribe()
    }

    /// Gets or creates a wildcard broadcast sender for the given type, returning a new receiver.
    fn get_or_create_wildcard_sender<T>(&mut self) -> broadcast::Receiver<(Handle, AssertionUpdate<T>)>
    where
        T: Clone + Send + Sync + 'static,
    {
        let sender = self
            .wildcard_channels
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel::<(Handle, AssertionUpdate<T>)>(self.channel_capacity);
                Box::new(tx)
            })
            .downcast_ref::<broadcast::Sender<(Handle, AssertionUpdate<T>)>>()
            .expect("type mismatch in dataspace registry");

        sender.subscribe()
    }
}

/// Shared inner state of the registry.
struct DataspaceRegistryInner {
    state: Mutex<RegistryState>,
}

/// A dataspace registry for async coordination between processes.
///
/// The registry stores broadcast channels indexed by type and [`Handle`]. Processes can subscribe to receive
/// assertion and retraction updates for a given type and handle (or all handles), and other processes can assert
/// or retract values that are delivered to all current subscribers.
///
/// # Thread Safety
///
/// `DataspaceRegistry` is `Clone` and can be safely shared across threads and tasks. All operations are thread-safe.
#[derive(Clone)]
pub struct DataspaceRegistry {
    inner: Arc<DataspaceRegistryInner>,
}

impl Default for DataspaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DataspaceRegistry {
    /// Creates a new empty registry with the default channel capacity.
    pub fn new() -> Self {
        Self::with_channel_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    /// Creates a new empty registry with the given channel capacity for broadcast channels.
    pub fn with_channel_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(DataspaceRegistryInner {
                state: Mutex::new(RegistryState::new(capacity)),
            }),
        }
    }

    /// Asserts a value with the given handle, notifying all current subscribers.
    ///
    /// The value is stored so that future subscribers can receive the current state when they subscribe. The assertion
    /// is also delivered to every active [`Subscription`] for the same type and handle, as well as every active
    /// [`WildcardSubscription`] for the same type.
    pub fn assert<T>(&self, value: T, handle: Handle)
    where
        T: Clone + Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);
        let mut state = self.inner.state.lock().unwrap();

        // Store the current value for future subscribers.
        state.current_values.insert(key.clone(), Box::new(value.clone()));

        let specific_sender = state
            .channels
            .get(&key)
            .and_then(|boxed| boxed.downcast_ref::<broadcast::Sender<AssertionUpdate<T>>>());

        let wildcard_sender = state
            .wildcard_channels
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<broadcast::Sender<(Handle, AssertionUpdate<T>)>>());

        if let Some(sender) = specific_sender {
            let _ = sender.send(AssertionUpdate::Asserted(value.clone()));
        }

        if let Some(sender) = wildcard_sender {
            let _ = sender.send((handle, AssertionUpdate::Asserted(value)));
        }
    }

    /// Retracts the value of the given type and handle, notifying all current subscribers.
    ///
    /// The stored value for the given type and handle is removed. The retraction is also delivered to every active
    /// [`Subscription`] for the same type and handle, as well as every active [`WildcardSubscription`] for the same
    /// type.
    pub fn retract<T>(&self, handle: Handle)
    where
        T: Clone + Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);
        let mut state = self.inner.state.lock().unwrap();

        // Remove the stored value.
        state.current_values.remove(&key);

        let specific_sender = state
            .channels
            .get(&key)
            .and_then(|boxed| boxed.downcast_ref::<broadcast::Sender<AssertionUpdate<T>>>());

        let wildcard_sender = state
            .wildcard_channels
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<broadcast::Sender<(Handle, AssertionUpdate<T>)>>());

        if let Some(sender) = specific_sender {
            let _ = sender.send(AssertionUpdate::Retracted);
        }

        if let Some(sender) = wildcard_sender {
            let _ = sender.send((handle, AssertionUpdate::Retracted));
        }
    }

    /// Subscribes to assertion and retraction updates for the given type and handle.
    ///
    /// Returns a [`Subscription`] that can be used to asynchronously receive updates. If no broadcast channel exists
    /// yet for this (type, handle) pair, one is created.
    ///
    /// If a value has already been asserted for this type and handle, the subscription will immediately yield the
    /// current value before any future broadcast updates.
    pub fn subscribe<T>(&self, handle: Handle) -> Subscription<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

        let mut state = self.inner.state.lock().unwrap();
        let rx = state.get_or_create_sender::<T>(key.clone());

        // Look up the current value for this (type, handle) pair and replay it to the new subscriber.
        let pending = state
            .current_values
            .get(&key)
            .and_then(|boxed| boxed.downcast_ref::<T>())
            .map(|value| AssertionUpdate::Asserted(value.clone()));

        Subscription { pending, rx }
    }

    /// Subscribes to assertion and retraction updates for the given type across all handles.
    ///
    /// Returns a [`WildcardSubscription`] that receives updates from every handle, along with the handle each update
    /// is associated with. If no wildcard broadcast channel exists yet for this type, one is created.
    ///
    /// If values have already been asserted for this type on any handles, the subscription will immediately yield all
    /// current values before any future broadcast updates.
    pub fn subscribe_all<T>(&self) -> WildcardSubscription<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let mut state = self.inner.state.lock().unwrap();
        let rx = state.get_or_create_wildcard_sender::<T>();

        // Collect all current values for this type across all handles.
        let type_id = TypeId::of::<T>();
        let pending: VecDeque<_> = state
            .current_values
            .iter()
            .filter(|(key, _)| key.type_id == type_id)
            .filter_map(|(key, boxed)| {
                boxed
                    .downcast_ref::<T>()
                    .map(|value| (key.handle, AssertionUpdate::Asserted(value.clone())))
            })
            .collect();

        WildcardSubscription { pending, rx }
    }
}

/// A subscription to updates for a specific type and handle from a [`DataspaceRegistry`].
///
/// Created by calling [`DataspaceRegistry::subscribe`]. Use [`recv`](Subscription::recv) to asynchronously wait for
/// the next update.
///
/// If the subscribed type and handle already had an asserted value when this subscription was created, the first call
/// to [`recv`](Subscription::recv) will return that value before any subsequent broadcast updates.
pub struct Subscription<T> {
    pending: Option<AssertionUpdate<T>>,
    rx: broadcast::Receiver<AssertionUpdate<T>>,
}

impl<T> Subscription<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// Receives the next assertion or retraction update.
    ///
    /// Returns `Some(update)` when an update is available, or `None` if the channel has been closed (all senders
    /// dropped). If messages were missed due to the subscriber falling behind, the missed messages are skipped and the
    /// next available update is returned.
    pub async fn recv(&mut self) -> Option<AssertionUpdate<T>> {
        if let Some(value) = self.pending.take() {
            return Some(value);
        }

        loop {
            match self.rx.recv().await {
                Ok(value) => return Some(value),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// A wildcard subscription to updates for a specific type across all handles from a [`DataspaceRegistry`].
///
/// Created by calling [`DataspaceRegistry::subscribe_all`]. Use [`recv`](WildcardSubscription::recv) to asynchronously
/// wait for the next update.
///
/// If any values have already been asserted for the subscribed type when this subscription was created, the first
/// calls to [`recv`](WildcardSubscription::recv) will return those values before any subsequent broadcast updates.
pub struct WildcardSubscription<T> {
    pending: VecDeque<(Handle, AssertionUpdate<T>)>,
    rx: broadcast::Receiver<(Handle, AssertionUpdate<T>)>,
}

impl<T> WildcardSubscription<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// Receives the next assertion or retraction update, along with the handle it is associated with.
    ///
    /// Returns `Some((handle, update))` when an update is available, or `None` if the channel has been closed (all
    /// senders dropped). If messages were missed due to the subscriber falling behind, the missed messages are skipped
    /// and the next available update is returned.
    pub async fn recv(&mut self) -> Option<(Handle, AssertionUpdate<T>)> {
        if let Some(value) = self.pending.pop_front() {
            return Some(value);
        }

        loop {
            match self.rx.recv().await {
                Ok(value) => return Some(value),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn subscribe_then_assert() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);
        registry.assert(42u32, h);

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(AssertionUpdate::Asserted(42)));
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_assertion() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub1 = registry.subscribe::<u32>(h);
        let mut sub2 = registry.subscribe::<u32>(h);

        registry.assert(42u32, h);

        let v1 = timeout(Duration::from_millis(100), sub1.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub2.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(42)));
        assert_eq!(v2, Some(AssertionUpdate::Asserted(42)));
    }

    #[tokio::test]
    async fn assert_without_subscribers_stores_value() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        // Assert before any subscriber exists -- the value is stored.
        registry.assert(42u32, h);

        // A later subscriber should receive the current value.
        let mut sub = registry.subscribe::<u32>(h);
        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(AssertionUpdate::Asserted(42)));
    }

    #[tokio::test]
    async fn different_types_same_handle() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub_u32 = registry.subscribe::<u32>(h);
        let mut sub_string = registry.subscribe::<String>(h);

        registry.assert(42u32, h);
        registry.assert("hello".to_string(), h);

        let v1 = timeout(Duration::from_millis(100), sub_u32.recv())
            .await
            .expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub_string.recv())
            .await
            .expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(42)));
        assert_eq!(v2, Some(AssertionUpdate::Asserted("hello".to_string())));
    }

    #[tokio::test]
    async fn process_handle() {
        let registry = DataspaceRegistry::new();
        let pid = crate::runtime::process::Id::new();
        let h = Handle::for_process(pid);

        let mut sub = registry.subscribe::<u32>(h);
        registry.assert(42u32, h);

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(AssertionUpdate::Asserted(42)));
    }

    #[tokio::test]
    async fn channel_closed_returns_none() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        // Drop the registry, which drops the Arc. Since we only have one reference, the sender is dropped.
        drop(registry);

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn lagged_subscriber_recovers() {
        // Create a registry with a tiny buffer so we can force lag.
        let registry = DataspaceRegistry::with_channel_capacity(2);
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        // Assert more values than the channel can hold.
        for i in 0..10 {
            registry.assert(i as u32, h);
        }

        // The subscriber should skip lagged messages and still receive a value.
        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert!(value.is_some());
    }

    #[tokio::test]
    async fn multiple_values_received_in_order() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        registry.assert(1u32, h);
        registry.assert(2u32, h);
        registry.assert(3u32, h);

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v3 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(1)));
        assert_eq!(v2, Some(AssertionUpdate::Asserted(2)));
        assert_eq!(v3, Some(AssertionUpdate::Asserted(3)));
    }

    #[tokio::test]
    async fn assert_then_retract() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        registry.assert(42u32, h);
        registry.retract::<u32>(h);

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(42)));
        assert_eq!(v2, Some(AssertionUpdate::Retracted));
    }

    #[tokio::test]
    async fn retract_without_subscribers_succeeds() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        // Retract without any subscribers -- should not panic.
        registry.retract::<u32>(h);
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_retraction() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub1 = registry.subscribe::<u32>(h);
        let mut sub2 = registry.subscribe::<u32>(h);

        registry.retract::<u32>(h);

        let v1 = timeout(Duration::from_millis(100), sub1.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub2.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Retracted));
        assert_eq!(v2, Some(AssertionUpdate::Retracted));
    }

    #[tokio::test]
    async fn assert_retract_assert_sequence() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        registry.assert(1u32, h);
        registry.retract::<u32>(h);
        registry.assert(2u32, h);

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v3 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(1)));
        assert_eq!(v2, Some(AssertionUpdate::Retracted));
        assert_eq!(v3, Some(AssertionUpdate::Asserted(2)));
    }

    #[tokio::test]
    async fn wildcard_receives_from_multiple_handles() {
        let registry = DataspaceRegistry::new();
        let h1 = Handle::new_global();
        let h2 = Handle::new_global();

        let mut sub = registry.subscribe_all::<u32>();

        registry.assert(1u32, h1);
        registry.assert(2u32, h2);

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some((h1, AssertionUpdate::Asserted(1))));
        assert_eq!(v2, Some((h2, AssertionUpdate::Asserted(2))));
    }

    #[tokio::test]
    async fn wildcard_receives_retraction() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe_all::<u32>();

        registry.assert(42u32, h);
        registry.retract::<u32>(h);

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some((h, AssertionUpdate::Asserted(42))));
        assert_eq!(v2, Some((h, AssertionUpdate::Retracted)));
    }

    #[tokio::test]
    async fn wildcard_and_specific_both_receive() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        let mut specific = registry.subscribe::<u32>(h);
        let mut wildcard = registry.subscribe_all::<u32>();

        registry.assert(42u32, h);

        let v_specific = timeout(Duration::from_millis(100), specific.recv())
            .await
            .expect("timeout");
        let v_wildcard = timeout(Duration::from_millis(100), wildcard.recv())
            .await
            .expect("timeout");

        assert_eq!(v_specific, Some(AssertionUpdate::Asserted(42)));
        assert_eq!(v_wildcard, Some((h, AssertionUpdate::Asserted(42))));
    }

    #[tokio::test]
    async fn wildcard_subscriber_created_before_specific_channels() {
        let registry = DataspaceRegistry::new();

        // Subscribe to all handles before any specific-handle activity exists.
        let mut wildcard = registry.subscribe_all::<u32>();

        let h = Handle::new_global();
        registry.assert(42u32, h);

        let value = timeout(Duration::from_millis(100), wildcard.recv())
            .await
            .expect("timeout");
        assert_eq!(value, Some((h, AssertionUpdate::Asserted(42))));
    }

    #[tokio::test]
    async fn subscribe_receives_current_value() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        // Assert before subscribing.
        registry.assert(42u32, h);

        // Subscribe after asserting -- should immediately get the current value.
        let mut sub = registry.subscribe::<u32>(h);
        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(AssertionUpdate::Asserted(42)));
    }

    #[tokio::test]
    async fn subscribe_after_retract_gets_nothing_pending() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        // Assert then retract -- no current value.
        registry.assert(42u32, h);
        registry.retract::<u32>(h);

        let mut sub = registry.subscribe::<u32>(h);

        // Drop the registry so the channel closes, proving no pending value is delivered.
        drop(registry);

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn subscribe_all_receives_current_values() {
        let registry = DataspaceRegistry::new();
        let h1 = Handle::new_global();
        let h2 = Handle::new_global();

        // Assert on two handles before subscribing.
        registry.assert(1u32, h1);
        registry.assert(2u32, h2);

        let mut sub = registry.subscribe_all::<u32>();

        // Should receive both current values (order is not guaranteed since HashMap iteration is unordered).
        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        let mut received = [v1.unwrap(), v2.unwrap()];
        received.sort_by_key(|(_, update)| match update {
            AssertionUpdate::Asserted(v) => *v,
            AssertionUpdate::Retracted => 0,
        });

        assert_eq!(received[0], (h1, AssertionUpdate::Asserted(1)));
        assert_eq!(received[1], (h2, AssertionUpdate::Asserted(2)));
    }

    #[tokio::test]
    async fn subscribe_receives_current_then_future() {
        let registry = DataspaceRegistry::new();
        let h = Handle::new_global();

        // Assert before subscribing.
        registry.assert(1u32, h);

        let mut sub = registry.subscribe::<u32>(h);

        // Assert again after subscribing.
        registry.assert(2u32, h);

        // First recv should return the initial value, second should return the broadcast value.
        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some(AssertionUpdate::Asserted(1)));
        assert_eq!(v2, Some(AssertionUpdate::Asserted(2)));
    }
}

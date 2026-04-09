//! A type-erased, async-aware dataspace registry for inter-process coordination.
//!
//! The [`DataspaceRegistry`] allows processes to assert and retract typed values by identifier, and subscribe to
//! receive notifications of those assertions and retractions. Multiple subscribers can observe the same updates.
//!
//! - **Assertion**: a value of type `T` becomes available, associated with a given identifier.
//! - **Retraction**: the value of type `T` associated with a given identifier is withdrawn.
//!
//! Subscribers can listen for updates matching a specific identifier, a prefix, or all identifiers for a given type.
//!
//! This enables decoupled coordination where processes don't need to know about each other, only the identifier and
//! type of the values they're exchanging.
//!
//! # Example
//!
//! ```
//! use saluki_core::runtime::state::{AssertionUpdate, Identifier, IdentifierFilter, DataspaceRegistry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let registry = DataspaceRegistry::new();
//! let id = Identifier::named("my_value");
//!
//! // Subscribe before asserting:
//! let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
//!
//! // Assert a value:
//! registry.assert(42u32, id.clone());
//!
//! // Receive the assertion:
//! let value = sub.recv().await;
//! assert_eq!(value, Some(AssertionUpdate::Asserted(id, 42))); #
//! # }
//! ```

use std::{
    any::{Any, TypeId},
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;

use super::{Identifier, IdentifierFilter};
use crate::runtime::process::Id;

const DEFAULT_CHANNEL_CAPACITY: usize = 16;

tokio::task_local! {
    pub(crate) static CURRENT_DATASPACE: DataspaceRegistry;
}

/// An update received by a subscription, indicating that a value was asserted or retracted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssertionUpdate<T> {
    /// A value was asserted (made available), along with the identifier it is associated with.
    Asserted(Identifier, T),

    /// The value associated with the given identifier was retracted (withdrawn).
    Retracted(Identifier),
}

/// Internal key combining type and identifier.
#[derive(Clone, PartialEq, Eq, Hash)]
struct StorageKey {
    type_id: TypeId,
    identifier: Identifier,
}

impl StorageKey {
    fn new<T: 'static>(identifier: Identifier) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            identifier,
        }
    }
}

/// A type-erased broadcast channel that supports sending retraction notifications without knowing `T`.
trait AnyChannel: Send + Sync {
    /// Sends a retraction for the given identifier.
    fn send_retraction(&self, id: &Identifier);

    /// Returns a downcasted reference to `self` that can be fallibly upcasted back to the original type.
    fn as_any(&self) -> &dyn Any;
}

/// Concrete implementation of [`AnyChannel`] that wraps a `broadcast::Sender<AssertionUpdate<T>>`.
struct TypedChannel<T: Clone + Send + Sync + 'static> {
    tx: broadcast::Sender<AssertionUpdate<T>>,
}

impl<T: Clone + Send + Sync + 'static> AnyChannel for TypedChannel<T> {
    fn send_retraction(&self, id: &Identifier) {
        let _ = self.tx.send(AssertionUpdate::Retracted(id.clone()));
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A type-erased filtered subscription channel.
struct FilteredChannel {
    type_id: TypeId,
    filter: IdentifierFilter,
    sender: Box<dyn AnyChannel>,
}

/// A stored assertion value along with the process that owns it.
struct StoredValue {
    value: Box<dyn Any + Send + Sync>,
    owner: Id,
}

/// Internal registry state protected by a mutex.
struct RegistryState {
    /// Broadcast senders for exact-match subscriptions, keyed by (type, identifier).
    channels: HashMap<StorageKey, Box<dyn AnyChannel>>,

    /// Filtered subscription channels that are evaluated on every assert/retract.
    filtered_channels: Vec<FilteredChannel>,

    /// Current assertion values for each (type, identifier) pair, stored type-erased.
    ///
    /// Entries are removed on retraction. Used to replay current state to new subscribers.
    current_values: HashMap<StorageKey, StoredValue>,

    /// Tracks which storage keys each process has asserted, for automatic retraction on process exit.
    process_assertions: HashMap<Id, HashSet<StorageKey>>,

    /// Default capacity for new broadcast channels.
    channel_capacity: usize,
}

impl RegistryState {
    fn new(channel_capacity: usize) -> Self {
        Self {
            channels: HashMap::new(),
            filtered_channels: Vec::new(),
            current_values: HashMap::new(),
            process_assertions: HashMap::new(),
            channel_capacity,
        }
    }

    /// Gets or creates a broadcast sender for the given key, returning a new receiver.
    fn get_or_create_exact_sender<T>(&mut self, key: StorageKey) -> broadcast::Receiver<AssertionUpdate<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let channel = self.channels.entry(key).or_insert_with(|| {
            let (tx, _) = broadcast::channel::<AssertionUpdate<T>>(self.channel_capacity);
            Box::new(TypedChannel { tx })
        });

        let typed = channel
            .as_any()
            .downcast_ref::<TypedChannel<T>>()
            // `StorageKey` includes `TypeId::of::<T>()`, so a channel stored under a
            // given key is always a `TypedChannel<T>` for the same `T`.
            .unwrap_or_else(|| unreachable!("type mismatch in dataspace registry"));

        typed.tx.subscribe()
    }

    /// Creates a new filtered subscription channel, returning a new receiver.
    fn create_filtered_sender<T>(&mut self, filter: IdentifierFilter) -> broadcast::Receiver<AssertionUpdate<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let (tx, rx) = broadcast::channel::<AssertionUpdate<T>>(self.channel_capacity);

        self.filtered_channels.push(FilteredChannel {
            type_id: TypeId::of::<T>(),
            filter,
            sender: Box::new(TypedChannel { tx }),
        });

        rx
    }

    /// Sends a retraction notification on all channels (exact + filtered) matching the given key.
    fn notify_retraction(&self, key: &StorageKey) {
        if let Some(ch) = self.channels.get(key) {
            ch.send_retraction(&key.identifier);
        }

        for filtered in &self.filtered_channels {
            if filtered.type_id == key.type_id && filtered.filter.matches(&key.identifier) {
                filtered.sender.send_retraction(&key.identifier);
            }
        }
    }
}

/// Shared inner state of the registry.
struct DataspaceRegistryInner {
    state: Mutex<RegistryState>,
}

/// A dataspace registry for async coordination between processes.
///
/// The registry stores broadcast channels indexed by type and [`Identifier`]. Processes can subscribe to receive
/// assertion and retraction updates for a given type and identifier filter, and other processes can assert or retract
/// values that are delivered to all matching subscribers.
///
/// # Thread Safety
///
/// `DataspaceRegistry` is `Clone` and can be safely shared across threadVs and tasks. All operations are thread-safe.
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

    /// Returns the dataspace registry for the current supervision tree, if one exists.
    pub fn try_current() -> Option<Self> {
        CURRENT_DATASPACE.try_with(|ds| ds.clone()).ok()
    }

    /// Creates a new empty registry with the given channel capacity for broadcast channels.
    pub fn with_channel_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(DataspaceRegistryInner {
                state: Mutex::new(RegistryState::new(capacity)),
            }),
        }
    }

    /// Asserts a value with the given identifier, notifying all matching subscribers.
    ///
    /// The assertion is automatically associated with the current process, and will be automatically retracted when
    /// that process exists. Only the owning process may update an existing assertion for a given type/identifier
    /// combination.
    pub fn assert<T>(&self, value: T, id: impl Into<Identifier>)
    where
        T: Clone + Send + Sync + 'static,
    {
        let id = id.into();
        let key = StorageKey::new::<T>(id.clone());
        let caller = Id::current();
        let mut state = self.inner.state.lock().unwrap();

        // If an assertion already exists for this key, only the owning process may update it.
        if let Some(existing) = state.current_values.get(&key) {
            debug_assert_eq!(
                existing.owner, caller,
                "process {caller:?} attempted to update assertion owned by {:?}",
                existing.owner
            );
            if existing.owner != caller {
                return;
            }
        }

        // Store the current value for future subscribers, along with the owning process.
        state.current_values.insert(
            key.clone(),
            StoredValue {
                value: Box::new(value.clone()),
                owner: caller,
            },
        );

        // Track this assertion against the owning process.
        state.process_assertions.entry(caller).or_default().insert(key.clone());

        // Notify exact-match subscribers.
        if let Some(ch) = state.channels.get(&key) {
            if let Some(typed) = ch.as_any().downcast_ref::<TypedChannel<T>>() {
                let _ = typed.tx.send(AssertionUpdate::Asserted(id.clone(), value.clone()));
            }
        }

        // Notify filtered subscribers.
        let type_id = TypeId::of::<T>();
        for channel in &state.filtered_channels {
            if channel.type_id == type_id && channel.filter.matches(&id) {
                if let Some(typed) = channel.sender.as_any().downcast_ref::<TypedChannel<T>>() {
                    let _ = typed.tx.send(AssertionUpdate::Asserted(id.clone(), value.clone()));
                }
            }
        }
    }

    /// Retracts the value of the given type and identifier, notifying all matching subscribers.
    ///
    /// Only the process that originally asserted the value may retract it.
    pub fn retract<T>(&self, id: impl Into<Identifier>)
    where
        T: Clone + Send + Sync + 'static,
    {
        let id = id.into();
        let key = StorageKey::new::<T>(id.clone());
        let caller = Id::current();
        let mut state = self.inner.state.lock().unwrap();

        // Check that the assertion exists and is owned by the calling process.
        let Some(stored) = state.current_values.get(&key) else {
            return;
        };

        debug_assert_eq!(
            stored.owner, caller,
            "process {caller:?} attempted to retract assertion owned by {:?}",
            stored.owner
        );
        if stored.owner != caller {
            return;
        }

        // Remove the stored value and clean up process tracking.
        state.current_values.remove(&key);

        if let Some(keys) = state.process_assertions.get_mut(&caller) {
            keys.remove(&key);
            if keys.is_empty() {
                state.process_assertions.remove(&caller);
            }
        }

        state.notify_retraction(&key);
    }

    /// Retracts all assertions made by the given process.
    ///
    /// This is called automatically when a process exits (via [`FutureProcess`] drop) to ensure that no stale
    /// assertions remain in the registry after the owning process is gone.
    pub(crate) fn retract_all_for_process(&self, process_id: Id) {
        let mut state = self.inner.state.lock().unwrap();

        let Some(keys) = state.process_assertions.remove(&process_id) else {
            return;
        };

        for key in keys {
            state.current_values.remove(&key);
            state.notify_retraction(&key);
        }
    }

    /// Subscribes to assertion and retraction updates matching the given filter.
    ///
    /// Returns a [`Subscription`] that can be used to asynchronously receive updates. Any assertions that match the
    /// filter at the time of subscribing will be immediately yielded.
    pub fn subscribe<T>(&self, filter: IdentifierFilter) -> Subscription<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let mut state = self.inner.state.lock().unwrap();

        match filter {
            IdentifierFilter::Exact(ref id) => {
                let key = StorageKey::new::<T>(id.clone());
                let rx = state.get_or_create_exact_sender::<T>(key.clone());

                // Replay current value if one exists.
                let pending: VecDeque<_> = state
                    .current_values
                    .get(&key)
                    .and_then(|stored| stored.value.downcast_ref::<T>())
                    .map(|value| AssertionUpdate::Asserted(id.clone(), value.clone()))
                    .into_iter()
                    .collect();

                Subscription { pending, rx }
            }
            filter @ (IdentifierFilter::All | IdentifierFilter::Prefix(_)) => {
                let rx = state.create_filtered_sender::<T>(filter.clone());

                // Replay all matching current values.
                let type_id = TypeId::of::<T>();
                let pending: VecDeque<_> = state
                    .current_values
                    .iter()
                    .filter(|(key, _)| key.type_id == type_id && filter.matches(&key.identifier))
                    .filter_map(|(key, stored)| {
                        stored
                            .value
                            .downcast_ref::<T>()
                            .map(|value| AssertionUpdate::Asserted(key.identifier.clone(), value.clone()))
                    })
                    .collect();

                Subscription { pending, rx }
            }
        }
    }
}

/// A subscription to updates for a specific type/identifier combination.
pub struct Subscription<T> {
    pending: VecDeque<AssertionUpdate<T>>,
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
        // TODO: Switch to bounded MPSC channels for delivering assertion/retraction updates.

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
    use std::future::pending;

    use tokio_test::{assert_pending, assert_ready, assert_ready_eq, task::spawn as test_spawn};

    use super::*;
    use crate::runtime::process::Process;

    #[test]
    fn subscribe_then_assert() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        registry.assert(42u32, id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn multiple_subscribers_receive_same_assertion() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub1 = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut sub2 = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        registry.assert(42u32, id.clone());

        let mut recv1 = test_spawn(sub1.recv());
        assert_ready_eq!(recv1.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));

        let mut recv2 = test_spawn(sub2.recv());
        assert_ready_eq!(recv2.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn assert_without_subscribers_stores_value() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        // Assert before any subscriber exists -- the value is stored.
        registry.assert(42u32, id.clone());

        // A later subscriber should receive the current value.
        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn different_types_same_identifier() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub_u32 = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut sub_string = registry.subscribe::<String>(IdentifierFilter::exact(id.clone()));

        registry.assert(42u32, id.clone());
        registry.assert("hello".to_string(), id.clone());

        let mut recv_u32 = test_spawn(sub_u32.recv());
        assert_ready_eq!(recv_u32.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));

        let mut recv_string = test_spawn(sub_string.recv());
        assert_ready_eq!(
            recv_string.poll(),
            Some(AssertionUpdate::Asserted(id, "hello".to_string()))
        );
    }

    #[test]
    fn process_identifier() {
        let registry = DataspaceRegistry::new();
        let pid = crate::runtime::process::Id::new();
        let id = Identifier::from(pid);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        registry.assert(42u32, id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn channel_closed_returns_none() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id));

        // Drop the registry, which drops the Arc. Since we only have one reference, the sender is dropped.
        drop(registry);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), None);
    }

    #[test]
    fn lagged_subscriber_recovers() {
        // Create a registry with a tiny buffer so we can force lag.
        let registry = DataspaceRegistry::with_channel_capacity(2);
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        // Assert more values than the channel can hold.
        for i in 0..10 {
            registry.assert(i as u32, id.clone());
        }

        // The subscriber should skip lagged messages and still receive a value.
        let mut recv = test_spawn(sub.recv());
        let value = assert_ready!(recv.poll());
        assert!(value.is_some());
    }

    #[test]
    fn multiple_values_received_in_order() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        registry.assert(1u32, id.clone());
        registry.assert(2u32, id.clone());
        registry.assert(3u32, id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 1)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 2)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 3)));
    }

    #[test]
    fn assert_then_retract() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        registry.assert(42u32, id.clone());
        registry.retract::<u32>(id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id)));
    }

    #[test]
    fn retract_without_subscribers_succeeds() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        // Retract without any subscribers -- should not panic.
        registry.retract::<u32>(id);
    }

    #[test]
    fn multiple_subscribers_receive_retraction() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub1 = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut sub2 = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        registry.assert(42u32, id.clone());
        registry.retract::<u32>(id.clone());

        // Drain assertion notifications.
        let mut recv1 = test_spawn(sub1.recv());
        assert_ready_eq!(recv1.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv1);

        let mut recv2 = test_spawn(sub2.recv());
        assert_ready_eq!(recv2.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv2);

        // Check retraction notifications.
        let mut recv1 = test_spawn(sub1.recv());
        assert_ready_eq!(recv1.poll(), Some(AssertionUpdate::Retracted(id.clone())));

        let mut recv2 = test_spawn(sub2.recv());
        assert_ready_eq!(recv2.poll(), Some(AssertionUpdate::Retracted(id)));
    }

    #[test]
    fn assert_retract_assert_sequence() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        registry.assert(1u32, id.clone());
        registry.retract::<u32>(id.clone());
        registry.assert(2u32, id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 1)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id.clone())));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 2)));
    }

    #[test]
    fn all_filter_receives_from_multiple_identifiers() {
        let registry = DataspaceRegistry::new();
        let id1 = Identifier::numeric(1);
        let id2 = Identifier::numeric(2);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::all());

        registry.assert(1u32, id1.clone());
        registry.assert(2u32, id2.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id1, 1)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id2, 2)));
    }

    #[test]
    fn all_filter_receives_retraction() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::all());

        registry.assert(42u32, id.clone());
        registry.retract::<u32>(id.clone());

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id)));
    }

    #[test]
    fn all_filter_and_exact_both_receive() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        let mut specific = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut all = registry.subscribe::<u32>(IdentifierFilter::all());

        registry.assert(42u32, id.clone());

        let mut recv_specific = test_spawn(specific.recv());
        assert_ready_eq!(recv_specific.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));

        let mut recv_all = test_spawn(all.recv());
        assert_ready_eq!(recv_all.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn all_filter_created_before_exact_channels() {
        let registry = DataspaceRegistry::new();

        // Subscribe to all identifiers before any specific-identifier activity exists.
        let mut all = registry.subscribe::<u32>(IdentifierFilter::all());

        let id = Identifier::numeric(1);
        registry.assert(42u32, id.clone());

        let mut recv = test_spawn(all.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn subscribe_receives_current_value() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        // Assert before subscribing.
        registry.assert(42u32, id.clone());

        // Subscribe after asserting -- should immediately get the current value.
        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 42)));
    }

    #[test]
    fn subscribe_after_retract_gets_nothing_pending() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        // Assert then retract -- no current value.
        registry.assert(42u32, id.clone());
        registry.retract::<u32>(id.clone());

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id));

        // Drop the registry so the channel closes, proving no pending value is delivered.
        drop(registry);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), None);
    }

    #[test]
    fn all_filter_receives_current_values() {
        let registry = DataspaceRegistry::new();
        let id1 = Identifier::numeric(1);
        let id2 = Identifier::numeric(2);

        // Assert on two identifiers before subscribing.
        registry.assert(1u32, id1.clone());
        registry.assert(2u32, id2.clone());

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::all());

        // Should receive both current values (order is not guaranteed since HashMap iteration is unordered).
        let mut recv = test_spawn(sub.recv());
        let v1 = assert_ready!(recv.poll());
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        let v2 = assert_ready!(recv.poll());

        let mut received = [v1.unwrap(), v2.unwrap()];
        received.sort_by_key(|update| match update {
            AssertionUpdate::Asserted(_, v) => *v,
            AssertionUpdate::Retracted(_) => 0,
        });

        assert_eq!(received[0], AssertionUpdate::Asserted(id1, 1));
        assert_eq!(received[1], AssertionUpdate::Asserted(id2, 2));
    }

    #[test]
    fn subscribe_receives_current_then_future() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(1);

        // Assert before subscribing.
        registry.assert(1u32, id.clone());

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        // Assert again after subscribing.
        registry.assert(2u32, id.clone());

        // First recv should return the initial value, second should return the broadcast value.
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 1)));
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 2)));
    }

    #[test]
    fn prefix_filter_matches_named_identifiers() {
        let registry = DataspaceRegistry::new();
        let id1 = Identifier::named("metrics.cpu");
        let id2 = Identifier::named("metrics.mem");
        let id3 = Identifier::named("logs.error");

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::prefix("metrics."));

        registry.assert(1u32, id1.clone());
        registry.assert(2u32, id2.clone());
        registry.assert(3u32, id3);

        // Should only receive the two metrics identifiers.
        let mut recv = test_spawn(sub.recv());
        let v1 = assert_ready!(recv.poll());
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        let v2 = assert_ready!(recv.poll());

        let mut received = [v1.unwrap(), v2.unwrap()];
        received.sort_by_key(|update| match update {
            AssertionUpdate::Asserted(_, v) => *v,
            AssertionUpdate::Retracted(_) => 0,
        });

        assert_eq!(received[0], AssertionUpdate::Asserted(id1, 1));
        assert_eq!(received[1], AssertionUpdate::Asserted(id2, 2));
    }

    #[test]
    fn prefix_filter_does_not_match_numeric() {
        let registry = DataspaceRegistry::new();
        let id = Identifier::numeric(42);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::prefix("any"));

        registry.assert(1u32, id);

        // Drop registry to close channel, proving no value was delivered.
        drop(registry);

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), None);
    }

    #[test]
    fn prefix_filter_replays_matching_current_values() {
        let registry = DataspaceRegistry::new();
        let id1 = Identifier::named("svc.alpha");
        let id2 = Identifier::named("svc.beta");
        let id3 = Identifier::named("other.gamma");

        // Assert before subscribing.
        registry.assert(1u32, id1.clone());
        registry.assert(2u32, id2.clone());
        registry.assert(3u32, id3);

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::prefix("svc."));

        // Should replay only the two matching values.
        let mut recv = test_spawn(sub.recv());
        let v1 = assert_ready!(recv.poll());
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        let v2 = assert_ready!(recv.poll());

        let mut received = [v1.unwrap(), v2.unwrap()];
        received.sort_by_key(|update| match update {
            AssertionUpdate::Asserted(_, v) => *v,
            AssertionUpdate::Retracted(_) => 0,
        });

        assert_eq!(received[0], AssertionUpdate::Asserted(id1, 1));
        assert_eq!(received[1], AssertionUpdate::Asserted(id2, 2));
    }

    #[test]
    fn try_current_returns_none_outside_context() {
        assert!(DataspaceRegistry::try_current().is_none());
    }

    #[test]
    fn current_and_try_current_work_within_scope() {
        let registry = DataspaceRegistry::new();
        let registry_clone = registry.clone();

        let mut scope_fut = test_spawn(CURRENT_DATASPACE.scope(registry, async {
            let current = DataspaceRegistry::try_current();
            assert!(current.is_some());

            // Verify it's the same registry by asserting in one and reading from the other.
            let current = current.unwrap();
            let id = Identifier::named("test");
            current.assert(42u32, id.clone());

            let mut sub = registry_clone.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
            let value = sub.recv().await;
            assert_eq!(value, Some(AssertionUpdate::Asserted(id, 42)));
        }));
        assert_ready!(scope_fut.poll());
    }

    #[test]
    fn retract_all_for_process_retracts_all_owned_assertions() {
        let registry = DataspaceRegistry::new();
        let process_id = Id::new();
        let id1 = Identifier::named("val1");
        let id2 = Identifier::named("val2");

        // Assert two values as if from the given process.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(process_id, || {
            registry.assert(1u32, id1.clone());
            registry.assert(2u32, id2.clone());
        });

        // Subscribe to both after assertion to get current values replayed.
        let mut sub1 = registry.subscribe::<u32>(IdentifierFilter::exact(id1.clone()));
        let mut sub2 = registry.subscribe::<u32>(IdentifierFilter::exact(id2.clone()));

        // Drain the initial replayed values.
        let mut recv = test_spawn(sub1.recv());
        let _ = assert_ready!(recv.poll());
        drop(recv);

        let mut recv = test_spawn(sub2.recv());
        let _ = assert_ready!(recv.poll());
        drop(recv);

        // Retract all assertions for the process.
        registry.retract_all_for_process(process_id);

        // Both subscribers should receive retraction notifications.
        let mut recv = test_spawn(sub1.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id1)));
        drop(recv);

        let mut recv = test_spawn(sub2.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id2)));
    }

    #[test]
    fn retract_all_for_process_does_not_affect_other_processes() {
        let registry = DataspaceRegistry::new();
        let pid_a = Id::new();
        let pid_b = Id::new();
        let id_a = Identifier::named("a");
        let id_b = Identifier::named("b");

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(1u32, id_a.clone());
        });
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_b, || {
            registry.assert(2u32, id_b.clone());
        });

        // Retract only process A's assertions.
        registry.retract_all_for_process(pid_a);

        // Process B's value should still be available to new subscribers.
        let mut sub_b = registry.subscribe::<u32>(IdentifierFilter::exact(id_b.clone()));
        let mut recv = test_spawn(sub_b.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id_b, 2)));
        drop(recv);

        // Process A's value should not be available.
        let mut sub_a = registry.subscribe::<u32>(IdentifierFilter::exact(id_a));
        drop(registry);
        let mut recv = test_spawn(sub_a.recv());
        assert_ready_eq!(recv.poll(), None);
    }

    #[test]
    fn retract_all_for_process_notifies_filtered_subscribers() {
        let registry = DataspaceRegistry::new();
        let process_id = Id::new();
        let id = Identifier::named("val");

        let mut all_sub = registry.subscribe::<u32>(IdentifierFilter::all());

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(process_id, || {
            registry.assert(42u32, id.clone());
        });

        // Receive the assertion.
        let mut recv = test_spawn(all_sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv);

        // Retract all for the process.
        registry.retract_all_for_process(process_id);

        // Should receive the retraction on the wildcard subscription.
        let mut recv = test_spawn(all_sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id)));
    }

    #[test]
    fn manual_retract_cleans_up_process_tracking() {
        let registry = DataspaceRegistry::new();
        let process_id = Id::new();
        let id = Identifier::named("val");

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(process_id, || {
            registry.assert(42u32, id.clone());
        });

        // Manually retract (must be from the owning process).
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(process_id, || {
            registry.retract::<u32>(id.clone());
        });

        // Drain the assertion and retraction.
        let mut recv = test_spawn(sub.recv());
        let _ = assert_ready!(recv.poll());
        drop(recv);

        let mut recv = test_spawn(sub.recv());
        let _ = assert_ready!(recv.poll());
        drop(recv);

        // Now retract_all_for_process should be a no-op (no duplicate retraction).
        registry.retract_all_for_process(process_id);

        // Drop the registry to close the channel, proving no further messages are pending.
        drop(registry);
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), None);
    }

    #[test]
    fn retract_all_for_unknown_process_is_noop() {
        let registry = DataspaceRegistry::new();
        // Should not panic.
        registry.retract_all_for_process(Id::new());
    }

    #[test]
    fn instrumented_process_retracts_on_normal_completion() {
        let registry = DataspaceRegistry::new();
        let process = Process::supervisor_with_dataspace("test_worker", None, Some(registry.clone())).unwrap();
        let id = "from_worker";

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id));

        // Spawn a subscriber future, asserting that it is quiesced.
        let mut recv_fut = test_spawn(sub.recv());
        assert!(!recv_fut.is_woken());
        assert_pending!(recv_fut.poll());

        // Create an instrumented future that asserts a value, then completes.
        let mut worker = test_spawn(process.into_process_future(async {
            DataspaceRegistry::try_current()
                .expect("dataspace registry should be available")
                .assert(42u32, "from_worker");
        }));

        // Poll the worker to completion — the assertion happens during this poll.
        assert_ready!(worker.poll());

        // The subscriber should now be woken and have the assertion update.
        assert!(recv_fut.is_woken());
        assert_ready_eq!(recv_fut.poll(), Some(AssertionUpdate::Asserted(id.into(), 42)));

        drop(recv_fut);

        // Set up a new subscriber call for getting the retraction.
        let mut recv_fut = test_spawn(sub.recv());
        assert_pending!(recv_fut.poll());
        assert!(!recv_fut.is_woken());

        // Drop the InstrumentedProcess — this triggers automatic retraction.
        drop(worker);

        // The drop should have woken the subscriber.
        assert!(recv_fut.is_woken());
        assert_ready_eq!(recv_fut.poll(), Some(AssertionUpdate::Retracted(id.into())));
    }

    #[test]
    fn instrumented_process_retracts_on_drop_before_completion() {
        let registry = DataspaceRegistry::new();
        let process = Process::supervisor_with_dataspace("test_worker", None, Some(registry.clone())).unwrap();
        let id = "from_worker";

        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id));

        // Spawn a subscriber future, asserting that it is quiesced.
        let mut recv_fut = test_spawn(sub.recv());
        assert!(!recv_fut.is_woken());
        assert_pending!(recv_fut.poll());

        // Create an instrumented future that asserts a value, then pends forever.
        let mut worker = test_spawn(process.into_process_future(async {
            DataspaceRegistry::try_current()
                .expect("dataspace registry should be available")
                .assert(42u32, "from_worker");
            pending::<()>().await;
        }));

        // Drive the worker to assert the value.
        assert_pending!(worker.poll());

        // The subscriber should now be woken and have the assertion update.
        assert!(recv_fut.is_woken());
        assert_ready_eq!(recv_fut.poll(), Some(AssertionUpdate::Asserted(id.into(), 42)));

        drop(recv_fut);

        // Set up a new subscriber call for getting the retraction.
        let mut recv_fut = test_spawn(sub.recv());
        assert_pending!(recv_fut.poll());
        assert!(!recv_fut.is_woken());

        // Drop the worker (simulates abort/cancel) — triggers automatic retraction.
        drop(worker);

        // The drop should have woken the subscriber with a retraction.
        assert!(recv_fut.is_woken());
        assert_ready_eq!(recv_fut.poll(), Some(AssertionUpdate::Retracted(id.into())));
    }

    #[test]
    fn instrumented_process_retracts_multiple_types_on_drop() {
        let registry = DataspaceRegistry::new();
        let process = Process::supervisor_with_dataspace("test_worker", None, Some(registry.clone())).unwrap();
        let id_num = "number";
        let id_str = "text";

        let mut sub_u32 = registry.subscribe::<u32>(IdentifierFilter::exact(id_num));
        let mut sub_str = registry.subscribe::<String>(IdentifierFilter::exact(id_str));

        // Spawn futures for both subscribers, asserting that they are quiesced.
        let mut recv_u32_fut = test_spawn(sub_u32.recv());
        let mut recv_str_fut = test_spawn(sub_str.recv());
        assert!(!recv_u32_fut.is_woken());
        assert!(!recv_str_fut.is_woken());
        assert_pending!(recv_u32_fut.poll());
        assert_pending!(recv_str_fut.poll());

        // Create an instrumented future that asserts values of two different types.
        let mut worker = test_spawn(process.into_process_future(async {
            let ds = DataspaceRegistry::try_current().expect("dataspace registry should be available");
            ds.assert(42u32, id_num);
            ds.assert("hello".to_string(), id_str);
            pending::<()>().await;
        }));

        // Drive the worker to assert the values.
        assert_pending!(worker.poll());

        // Both subscribers should now be woken and have the assertion updates.
        assert!(recv_u32_fut.is_woken());
        assert!(recv_str_fut.is_woken());
        assert_ready_eq!(recv_u32_fut.poll(), Some(AssertionUpdate::Asserted(id_num.into(), 42)));
        assert_ready_eq!(
            recv_str_fut.poll(),
            Some(AssertionUpdate::Asserted(id_str.into(), "hello".to_string()))
        );

        drop(recv_u32_fut);
        drop(recv_str_fut);

        // Set up new subscriber calls for getting the retractions.
        let mut recv_u32 = test_spawn(sub_u32.recv());
        let mut recv_str = test_spawn(sub_str.recv());
        assert_pending!(recv_u32.poll());
        assert_pending!(recv_str.poll());
        assert!(!recv_u32.is_woken());
        assert!(!recv_str.is_woken());

        // Drop the worker — both types should be retracted.
        drop(worker);

        assert!(recv_u32.is_woken());
        assert!(recv_str.is_woken());
        assert_ready_eq!(recv_u32.poll(), Some(AssertionUpdate::Retracted(id_num.into())));
        assert_ready_eq!(recv_str.poll(), Some(AssertionUpdate::Retracted(id_str.into())));
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn retract_from_non_owner_is_ignored() {
        let registry = DataspaceRegistry::new();
        let pid_a = Id::new();
        let pid_b = Id::new();
        let id = Identifier::named("owned_by_a");

        // Assert as process A.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(42u32, id.clone());
        });

        // Attempt to retract as process B -- should be silently ignored.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_b, || {
            registry.retract::<u32>(id.clone());
        });

        // Value should still be present for new subscribers.
        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv);

        // Retract as process A -- should succeed.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.retract::<u32>(id.clone());
        });

        // Subscriber should receive the retraction.
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Retracted(id)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "attempted to retract assertion owned by")]
    fn retract_from_non_owner_panics_in_debug() {
        let registry = DataspaceRegistry::new();
        let pid_a = Id::new();
        let pid_b = Id::new();
        let id = Identifier::named("owned_by_a");

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(42u32, id.clone());
        });

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_b, || {
            registry.retract::<u32>(id.clone());
        });
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn reassert_from_non_owner_is_ignored() {
        let registry = DataspaceRegistry::new();
        let pid_a = Id::new();
        let pid_b = Id::new();
        let id = Identifier::named("owned_by_a");

        // Assert as process A.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(42u32, id.clone());
        });

        // Attempt to overwrite as process B -- should be silently ignored.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_b, || {
            registry.assert(99u32, id.clone());
        });

        // Value should still be the original.
        let mut sub = registry.subscribe::<u32>(IdentifierFilter::exact(id.clone()));
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id.clone(), 42)));
        drop(recv);

        // Update as process A -- should succeed.
        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(100u32, id.clone());
        });

        // Subscriber should receive the update.
        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(recv.poll(), Some(AssertionUpdate::Asserted(id, 100)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "attempted to update assertion owned by")]
    fn reassert_from_non_owner_panics_in_debug() {
        let registry = DataspaceRegistry::new();
        let pid_a = Id::new();
        let pid_b = Id::new();
        let id = Identifier::named("owned_by_a");

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_a, || {
            registry.assert(42u32, id.clone());
        });

        crate::runtime::process::CURRENT_PROCESS_ID.sync_scope(pid_b, || {
            registry.assert(99u32, id.clone());
        });
    }
}

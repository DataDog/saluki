//! A type-erased, async-aware publish/subscribe registry for inter-process coordination.
//!
//! The [`PubSubRegistry`] allows processes to publish typed values by handle and subscribe to receive those values,
//! with async waiting for new values. Multiple subscribers can receive the same published value.
//!
//! This enables decoupled coordination where processes don't need to know about each other, only the handle and
//! type of the values they're exchanging.
//!
//! # Example
//!
//! ```
//! use saluki_core::runtime::state::{Handle, PubSubRegistry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let registry = PubSubRegistry::new();
//! let handle = Handle::new_global();
//!
//! // Subscribe before publishing
//! let mut sub = registry.subscribe::<u32>(handle);
//!
//! // Publish a value
//! registry.publish(42u32, handle).unwrap();
//!
//! // Receive the value
//! let value = sub.recv().await;
//! assert_eq!(value, Some(42));
//! # }
//! ```

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use snafu::Snafu;
use tokio::sync::broadcast;

use super::Handle;

const DEFAULT_CHANNEL_CAPACITY: usize = 16;

/// Errors that can occur when publishing to the registry.
#[derive(Debug, Snafu)]
pub enum PubSubPublishError {
    /// No subscribers exist for the given type and handle.
    #[snafu(display("No subscribers exist for type '{}' with handle {:?}", type_name, handle))]
    NoSubscribers {
        /// The name of the type.
        type_name: &'static str,
        /// The handle.
        handle: Handle,
    },
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
    channels: HashMap<StorageKey, Box<dyn Any + Send + Sync>>,

    /// Default capacity for new broadcast channels.
    channel_capacity: usize,
}

impl RegistryState {
    fn new(channel_capacity: usize) -> Self {
        Self {
            channels: HashMap::new(),
            channel_capacity,
        }
    }

    /// Gets or creates a broadcast sender for the given key, returning a new receiver.
    fn get_or_create_sender<T>(&mut self, key: StorageKey) -> broadcast::Receiver<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let sender = self
            .channels
            .entry(key)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel::<T>(self.channel_capacity);
                Box::new(tx)
            })
            .downcast_ref::<broadcast::Sender<T>>()
            .expect("type mismatch in pub/sub registry");

        sender.subscribe()
    }
}

/// Shared inner state of the registry.
struct PubSubRegistryInner {
    state: Mutex<RegistryState>,
}

/// A publish/subscribe registry for async coordination between processes.
///
/// The registry stores broadcast channels indexed by type and [`Handle`]. Processes can subscribe to receive
/// values of a given type and handle, and other processes can publish values that are delivered to all current
/// subscribers.
///
/// # Thread Safety
///
/// `PubSubRegistry` is `Clone` and can be safely shared across threads and tasks. All operations are thread-safe.
#[derive(Clone)]
pub struct PubSubRegistry {
    inner: Arc<PubSubRegistryInner>,
}

impl Default for PubSubRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PubSubRegistry {
    /// Creates a new empty registry with the default channel capacity.
    pub fn new() -> Self {
        Self::with_channel_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    /// Creates a new empty registry with the given channel capacity for broadcast channels.
    pub fn with_channel_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(PubSubRegistryInner {
                state: Mutex::new(RegistryState::new(capacity)),
            }),
        }
    }

    /// Publishes a value with the given handle to all current subscribers.
    ///
    /// The value is delivered to every active [`Subscription`] for the same type and handle.
    ///
    /// # Errors
    ///
    /// Returns an error if no subscribers exist for the given type and handle.
    pub fn publish<T>(&self, value: T, handle: Handle) -> Result<(), PubSubPublishError>
    where
        T: Clone + Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

        let state = self.inner.state.lock().unwrap();

        let sender = state
            .channels
            .get(&key)
            .and_then(|boxed| boxed.downcast_ref::<broadcast::Sender<T>>())
            .ok_or(PubSubPublishError::NoSubscribers {
                type_name: std::any::type_name::<T>(),
                handle,
            })?;

        // Ignore the receiver count returned by send. If all receivers have been dropped since we
        // checked, the value is simply lost — that's acceptable in pub/sub semantics.
        let _ = sender.send(value);

        Ok(())
    }

    /// Subscribes to values of the given type and handle.
    ///
    /// Returns a [`Subscription`] that can be used to asynchronously receive published values. If no broadcast channel
    /// exists yet for this (type, handle) pair, one is created.
    pub fn subscribe<T>(&self, handle: Handle) -> Subscription<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

        let mut state = self.inner.state.lock().unwrap();
        let rx = state.get_or_create_sender::<T>(key);

        Subscription { rx }
    }
}

/// A subscription to values from a [`PubSubRegistry`].
///
/// Created by calling [`PubSubRegistry::subscribe`]. Use [`recv`](Subscription::recv) to asynchronously wait for the
/// next published value.
pub struct Subscription<T> {
    rx: broadcast::Receiver<T>,
}

impl<T> Subscription<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// Receives the next published value.
    ///
    /// Returns `Some(value)` when a value is available, or `None` if the channel has been closed (all publishers
    /// dropped). If messages were missed due to the subscriber falling behind, the missed messages are skipped and the
    /// next available value is returned.
    pub async fn recv(&mut self) -> Option<T> {
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
    async fn subscribe_then_publish() {
        let registry = PubSubRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);
        registry.publish(42u32, h).unwrap();

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(42));
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_value() {
        let registry = PubSubRegistry::new();
        let h = Handle::new_global();

        let mut sub1 = registry.subscribe::<u32>(h);
        let mut sub2 = registry.subscribe::<u32>(h);

        registry.publish(42u32, h).unwrap();

        let v1 = timeout(Duration::from_millis(100), sub1.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub2.recv()).await.expect("timeout");

        assert_eq!(v1, Some(42));
        assert_eq!(v2, Some(42));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_returns_error() {
        let registry = PubSubRegistry::new();
        let h = Handle::new_global();

        let result = registry.publish(42u32, h);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn different_types_same_handle() {
        let registry = PubSubRegistry::new();
        let h = Handle::new_global();

        let mut sub_u32 = registry.subscribe::<u32>(h);
        let mut sub_string = registry.subscribe::<String>(h);

        registry.publish(42u32, h).unwrap();
        registry.publish("hello".to_string(), h).unwrap();

        let v1 = timeout(Duration::from_millis(100), sub_u32.recv())
            .await
            .expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub_string.recv())
            .await
            .expect("timeout");

        assert_eq!(v1, Some(42));
        assert_eq!(v2, Some("hello".to_string()));
    }

    #[tokio::test]
    async fn process_handle() {
        let registry = PubSubRegistry::new();
        let pid = crate::runtime::process::Id::new();
        let h = Handle::for_process(pid);

        let mut sub = registry.subscribe::<u32>(h);
        registry.publish(42u32, h).unwrap();

        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert_eq!(value, Some(42));
    }

    #[tokio::test]
    async fn channel_closed_returns_none() {
        let registry = PubSubRegistry::new();
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
        let registry = PubSubRegistry::with_channel_capacity(2);
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        // Publish more values than the channel can hold.
        for i in 0..10 {
            registry.publish(i as u32, h).unwrap();
        }

        // The subscriber should skip lagged messages and still receive a value.
        let value = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        assert!(value.is_some());
    }

    #[tokio::test]
    async fn multiple_values_received_in_order() {
        let registry = PubSubRegistry::new();
        let h = Handle::new_global();

        let mut sub = registry.subscribe::<u32>(h);

        registry.publish(1u32, h).unwrap();
        registry.publish(2u32, h).unwrap();
        registry.publish(3u32, h).unwrap();

        let v1 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v2 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");
        let v3 = timeout(Duration::from_millis(100), sub.recv()).await.expect("timeout");

        assert_eq!(v1, Some(1));
        assert_eq!(v2, Some(2));
        assert_eq!(v3, Some(3));
    }
}

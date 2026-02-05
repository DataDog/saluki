//! A type-erased, async-aware registry for inter-process coordination.
//!
//! The [`ResourceRegistry`] allows processes to publish and acquire typed resources by identifier, with async waiting
//! when resources aren't yet available. Acquired resources are temporarily borrowed via a guard—when the guard is
//! dropped, the resource returns to the registry.
//!
//! This enables decoupled coordination where processes don't need to know about each other, only the identifier and
//! type of the resource they're exchanging.
//!
//! # Example
//!
//! ```
//! use saluki_core::runtime::state::{Identifier, ResourceRegistry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let registry = ResourceRegistry::new();
//!
//! // Publish a resource
//! registry.publish(42u32, "my-resource").unwrap();
//!
//! // Acquire the resource (returns immediately since it's available)
//! let guard = registry.acquire::<u32>("my-resource").await;
//! assert_eq!(*guard, 42);
//!
//! // Resource is returned to registry when guard is dropped
//! drop(guard);
//!
//! // Can acquire again
//! let guard = registry.try_acquire::<u32>("my-resource");
//! assert!(guard.is_some());
//! # }
//! ```

use std::{
    any::{Any, TypeId},
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use snafu::Snafu;
use stringtheory::MetaString;
use tokio::sync::oneshot;

/// Identifier for resources in the registry.
///
/// Resources are keyed by both their Rust type and an identifier. The identifier can be either a string or an integer,
/// allowing flexibility in how resources are named.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Identifier {
    /// A string identifier.
    String(MetaString),
    /// An integer identifier.
    Integer(u64),
}

impl From<&str> for Identifier {
    fn from(s: &str) -> Self {
        Self::String(MetaString::from(s))
    }
}

impl From<String> for Identifier {
    fn from(s: String) -> Self {
        Self::String(MetaString::from(s))
    }
}

impl From<MetaString> for Identifier {
    fn from(s: MetaString) -> Self {
        Self::String(s)
    }
}

impl From<u64> for Identifier {
    fn from(n: u64) -> Self {
        Self::Integer(n)
    }
}

impl From<i64> for Identifier {
    fn from(n: i64) -> Self {
        Self::Integer(n as u64)
    }
}

impl From<u32> for Identifier {
    fn from(n: u32) -> Self {
        Self::Integer(n as u64)
    }
}

impl From<i32> for Identifier {
    fn from(n: i32) -> Self {
        Self::Integer(n as u64)
    }
}

impl From<usize> for Identifier {
    fn from(n: usize) -> Self {
        Self::Integer(n as u64)
    }
}

/// Errors that can occur when publishing to the registry.
#[derive(Debug, Snafu)]
pub enum PublishError {
    /// A resource with this type and identifier already exists in the registry.
    #[snafu(display("Resource already exists for type '{}' with identifier {:?}", type_name, identifier))]
    AlreadyExists {
        /// The name of the type.
        type_name: &'static str,
        /// The identifier.
        identifier: Identifier,
    },
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

/// Internal registry state protected by a mutex.
struct RegistryState {
    /// Available resources ready for acquisition.
    available: HashMap<StorageKey, Box<dyn Any + Send + Sync>>,

    /// Keys of resources currently acquired (held by guards).
    acquired: HashSet<StorageKey>,

    /// Waiters for resources not yet available.
    waiters: HashMap<StorageKey, VecDeque<oneshot::Sender<Box<dyn Any + Send + Sync>>>>,

    /// Type names for observability (TypeId -> type_name).
    type_names: HashMap<TypeId, &'static str>,
}

impl RegistryState {
    fn new() -> Self {
        Self {
            available: HashMap::new(),
            acquired: HashSet::new(),
            waiters: HashMap::new(),
            type_names: HashMap::new(),
        }
    }

    fn register_type<T: 'static>(&mut self) {
        self.type_names
            .entry(TypeId::of::<T>())
            .or_insert_with(std::any::type_name::<T>);
    }

    fn type_name(&self, type_id: TypeId) -> &'static str {
        self.type_names.get(&type_id).copied().unwrap_or("<unknown>")
    }

    /// Check if a resource exists (either available or acquired).
    fn resource_exists(&self, key: &StorageKey) -> bool {
        self.available.contains_key(key) || self.acquired.contains(key)
    }

    /// Try to send a boxed resource to the first available waiter.
    ///
    /// Returns `Some(boxed_resource)` if no waiter accepted the resource (all dropped or none exist).
    /// Returns `None` if a waiter accepted the resource (and key is marked as acquired).
    fn try_send_to_waiter(
        &mut self, key: &StorageKey, boxed: Box<dyn Any + Send + Sync>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        // Loop through waiters until one accepts the value or we run out
        let mut current_boxed = boxed;
        while let Some(waiters) = self.waiters.get_mut(key) {
            if let Some(sender) = waiters.pop_front() {
                // Clean up empty waiter queue before sending
                if waiters.is_empty() {
                    self.waiters.remove(key);
                }

                // Try to send to this waiter
                match sender.send(current_boxed) {
                    Ok(()) => {
                        // Waiter accepted, mark as acquired
                        self.acquired.insert(key.clone());
                        return None;
                    }
                    Err(returned_boxed) => {
                        // Waiter was dropped, try next one
                        current_boxed = returned_boxed;
                    }
                }
            } else {
                // No more waiters in the queue
                self.waiters.remove(key);
                break;
            }
        }

        // No waiter accepted the value
        Some(current_boxed)
    }
}

/// Shared inner state of the registry.
struct ResourceRegistryInner {
    state: Mutex<RegistryState>,
}

impl ResourceRegistryInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(RegistryState::new()),
        }
    }

    /// Return a boxed resource to the registry (called when a guard is dropped).
    fn return_boxed_resource(&self, key: StorageKey, boxed: Box<dyn Any + Send + Sync>) {
        let mut state = self.state.lock().unwrap();

        // Remove from acquired set
        state.acquired.remove(&key);

        // Try to send to a waiter, or store as available
        if let Some(boxed) = state.try_send_to_waiter(&key, boxed) {
            state.available.insert(key, boxed);
        }
    }
}

/// A resource registry for async coordination between processes.
///
/// The registry stores typed resources indexed by an [`Identifier`]. Processes can publish resources and acquire them
/// asynchronously. When a resource is acquired, it is temporarily removed from the registry and returned via a
/// [`ResourceGuard`]. When the guard is dropped, the resource is automatically returned to the registry.
///
/// # Thread Safety
///
/// `ResourceRegistry` is `Clone` and can be safely shared across threads and tasks. All operations are thread-safe.
#[derive(Clone)]
pub struct ResourceRegistry {
    inner: Arc<ResourceRegistryInner>,
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceRegistry {
    /// Creates a new empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ResourceRegistryInner::new()),
        }
    }

    /// Publishes a resource with the given identifier.
    ///
    /// The resource becomes available for acquisition by other processes.
    ///
    /// # Errors
    ///
    /// Returns an error if a resource with the same type and identifier already exists in the registry (whether
    /// available or currently acquired).
    pub fn publish<T>(&self, resource: T, identifier: impl Into<Identifier>) -> Result<(), PublishError>
    where
        T: Send + Sync + 'static,
    {
        let identifier = identifier.into();
        let key = StorageKey::new::<T>(identifier.clone());

        let mut state = self.inner.state.lock().unwrap();
        state.register_type::<T>();

        // Check if resource already exists
        if state.resource_exists(&key) {
            return Err(PublishError::AlreadyExists {
                type_name: std::any::type_name::<T>(),
                identifier,
            });
        }

        let boxed: Box<dyn Any + Send + Sync> = Box::new(resource);

        // Try to send to a waiter, or store as available
        if let Some(boxed) = state.try_send_to_waiter(&key, boxed) {
            state.available.insert(key, boxed);
        }

        Ok(())
    }

    /// Tries to acquire a resource immediately without waiting.
    ///
    /// Returns `Some(guard)` if the resource is available, `None` otherwise.
    pub fn try_acquire<T>(&self, identifier: impl Into<Identifier>) -> Option<ResourceGuard<T>>
    where
        T: Send + Sync + 'static,
    {
        let identifier = identifier.into();
        let key = StorageKey::new::<T>(identifier);

        let mut state = self.inner.state.lock().unwrap();
        state.register_type::<T>();

        // Try to get the resource
        if let Some(boxed) = state.available.remove(&key) {
            state.acquired.insert(key.clone());
            let resource = *boxed.downcast::<T>().expect("type mismatch in registry");
            return Some(ResourceGuard {
                resource: Some(resource),
                registry: Arc::clone(&self.inner),
                key,
            });
        }

        None
    }

    /// Acquires a resource with the given identifier, waiting if not yet available.
    ///
    /// Returns a guard that provides access to the resource. When the guard is dropped, the resource is returned to the
    /// registry and becomes available for acquisition again.
    pub async fn acquire<T>(&self, identifier: impl Into<Identifier>) -> ResourceGuard<T>
    where
        T: Send + Sync + 'static,
    {
        let identifier = identifier.into();
        let key = StorageKey::new::<T>(identifier);

        loop {
            let rx = {
                let mut state = self.inner.state.lock().unwrap();
                state.register_type::<T>();

                // Try to acquire synchronously
                if let Some(boxed) = state.available.remove(&key) {
                    state.acquired.insert(key.clone());
                    let resource = *boxed.downcast::<T>().expect("type mismatch in registry");
                    return ResourceGuard {
                        resource: Some(resource),
                        registry: Arc::clone(&self.inner),
                        key,
                    };
                }

                // Not available, register as waiter
                let (tx, rx) = oneshot::channel();
                state.waiters.entry(key.clone()).or_default().push_back(tx);
                rx
            };

            // Wait for resource (lock released)
            match rx.await {
                Ok(boxed) => {
                    let resource = *boxed.downcast::<T>().expect("type mismatch in registry");
                    return ResourceGuard {
                        resource: Some(resource),
                        registry: Arc::clone(&self.inner),
                        key,
                    };
                }
                Err(_) => {
                    // Sender dropped without sending - this can happen if the registry
                    // is being cleaned up. Retry the acquire.
                    continue;
                }
            }
        }
    }

    /// Returns a snapshot of the registry state for observability.
    ///
    /// The snapshot provides information about available resources, acquired resources, and pending requests.
    pub fn snapshot(&self) -> ResourceRegistrySnapshot {
        let state = self.inner.state.lock().unwrap();

        let available_resources = state
            .available
            .keys()
            .map(|key| ResourceInfo {
                type_name: state.type_name(key.type_id),
                identifier: key.identifier.clone(),
            })
            .collect();

        let acquired_resources = state
            .acquired
            .iter()
            .map(|key| ResourceInfo {
                type_name: state.type_name(key.type_id),
                identifier: key.identifier.clone(),
            })
            .collect();

        let pending_requests = state
            .waiters
            .iter()
            .map(|(key, waiters)| PendingRequestInfo {
                type_name: state.type_name(key.type_id),
                identifier: key.identifier.clone(),
                waiter_count: waiters.len(),
            })
            .collect();

        ResourceRegistrySnapshot {
            available_resources,
            acquired_resources,
            pending_requests,
        }
    }
}

/// A guard providing temporary access to a resource from the registry.
///
/// When dropped, the resource is automatically returned to the registry and becomes available for acquisition again (or
/// is sent directly to the next waiter if one exists).
///
/// The guard provides both immutable and mutable access to the resource via [`Deref`] and [`DerefMut`].
pub struct ResourceGuard<T: Send + Sync + 'static> {
    resource: Option<T>,
    registry: Arc<ResourceRegistryInner>,
    key: StorageKey,
}

impl<T: Send + Sync + 'static> Deref for ResourceGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.resource.as_ref().expect("ResourceGuard resource already taken")
    }
}

impl<T: Send + Sync + 'static> DerefMut for ResourceGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.resource.as_mut().expect("ResourceGuard resource already taken")
    }
}

impl<T: Send + Sync + 'static> Drop for ResourceGuard<T> {
    fn drop(&mut self) {
        if let Some(resource) = self.resource.take() {
            let boxed: Box<dyn Any + Send + Sync> = Box::new(resource);
            self.registry.return_boxed_resource(self.key.clone(), boxed);
        }
    }
}

/// A snapshot of the registry state for debugging/observability.
#[derive(Debug)]
pub struct ResourceRegistrySnapshot {
    /// Resources currently available (not acquired).
    pub available_resources: Vec<ResourceInfo>,
    /// Resources currently acquired (held by guards).
    pub acquired_resources: Vec<ResourceInfo>,
    /// Outstanding acquire requests (waiters).
    pub pending_requests: Vec<PendingRequestInfo>,
}

/// Information about a resource in the registry.
#[derive(Debug, Clone)]
pub struct ResourceInfo {
    /// The name of the resource's type.
    pub type_name: &'static str,
    /// The identifier of the resource.
    pub identifier: Identifier,
}

/// Information about pending acquire requests.
#[derive(Debug, Clone)]
pub struct PendingRequestInfo {
    /// The name of the requested type.
    pub type_name: &'static str,
    /// The identifier being requested.
    pub identifier: Identifier,
    /// The number of waiters for this (type, identifier) pair.
    pub waiter_count: usize,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[test]
    fn basic_publish_then_acquire() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();

        let guard = registry.try_acquire::<u32>("test");
        assert!(guard.is_some());
        assert_eq!(*guard.unwrap(), 42);
    }

    #[tokio::test]
    async fn acquire_then_publish_async_waiting() {
        let registry = ResourceRegistry::new();
        let registry_clone = registry.clone();

        // Spawn a task that will acquire (and wait)
        let acquire_handle = tokio::spawn(async move { registry_clone.acquire::<u32>("test").await });

        // Give the acquire task time to register as waiter
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Publish the resource
        registry.publish(42u32, "test").unwrap();

        // The acquire should complete
        let guard = timeout(Duration::from_millis(100), acquire_handle)
            .await
            .expect("timeout")
            .expect("join error");

        assert_eq!(*guard, 42);
    }

    #[test]
    fn guard_drop_returns_resource() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();

        // Acquire and drop
        {
            let guard = registry.try_acquire::<u32>("test").unwrap();
            assert_eq!(*guard, 42);
        }

        // Should be able to acquire again
        let guard = registry.try_acquire::<u32>("test");
        assert!(guard.is_some());
        assert_eq!(*guard.unwrap(), 42);
    }

    #[test]
    fn publish_duplicate_returns_error() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();

        let result = registry.publish(100u32, "test");
        assert!(result.is_err());

        // Original resource should still be there
        let guard = registry.try_acquire::<u32>("test").unwrap();
        assert_eq!(*guard, 42);
    }

    #[test]
    fn publish_while_acquired_returns_error() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();

        let _guard = registry.try_acquire::<u32>("test").unwrap();

        // Try to publish while acquired
        let result = registry.publish(100u32, "test");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_waiters_fifo() {
        let registry = ResourceRegistry::new();

        // Spawn multiple waiters
        let registry1 = registry.clone();
        let handle1 = tokio::spawn(async move { registry1.acquire::<u32>("test").await });

        tokio::time::sleep(Duration::from_millis(5)).await;

        let registry2 = registry.clone();
        let handle2 = tokio::spawn(async move { registry2.acquire::<u32>("test").await });

        tokio::time::sleep(Duration::from_millis(5)).await;

        // Publish first resource - should go to first waiter
        registry.publish(1u32, "test").unwrap();

        let guard1 = timeout(Duration::from_millis(100), handle1)
            .await
            .expect("timeout")
            .expect("join error");
        assert_eq!(*guard1, 1);

        // Second waiter should still be waiting
        assert!(!handle2.is_finished());

        // Drop first guard - resource returns to registry, should go to second waiter
        drop(guard1);

        // The returned resource should go to second waiter
        let guard2 = timeout(Duration::from_millis(100), handle2)
            .await
            .expect("timeout")
            .expect("join error");
        assert_eq!(*guard2, 1);
    }

    #[test]
    fn different_types_same_identifier() {
        let registry = ResourceRegistry::new();

        // Publish different types with same identifier
        registry.publish(42u32, "test").unwrap();
        registry.publish("hello".to_string(), "test").unwrap();

        // Acquire each type
        let guard_u32 = registry.try_acquire::<u32>("test").unwrap();
        let guard_string = registry.try_acquire::<String>("test").unwrap();

        assert_eq!(*guard_u32, 42);
        assert_eq!(*guard_string, "hello");
    }

    #[test]
    fn try_acquire_returns_none_when_not_available() {
        let registry = ResourceRegistry::new();

        let guard = registry.try_acquire::<u32>("nonexistent");
        assert!(guard.is_none());
    }

    #[test]
    fn try_acquire_returns_none_when_acquired() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();
        let _guard = registry.try_acquire::<u32>("test").unwrap();

        // Try to acquire while held
        let second = registry.try_acquire::<u32>("test");
        assert!(second.is_none());
    }

    #[test]
    fn snapshot_shows_available() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 1);
        assert_eq!(snapshot.acquired_resources.len(), 0);
        assert_eq!(snapshot.pending_requests.len(), 0);
    }

    #[test]
    fn snapshot_shows_acquired() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, "test").unwrap();
        let _guard = registry.try_acquire::<u32>("test").unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 0);
        assert_eq!(snapshot.acquired_resources.len(), 1);
        assert_eq!(snapshot.pending_requests.len(), 0);
    }

    #[tokio::test]
    async fn snapshot_shows_pending() {
        let registry = ResourceRegistry::new();
        let registry_clone = registry.clone();

        // Spawn a waiter
        let _handle = tokio::spawn(async move { registry_clone.acquire::<u32>("test").await });

        // Give it time to register
        tokio::time::sleep(Duration::from_millis(10)).await;

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 0);
        assert_eq!(snapshot.acquired_resources.len(), 0);
        assert_eq!(snapshot.pending_requests.len(), 1);
        assert_eq!(snapshot.pending_requests[0].waiter_count, 1);
    }

    #[test]
    fn integer_identifier() {
        let registry = ResourceRegistry::new();

        registry.publish(42u32, 123u64).unwrap();

        let guard = registry.try_acquire::<u32>(123u64).unwrap();
        assert_eq!(*guard, 42);
    }

    #[test]
    fn mutable_access_through_guard() {
        let registry = ResourceRegistry::new();

        registry.publish(vec![1, 2, 3], "test").unwrap();

        {
            let mut guard = registry.try_acquire::<Vec<i32>>("test").unwrap();
            guard.push(4);
        }

        let guard = registry.try_acquire::<Vec<i32>>("test").unwrap();
        assert_eq!(*guard, vec![1, 2, 3, 4]);
    }
}

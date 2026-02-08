//! A type-erased, async-aware registry for inter-process coordination.
//!
//! The [`ResourceRegistry`] allows processes to publish and acquire typed resources by handle, with async waiting
//! when resources aren't yet available. Acquired resources are temporarily borrowed via a guard—when the guard is
//! dropped, the resource returns to the registry.
//!
//! This enables decoupled coordination where processes don't need to know about each other, only the handle and
//! type of the resource they're exchanging.
//!
//! # Example
//!
//! ```
//! use saluki_core::runtime::state::{Handle, ResourceRegistry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let registry = ResourceRegistry::new();
//!
//! // Publish a resource
//! let handle = Handle::new_global();
//! registry.publish(42u32, handle).unwrap();
//!
//! // Acquire the resource (returns immediately since it's available)
//! let guard = registry.acquire::<u32>(handle).await;
//! assert_eq!(*guard, 42);
//!
//! // Resource is returned to registry when guard is dropped
//! drop(guard);
//!
//! // Can acquire again
//! let guard = registry.try_acquire::<u32>(handle);
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
use tokio::sync::oneshot;

use super::Handle;

/// Errors that can occur when publishing to the registry.
#[derive(Debug, Snafu)]
pub enum PublishError {
    /// A resource with this type and handle already exists in the registry.
    #[snafu(display("Resource already exists for type '{}' with handle {:?}", type_name, handle))]
    AlreadyExists {
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
/// The registry stores typed resources indexed by a [`Handle`]. Processes can publish resources and acquire them
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

    /// Publishes a resource with the given handle.
    ///
    /// The resource becomes available for acquisition by other processes.
    ///
    /// # Errors
    ///
    /// Returns an error if a resource with the same type and handle already exists in the registry (whether
    /// available or currently acquired).
    pub fn publish<T>(&self, resource: T, handle: Handle) -> Result<(), PublishError>
    where
        T: Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

        let mut state = self.inner.state.lock().unwrap();
        state.register_type::<T>();

        // Check if resource already exists
        if state.resource_exists(&key) {
            return Err(PublishError::AlreadyExists {
                type_name: std::any::type_name::<T>(),
                handle,
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
    pub fn try_acquire<T>(&self, handle: Handle) -> Option<ResourceGuard<T>>
    where
        T: Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

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

    /// Acquires a resource with the given handle, waiting if not yet available.
    ///
    /// Returns a guard that provides access to the resource. When the guard is dropped, the resource is returned to the
    /// registry and becomes available for acquisition again.
    pub async fn acquire<T>(&self, handle: Handle) -> ResourceGuard<T>
    where
        T: Send + Sync + 'static,
    {
        let key = StorageKey::new::<T>(handle);

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
                handle: key.handle,
            })
            .collect();

        let acquired_resources = state
            .acquired
            .iter()
            .map(|key| ResourceInfo {
                type_name: state.type_name(key.type_id),
                handle: key.handle,
            })
            .collect();

        let pending_requests = state
            .waiters
            .iter()
            .map(|(key, waiters)| PendingRequestInfo {
                type_name: state.type_name(key.type_id),
                handle: key.handle,
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
    /// The handle of the resource.
    pub handle: Handle,
}

/// Information about pending acquire requests.
#[derive(Debug, Clone)]
pub struct PendingRequestInfo {
    /// The name of the requested type.
    pub type_name: &'static str,
    /// The handle being requested.
    pub handle: Handle,
    /// The number of waiters for this (type, handle) pair.
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
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();

        let guard = registry.try_acquire::<u32>(h);
        assert!(guard.is_some());
        assert_eq!(*guard.unwrap(), 42);
    }

    #[tokio::test]
    async fn acquire_then_publish_async_waiting() {
        let registry = ResourceRegistry::new();
        let registry_clone = registry.clone();
        let h = Handle::new_global();

        // Spawn a task that will acquire (and wait)
        let acquire_handle = tokio::spawn(async move { registry_clone.acquire::<u32>(h).await });

        // Give the acquire task time to register as waiter
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Publish the resource
        registry.publish(42u32, h).unwrap();

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
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();

        // Acquire and drop
        {
            let guard = registry.try_acquire::<u32>(h).unwrap();
            assert_eq!(*guard, 42);
        }

        // Should be able to acquire again
        let guard = registry.try_acquire::<u32>(h);
        assert!(guard.is_some());
        assert_eq!(*guard.unwrap(), 42);
    }

    #[test]
    fn publish_duplicate_returns_error() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();

        let result = registry.publish(100u32, h);
        assert!(result.is_err());

        // Original resource should still be there
        let guard = registry.try_acquire::<u32>(h).unwrap();
        assert_eq!(*guard, 42);
    }

    #[test]
    fn publish_while_acquired_returns_error() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();

        let _guard = registry.try_acquire::<u32>(h).unwrap();

        // Try to publish while acquired
        let result = registry.publish(100u32, h);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_waiters_fifo() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        // Spawn multiple waiters
        let registry1 = registry.clone();
        let handle1 = tokio::spawn(async move { registry1.acquire::<u32>(h).await });

        tokio::time::sleep(Duration::from_millis(5)).await;

        let registry2 = registry.clone();
        let handle2 = tokio::spawn(async move { registry2.acquire::<u32>(h).await });

        tokio::time::sleep(Duration::from_millis(5)).await;

        // Publish first resource - should go to first waiter
        registry.publish(1u32, h).unwrap();

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
    fn different_types_same_handle() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        // Publish different types with same handle
        registry.publish(42u32, h).unwrap();
        registry.publish("hello".to_string(), h).unwrap();

        // Acquire each type
        let guard_u32 = registry.try_acquire::<u32>(h).unwrap();
        let guard_string = registry.try_acquire::<String>(h).unwrap();

        assert_eq!(*guard_u32, 42);
        assert_eq!(*guard_string, "hello");
    }

    #[test]
    fn try_acquire_returns_none_when_not_available() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        let guard = registry.try_acquire::<u32>(h);
        assert!(guard.is_none());
    }

    #[test]
    fn try_acquire_returns_none_when_acquired() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();
        let _guard = registry.try_acquire::<u32>(h).unwrap();

        // Try to acquire while held
        let second = registry.try_acquire::<u32>(h);
        assert!(second.is_none());
    }

    #[test]
    fn snapshot_shows_available() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 1);
        assert_eq!(snapshot.acquired_resources.len(), 0);
        assert_eq!(snapshot.pending_requests.len(), 0);
    }

    #[test]
    fn snapshot_shows_acquired() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(42u32, h).unwrap();
        let _guard = registry.try_acquire::<u32>(h).unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 0);
        assert_eq!(snapshot.acquired_resources.len(), 1);
        assert_eq!(snapshot.pending_requests.len(), 0);
    }

    #[tokio::test]
    async fn snapshot_shows_pending() {
        let registry = ResourceRegistry::new();
        let registry_clone = registry.clone();
        let h = Handle::new_global();

        // Spawn a waiter
        let _handle = tokio::spawn(async move { registry_clone.acquire::<u32>(h).await });

        // Give it time to register
        tokio::time::sleep(Duration::from_millis(10)).await;

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.available_resources.len(), 0);
        assert_eq!(snapshot.acquired_resources.len(), 0);
        assert_eq!(snapshot.pending_requests.len(), 1);
        assert_eq!(snapshot.pending_requests[0].waiter_count, 1);
    }

    #[test]
    fn mutable_access_through_guard() {
        let registry = ResourceRegistry::new();
        let h = Handle::new_global();

        registry.publish(vec![1, 2, 3], h).unwrap();

        {
            let mut guard = registry.try_acquire::<Vec<i32>>(h).unwrap();
            guard.push(4);
        }

        let guard = registry.try_acquire::<Vec<i32>>(h).unwrap();
        assert_eq!(*guard, vec![1, 2, 3, 4]);
    }
}

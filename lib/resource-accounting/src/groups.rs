use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    sync::{Mutex, OnceLock},
    task::{Context, Poll},
};

use pin_project::pin_project;

use super::stats::thread_cpu_time_nanos;
use crate::ResourceStats;

static REGISTRY: OnceLock<ResourceGroupRegistry> = OnceLock::new();
static ROOT_GROUP: ResourceStats = ResourceStats::new();

thread_local! {
    pub(super) static CURRENT_GROUP: RefCell<NonNull<ResourceStats>> = RefCell::new(NonNull::from(&ROOT_GROUP));
}

/// A token associated with a specific resource group.
///
/// Used to attribute allocations and deallocations to a specific group with a scope guard [`TrackingGuard`], or
/// through helpers provided by the [`Track`] trait.
#[derive(Clone, Copy)]
pub struct ResourceGroupToken {
    group_ptr: NonNull<ResourceStats>,
}

impl ResourceGroupToken {
    fn new(group_ptr: NonNull<ResourceStats>) -> Self {
        Self { group_ptr }
    }

    /// Returns an `ResourceGroupToken` for the current resource group.
    pub fn current() -> Self {
        CURRENT_GROUP.with(|current_group| {
            let group_ptr = current_group.borrow();
            Self::new(*group_ptr)
        })
    }

    #[cfg(test)]
    fn ptr_eq(&self, other: &Self) -> bool {
        self.group_ptr == other.group_ptr
    }

    /// Returns the token for the root resource group.
    pub(crate) fn root() -> Self {
        Self::new(NonNull::from(&ROOT_GROUP))
    }

    /// Enters this resource group, returning a guard that will exit the resource group when dropped.
    pub fn enter(&self) -> ResourceTrackingGuard<'_> {
        // Track our starting point for this thread's CPU usage.
        let thread_cpu_usage_start = thread_cpu_time_nanos().unwrap_or(0);

        // Swap the current group to the one we're tracking.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();
            let previous_group_ptr = *group_ptr;
            *group_ptr = self.group_ptr;

            ResourceTrackingGuard {
                previous_group_ptr,
                thread_cpu_usage_start,
                _token: PhantomData,
            }
        })
    }
}

// SAFETY: There's nothing inherently thread-specific about the token.
unsafe impl Send for ResourceGroupToken {}

// SAFETY: There's nothing unsafe about sharing the token between threads, as it's safe to enter the same token on
// multiple threads at the same time, and the token itself has no internal state or interior mutability.
unsafe impl Sync for ResourceGroupToken {}

/// A guard representing an resource group which has been entered.
///
/// When the guard is dropped, the resource group will be exited and the previously entered resource group will be
/// restored.
///
/// This is returned by the [`ResourceGroupToken::enter`] method.
pub struct ResourceTrackingGuard<'a> {
    previous_group_ptr: NonNull<ResourceStats>,
    thread_cpu_usage_start: u64,
    _token: PhantomData<&'a ResourceGroupToken>,
}

impl Drop for ResourceTrackingGuard<'_> {
    fn drop(&mut self) {
        // Grab our current total CPU usage for the thread, and calculate the delta.
        let thread_cpu_usage_end = thread_cpu_time_nanos().unwrap_or(0);
        let cpu_usage_delta = thread_cpu_usage_end.saturating_sub(self.thread_cpu_usage_start);

        // Reset the current group to the one that existed before we entered.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();

            // Now track the delta in CPU usage, if available, before resetting the group.
            if cpu_usage_delta != 0 {
                // SAFETY: We only construct the pointer to `ResourceStats` from a leaked heap allocation, and we never
                // deallocate it, so it's always non-null/aligned/valid-for-`T`, etc.
                unsafe { group_ptr.as_ref().track_cpu_time(cpu_usage_delta) }
            }

            *group_ptr = self.previous_group_ptr;
        });
    }
}

/// An object wrapper that tracks allocations and attributes them to a specific group.
///
/// Provides methods and implementations to help ensure that operations against/using the wrapped object have all
/// allocations properly tracked and attributed to a given group.
///
/// Implements [`Future`] when the wrapped object itself implements [`Future`].
//
// TODO: A more complete example of this sort of thing is `tracing::Instrumented`, where they also have some fancy code
// to trace execution even in the drop logic of the wrapped future. I'm not sure we need that here, because we don't
// care about what components an object is deallocated in, and I don't think we expect to have any futures where the
// drop logic actually _allocates_, and certainly not in a way where we want to attribute it to that future's attached
// component... but for posterity, I'm mentioning it here since we _might_ consider doing it. Might.
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Tracked<Inner> {
    token: ResourceGroupToken,

    #[pin]
    inner: Inner,
}

impl<Inner> Tracked<Inner> {
    /// Consumes this object and returns the inner object and tracking token.
    pub fn into_parts(self) -> (ResourceGroupToken, Inner) {
        (self.token, self.inner)
    }
}

impl<Inner> Future for Tracked<Inner>
where
    Inner: Future,
{
    type Output = Inner::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _enter = this.token.enter();

        this.inner.poll(cx)
    }
}

/// Attaches resource groups to a [`Future`].
pub trait Track: Sized {
    /// Instruments this type by attaching the given resource group token, returning a `Tracked` wrapper.
    ///
    /// The resource group will be entered every time the wrapped future is polled.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use resource_accounting::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
    ///
    /// # async fn doc() {
    /// let future = async {
    ///     // All allocations in this future will be attached to the resource group
    ///     // represented by `token`...
    /// };
    ///
    /// let token = ResourceGroupRegistry::global().register_resource_group("my-group");
    /// future
    ///     .track_resources(token)
    ///     .await
    /// # }
    fn track_resources(self, token: ResourceGroupToken) -> Tracked<Self> {
        Tracked { token, inner: self }
    }

    /// Instruments this type by attaching the current resource group, returning a `Tracked` wrapper.
    ///
    /// The resource group will be entered every time the wrapped future is polled.
    ///
    /// This can be used to propagate the current resource group when spawning a new future.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use resource_accounting::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
    ///
    /// # mod tokio {
    /// #     pub(super) fn spawn(_: impl std::future::Future) {}
    /// # }
    /// # async fn doc() {
    /// let token = ResourceGroupRegistry::global().register_resource_group("my-group");
    /// let _enter = token.enter();
    ///
    /// // ...
    ///
    /// let future = async {
    ///     // All allocations in this future will be attached to the resource group
    ///     // represented by `token`...
    /// };
    /// tokio::spawn(future.in_current_resource_group());
    /// # }
    /// ```
    fn in_current_resource_group(self) -> Tracked<Self> {
        Tracked {
            token: ResourceGroupToken::current(),
            inner: self,
        }
    }
}

impl<T: Sized> Track for T {}

/// A registry of resource groups and the statistics for each of them.
///
/// Resource groups are user-defined groups which can then be associated with memory and CPU usage in distinct code
/// regions. This mechanism allows for granular resource accounting at the level which makes sense to the application,
/// such as per-thread, per-async task, and so on.
///
/// # Token guard
///
/// When an resource group is registered, an `ResourceGroupToken` is returned. This token can be used to "enter" the
/// group, which causes memory and CPU usage on the current thread to be attributed to that group. Entering the group
/// returns a drop guard that restores the previously entered group when dropped.
///
/// This allows for arbitrarily nested resource groups.
///
/// Additionally, [`Tracked`] can be used to wrap a [`Future`], attaching it to a specific resource group token. This
/// causes the future to track all memory and CPU usage during polls such that the usage is properly attributed to the
/// resource group.
///
/// # Resources tracked
///
/// ## Memory usage
///
/// In order for memory usage to be tracked, [`TrackingAllocator`][crate::allocator::TrackingAllocator] must be installed
/// as the global allocator for the process.
///
/// ## CPU usage
///
/// CPU usage is automatically tracked if platform support is detected.
///
/// Currently, only Linux is supported for CPU usage tracking.
pub struct ResourceGroupRegistry {
    resource_groups: Mutex<HashMap<String, Box<ResourceStats>>>,
}

impl ResourceGroupRegistry {
    fn new() -> Self {
        in_root_resource_group(|| Self {
            resource_groups: Mutex::new(HashMap::with_capacity(4)),
        })
    }

    /// Gets a reference to the global resource group registry.
    pub fn global() -> &'static Self {
        REGISTRY.get_or_init(Self::new)
    }

    /// Returns `true` if `TrackingAllocator` is installed as the global allocator.
    pub fn allocator_installed() -> bool {
        // Essentially, when we load the group registry, and it gets created for the first time, it will specifically
        // allocate its internal data structures while entered into the root resource group.
        //
        // This means that if the allocator is installed, we should always have some allocations in the root group by
        // the time we call `ResourceStats::has_allocated`.
        ROOT_GROUP.has_allocated()
    }

    /// Registers a new resource group with the given name.
    ///
    /// Returns an `ResourceGroupToken` that can be used to attribute CPU and memory usage to the
    /// newly created resource group.
    pub fn register_resource_group<S>(&self, name: S) -> ResourceGroupToken
    where
        S: AsRef<str>,
    {
        in_root_resource_group(|| {
            let mut resource_groups = self.resource_groups.lock().unwrap();
            match resource_groups.get(name.as_ref()) {
                Some(stats) => ResourceGroupToken::new(NonNull::from(&**stats)),
                None => {
                    let resource_group_stats = Box::new(ResourceStats::new());
                    let token = ResourceGroupToken::new(NonNull::from(&*resource_group_stats));

                    resource_groups.insert(name.as_ref().to_string(), resource_group_stats);

                    token
                }
            }
        })
    }

    /// Visits all resource groups in the registry and calls the given closure with their names and statistics.
    pub fn visit_resource_groups<F>(&self, mut f: F)
    where
        F: FnMut(&str, &ResourceStats),
    {
        in_root_resource_group(|| {
            f("root", &ROOT_GROUP);

            let resource_groups = self.resource_groups.lock().unwrap();
            for (name, stats) in resource_groups.iter() {
                f(name, stats);
            }
        });
    }
}

fn in_root_resource_group<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let token = ResourceGroupToken::root();
    let _enter = token.enter();
    f()
}

#[cfg(test)]
mod tests {
    use super::ResourceGroupRegistry;

    #[test]
    fn existing_group() {
        let registry = ResourceGroupRegistry::new();
        let token = registry.register_resource_group("test");
        let token2 = registry.register_resource_group("test");
        let token3 = registry.register_resource_group("test2");

        assert!(token.ptr_eq(&token2));
        assert!(!token.ptr_eq(&token3));
    }

    #[test]
    fn visit_resource_groups() {
        let registry = ResourceGroupRegistry::new();
        let _token = registry.register_resource_group("my-group");

        let mut visited = Vec::new();
        registry.visit_resource_groups(|name, _stats| {
            visited.push(name.to_string());
        });

        assert_eq!(visited.len(), 2);
        assert_eq!(visited[0], "root");
        assert_eq!(visited[1], "my-group");
    }
}

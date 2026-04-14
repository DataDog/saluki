//! Global allocator implementation that allows tracking allocations on a per-group basis.

// TODO: The current design does not allow for deregistering groups, which is currently fine and
// likely will be for a while, but would be a limitation in a world where we dynamically launched
// data pipelines and wanted to clean up removed components, and so on.

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::RefCell,
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Mutex, OnceLock,
    },
    task::{Context, Poll},
};

use pin_project::pin_project;

use crate::cpu::CpuStats;

const STATS_LAYOUT: Layout = Layout::new::<*const ResourceStats>();

static REGISTRY: OnceLock<ResourceGroupRegistry> = OnceLock::new();
static ROOT_GROUP: ResourceStats = ResourceStats::new();

thread_local! {
    static CURRENT_GROUP: RefCell<NonNull<ResourceStats>> = RefCell::new(NonNull::from(&ROOT_GROUP));
}

/// A global allocator that tracks allocations on a per-group basis.
///
/// This allocator provides the ability to track the allocations/deallocations, both in bytes and objects, for
/// different, user-defined resource groups.
///
/// ## Resource groups
///
/// Allocation (and deallocations) are tracked by **resource group**. When this allocator is used, every allocation is
/// associated with an resource group. Resource groups are user-defined, except for the default "root" allocation
/// group which acts as a catch-all for allocations when a user-defined group is not entered.
///
/// ## Token guard
///
/// When an resource group is registered, an `ResourceGroupToken` is returned. This token can be used to "enter" the
/// group, which attribute all allocations on the current thread to that group. Entering the group returns a drop guard
/// that restores the previously entered allocation when it is dropped.
///
/// This allows for arbitrarily nested resource groups.
///
/// ## Changes to memory layout
///
/// In order to associate an allocation with the current resource group, a small trailer is added to the requested
/// allocation layout, in the form of a pointer to the statistics for the resource group. This allows updating the
/// statistics directly when an allocation is deallocated, without having to externally keep track of what group a given
/// allocation belongs to. These statistics are updated directly when the allocation is initially made, and when it is
/// deallocated.
///
/// This means that all requested allocations end up being one machine word larger: 4 bytes on 32-bit systems, and 8
/// bytes on 64-bit systems.
pub struct TrackingAllocator<A> {
    allocator: A,
}

impl<A> TrackingAllocator<A> {
    /// Creates a new `TrackingAllocator` that wraps another allocator.
    ///
    /// The wrapped allocator is used to actually allocate and deallocate memory, while this allocator is responsible
    /// purely for tracking the allocations and deallocations themselves.
    pub const fn new(allocator: A) -> Self {
        Self { allocator }
    }
}

unsafe impl<A> GlobalAlloc for TrackingAllocator<A>
where
    A: GlobalAlloc,
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Adjust the requested layout to fit our trailer and then try and allocate it.
        let (layout, trailer_start) = get_layout_with_group_trailer(layout);
        let layout_size = layout.size();
        let ptr = self.allocator.alloc(layout);
        if ptr.is_null() {
            return ptr;
        }

        // Store the pointer to the current resource group in the trailer, and also update the statistics.
        let trailer_ptr = ptr.add(trailer_start) as *mut *mut ResourceStats;
        CURRENT_GROUP.with(|current_group| {
            let group_ptr = current_group.borrow();
            group_ptr.as_ref().track_allocation(layout_size);

            trailer_ptr.write(group_ptr.as_ptr());
        });

        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Read the pointer to the owning resource group from the trailer and update the statistics.
        let (layout, trailer_start) = get_layout_with_group_trailer(layout);
        let trailer_ptr = ptr.add(trailer_start) as *mut *mut ResourceStats;
        let group = (*trailer_ptr).as_ref().unwrap();
        group.track_deallocation(layout.size());

        // Deallocate the memory.
        self.allocator.dealloc(ptr, layout);
    }
}

fn get_layout_with_group_trailer(layout: Layout) -> (Layout, usize) {
    let (new_layout, trailer_start) = layout.extend(STATS_LAYOUT).unwrap();
    (new_layout.pad_to_align(), trailer_start)
}

/// Statistics for an resource group.
pub struct ResourceStats {
    allocated_bytes: AtomicUsize,
    allocated_objects: AtomicUsize,
    deallocated_bytes: AtomicUsize,
    deallocated_objects: AtomicUsize,
    cpu: CpuStats,
}

impl ResourceStats {
    const fn new() -> Self {
        Self {
            allocated_bytes: AtomicUsize::new(0),
            allocated_objects: AtomicUsize::new(0),
            deallocated_bytes: AtomicUsize::new(0),
            deallocated_objects: AtomicUsize::new(0),
            cpu: CpuStats::new(),
        }
    }

    /// Gets a reference to the statistics of the root resource group.
    fn root() -> &'static Self {
        &ROOT_GROUP
    }

    /// Returns `true` if the given group has allocated any memory at all.
    pub fn has_allocated(&self) -> bool {
        self.allocated_bytes.load(Relaxed) > 0
    }

    #[inline]
    fn track_allocation(&self, size: usize) {
        self.allocated_bytes.fetch_add(size, Relaxed);
        self.allocated_objects.fetch_add(1, Relaxed);
    }

    #[inline]
    fn track_deallocation(&self, size: usize) {
        self.deallocated_bytes.fetch_add(size, Relaxed);
        self.deallocated_objects.fetch_add(1, Relaxed);
    }

    #[inline]
    fn track_cpu_time(&self, nanos: u64) {
        self.cpu.track_cpu_time(nanos);
    }

    /// Captures a snapshot of the current statistics based on the delta from a previous snapshot.
    ///
    /// This can be used to keep a single local snapshot of the last delta, and then both track the delta since that
    /// snapshot, as well as update the snapshot to the current statistics.
    ///
    /// Callers should generally create their snapshot via [`ResourceStatsSnapshot::empty`] and then use this method
    /// to get their snapshot delta, utilize those delta values in whatever way is necessary, and then merge the
    /// snapshot delta into the primary snapshot via [`ResourceStatsSnapshot::merge`] to make it current.
    pub fn snapshot_delta(&self, previous: &ResourceStatsSnapshot) -> ResourceStatsSnapshot {
        ResourceStatsSnapshot {
            allocated_bytes: self.allocated_bytes.load(Relaxed) - previous.allocated_bytes,
            allocated_objects: self.allocated_objects.load(Relaxed) - previous.allocated_objects,
            deallocated_bytes: self.deallocated_bytes.load(Relaxed) - previous.deallocated_bytes,
            deallocated_objects: self.deallocated_objects.load(Relaxed) - previous.deallocated_objects,
            cpu_time_nanos: self.cpu.cpu_time_nanos() - previous.cpu_time_nanos,
        }
    }
}

/// Snapshot of allocation statistics for a group.
pub struct ResourceStatsSnapshot {
    /// Number of allocated bytes since the last snapshot.
    pub allocated_bytes: usize,

    /// Number of allocated objects since the last snapshot.
    pub allocated_objects: usize,

    /// Number of deallocated bytes since the last snapshot.
    pub deallocated_bytes: usize,

    /// Number of deallocated objects since the last snapshot.
    pub deallocated_objects: usize,

    /// Cumulative CPU time in nanoseconds consumed by this group since the last snapshot.
    pub cpu_time_nanos: u64,
}

impl ResourceStatsSnapshot {
    /// Creates an empty `ResourceStatsSnapshot`.
    pub const fn empty() -> Self {
        Self {
            allocated_bytes: 0,
            allocated_objects: 0,
            deallocated_bytes: 0,
            deallocated_objects: 0,
            cpu_time_nanos: 0,
        }
    }

    /// Returns the number of live allocated bytes.
    pub fn live_bytes(&self) -> usize {
        self.allocated_bytes - self.deallocated_bytes
    }

    /// Returns the number of live allocated objects.
    pub fn live_objects(&self) -> usize {
        self.allocated_objects - self.deallocated_objects
    }

    /// Merges `other` into `self`.
    ///
    /// This can be used to accumulate the total number of (de)allocated bytes and objects when handling the deltas
    /// generated from `ResourceStats::consume`.
    pub fn merge(&mut self, other: &Self) {
        self.allocated_bytes += other.allocated_bytes;
        self.allocated_objects += other.allocated_objects;
        self.deallocated_bytes += other.deallocated_bytes;
        self.deallocated_objects += other.deallocated_objects;
        self.cpu_time_nanos += other.cpu_time_nanos;
    }
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

    /// Records CPU time consumed during a poll to this group's stats.
    #[inline]
    fn track_cpu_time(&self, nanos: u64) {
        // SAFETY: The group pointer is valid for the lifetime of the program, as allocation
        // groups are never deallocated.
        unsafe { self.group_ptr.as_ref().track_cpu_time(nanos) };
    }

    /// Enters this resource group, returning a guard that will exit the resource group when dropped.
    pub fn enter(&self) -> TrackingGuard<'_> {
        // Swap the current group to the one we're tracking.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();
            let previous_group_ptr = *group_ptr;
            *group_ptr = self.group_ptr;

            TrackingGuard {
                previous_group_ptr,
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
/// When the guard is dropped, the resource group will be exited and the previously entered
/// resource group will be restored.
///
/// This is returned by the [`ResourceGroupToken::enter`] method.
pub struct TrackingGuard<'a> {
    previous_group_ptr: NonNull<ResourceStats>,
    _token: PhantomData<&'a ResourceGroupToken>,
}

impl Drop for TrackingGuard<'_> {
    fn drop(&mut self) {
        // Reset the current group to the one that existed before we entered.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();
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

        let before = crate::cpu::thread_cpu_time_nanos();
        let result = this.inner.poll(cx);
        if let (Some(before), Some(after)) = (before, crate::cpu::thread_cpu_time_nanos()) {
            this.token.track_cpu_time(after.saturating_sub(before));
        }

        result
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
    /// use memory_accounting::allocator::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
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
    /// use memory_accounting::allocator::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
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
        ResourceStats::root().has_allocated()
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

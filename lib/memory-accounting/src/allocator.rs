//! A global allocator that tracks allocations and deallocations and attributes them to specific components.

// TODO: The current design does not allow for deregistering components, which is currently fine and likely will be for
// a while, but would be a limitation in a world where we dynamically launched data pipelines and wanted to clean up
// removed components, and so on.

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

const STATS_LAYOUT: Layout = Layout::new::<*const AllocationStats>();

static REGISTRY: OnceLock<ComponentRegistry> = OnceLock::new();
static ROOT_COMPONENT: AllocationStats = AllocationStats::new();

thread_local! {
    static CURRENT_COMPONENT: RefCell<NonNull<AllocationStats>> = RefCell::new(NonNull::from(&ROOT_COMPONENT));
}

/// A global allocator that tracks allocations and deallocations and attributes them to specific components.
pub struct TrackingAllocator<A> {
    allocator: A,
}

impl<A> TrackingAllocator<A> {
    /// Creates a new `TrackingAllocator` that wraps the given allocator.
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
        let (layout, trailer_start) = get_layout_with_component_trailer(layout);
        let layout_size = layout.size();
        let ptr = self.allocator.alloc(layout);
        if ptr.is_null() {
            return ptr;
        }

        // Store the pointer to the current component in the trailer, and also update the statistics.
        let trailer_ptr = ptr.add(trailer_start) as *mut *mut AllocationStats;
        CURRENT_COMPONENT.with(|current_component| {
            let component_ptr = current_component.borrow();
            component_ptr.as_ref().track_allocation(layout_size);

            trailer_ptr.write(component_ptr.as_ptr());
        });

        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Read the pointer to the owning component from the trailer and update the statistics.
        let (layout, trailer_start) = get_layout_with_component_trailer(layout);
        let trailer_ptr = ptr.add(trailer_start) as *mut *mut AllocationStats;
        let component = (*trailer_ptr).as_ref().unwrap();
        component.track_deallocation(layout.size());

        // Deallocate the memory.
        self.allocator.dealloc(ptr, layout);
    }
}

fn get_layout_with_component_trailer(layout: Layout) -> (Layout, usize) {
    let (new_layout, trailer_start) = layout.extend(STATS_LAYOUT).unwrap();
    (new_layout.pad_to_align(), trailer_start)
}

/// Allocation statistics for a component.
pub struct AllocationStats {
    allocated_bytes: AtomicUsize,
    allocated_objects: AtomicUsize,
    deallocated_bytes: AtomicUsize,
    deallocated_objects: AtomicUsize,
}

impl AllocationStats {
    const fn new() -> Self {
        Self {
            allocated_bytes: AtomicUsize::new(0),
            allocated_objects: AtomicUsize::new(0),
            deallocated_bytes: AtomicUsize::new(0),
            deallocated_objects: AtomicUsize::new(0),
        }
    }

    /// Gets a reference to the root component.
    pub fn root() -> &'static Self {
        &ROOT_COMPONENT
    }

    /// Returns `true` if the given component has allocated any memory at all.
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

    /// Consumes the current value of each statistics, resetting them to zero.
    ///
    /// Deltas represent the total change in each value since the _last_ delta, and the caller must keep track of these
    /// changes themselves if they wish to track the total values over the life of the process.
    pub fn consume(&self) -> AllocationStatsDelta {
        AllocationStatsDelta {
            allocated_bytes: self.allocated_bytes.swap(0, Relaxed),
            allocated_objects: self.allocated_objects.swap(0, Relaxed),
            deallocated_bytes: self.deallocated_bytes.swap(0, Relaxed),
            deallocated_objects: self.deallocated_objects.swap(0, Relaxed),
        }
    }
}

/// Delta allocation statistics for a component.
pub struct AllocationStatsDelta {
    /// Number of allocated bytes since the last delta.
    pub allocated_bytes: usize,

    /// Number of allocated objects since the last delta.
    pub allocated_objects: usize,

    /// Number of deallocated bytes since the last delta.
    pub deallocated_bytes: usize,

    /// Number of deallocated objects since the last delta.
    pub deallocated_objects: usize,
}

impl AllocationStatsDelta {
    /// Creates an empty `AllocationStatsDelta`.
    pub const fn empty() -> Self {
        Self {
            allocated_bytes: 0,
            allocated_objects: 0,
            deallocated_bytes: 0,
            deallocated_objects: 0,
        }
    }

    /// Merges `other` into `self`.
    ///
    /// This can be used to accumulate the total number of (de)allocated bytes and objects when handling the deltas
    /// generated from `AllocationStats::consume`.
    pub fn merge(&mut self, other: &Self) {
        self.allocated_bytes += other.allocated_bytes;
        self.allocated_objects += other.allocated_objects;
        self.deallocated_bytes += other.deallocated_bytes;
        self.deallocated_objects += other.deallocated_objects;
    }
}

/// A token that allows tracking allocations and deallocations for a specific component.
pub struct TrackingToken {
    component_ptr: NonNull<AllocationStats>,
}

impl TrackingToken {
    fn new(component_ptr: NonNull<AllocationStats>) -> Self {
        Self { component_ptr }
    }

    fn current() -> Self {
        CURRENT_COMPONENT.with(|current_component| {
            let component_ptr = current_component.borrow();
            Self::new(*component_ptr)
        })
    }

    fn root() -> Self {
        Self::new(NonNull::from(&ROOT_COMPONENT))
    }

    /// Enters the tracking context for this token, attributing allocations to the component it represents.
    ///
    /// This method returns a guard that will reset the currently-tracked component to its previous value when dropped.
    pub fn enter(&self) -> TrackingGuard<'_> {
        // Swap the current component to the one we're tracking.
        CURRENT_COMPONENT.with(|current_component| {
            let mut component_ptr = current_component.borrow_mut();
            let previous_component_ptr = *component_ptr;
            *component_ptr = self.component_ptr;

            TrackingGuard {
                previous_component_ptr,
                _token: PhantomData,
            }
        })
    }
}

// SAFETY: There's nothing inherently thread-specific about the token.
unsafe impl Send for TrackingToken {}

/// A guard that resets the currently-tracked component to its previous value when dropped.
pub struct TrackingGuard<'a> {
    previous_component_ptr: NonNull<AllocationStats>,
    _token: PhantomData<&'a TrackingToken>,
}

impl<'a> Drop for TrackingGuard<'a> {
    fn drop(&mut self) {
        // Reset the current component to the one that existed before we entered.
        CURRENT_COMPONENT.with(|current_component| {
            let mut component_ptr = current_component.borrow_mut();
            *component_ptr = self.previous_component_ptr;
        });
    }
}

/// An object wrapper that tracks allocations and attributes them to a specific component.
///
/// Provides methods and implementations to help ensure that operations against/using the wrapped object have all
/// allocations properly tracked and attributed to a given component.
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
    token: TrackingToken,

    #[pin]
    inner: Inner,
}

impl<Inner> Tracked<Inner> {
    /// Enters the tracking context for this token, attributing allocations to the component it represents.
    ///
    /// This method returns a guard that will reset the currently-tracked component to its previous value when dropped.
    pub fn enter(&self) -> TrackingGuard<'_> {
        self.token.enter()
    }

    /// Gets a reference to the inner object.
    pub fn inner_ref(&self) -> &Inner {
        &self.inner
    }

    /// Consumes this object and returns the inner object and tracking token.
    pub fn into_parts(self) -> (TrackingToken, Inner) {
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

/// Attaches tracking tokens to a [`Future`].
pub trait Track: Sized {
    /// Tracks allocations and deallocations for this object using the given token.
    fn track_allocations(self, token: TrackingToken) -> Tracked<Self> {
        Tracked { token, inner: self }
    }

    /// Tracks allocations and deallocations for this object using the current component.
    fn in_current_component(self) -> Tracked<Self> {
        Tracked {
            token: TrackingToken::current(),
            inner: self,
        }
    }
}

impl<T: Sized> Track for T {}

/// A registry of components that can have allocations and deallocations attributed to them.
pub struct ComponentRegistry {
    components: Mutex<HashMap<String, Box<AllocationStats>>>,
}

impl ComponentRegistry {
    fn new() -> Self {
        with_root_component(|| Self {
            components: Mutex::new(HashMap::new()),
        })
    }

    /// Gets a reference to the global component registry.
    pub fn global() -> &'static Self {
        REGISTRY.get_or_init(ComponentRegistry::new)
    }

    /// Registers a new component with the given name.
    ///
    /// Returns a `TrackingToken` that can be used to attribute allocations and deallocations to this component.
    pub fn register_component<S>(&self, name: S) -> TrackingToken
    where
        S: Into<String>,
    {
        with_root_component(|| {
            let component_stats = Box::new(AllocationStats::new());
            let token = TrackingToken::new(NonNull::from(&*component_stats));

            self.components.lock().unwrap().insert(name.into(), component_stats);

            token
        })
    }

    /// Visits all components in the registry and calls the given closure with their names and statistics.
    pub fn visit_components<F>(&self, mut f: F)
    where
        F: FnMut(&str, &AllocationStats),
    {
        with_root_component(|| {
            f("root", &ROOT_COMPONENT);

            let components = self.components.lock().unwrap();
            for (name, stats) in components.iter() {
                f(name, stats);
            }
        });
    }
}

fn with_root_component<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let token = TrackingToken::root();
    let _enter = token.enter();
    f()
}

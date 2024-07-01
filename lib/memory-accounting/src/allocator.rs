//! A global allocator that tracks allocations and deallocations and attributes them to specific components.

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
use ubyte::ToByteUnit as _;

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

    fn track_allocation(&self, size: usize) {
        self.allocated_bytes.fetch_add(size, Relaxed);
        self.allocated_objects.fetch_add(1, Relaxed);
    }

    fn track_deallocation(&self, size: usize) {
        self.deallocated_bytes.fetch_add(size, Relaxed);
        self.deallocated_objects.fetch_add(1, Relaxed);
    }

    /// Gets the total number of bytes allocated by this component.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes.load(Relaxed)
    }

    /// Gets the total number of objects allocated by this component.
    pub fn allocated_objects(&self) -> usize {
        self.allocated_objects.load(Relaxed)
    }

    /// Gets the total number of bytes deallocated by this component.
    pub fn deallocated_bytes(&self) -> usize {
        self.deallocated_bytes.load(Relaxed)
    }

    /// Gets the total number of objects deallocated by this component.
    pub fn deallocated_objects(&self) -> usize {
        self.deallocated_objects.load(Relaxed)
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

/// A [`Future`] that tracks allocations and attributes them to a specific component.
#[pin_project]
pub struct Tracked<F> {
    token: TrackingToken,

    #[pin]
    inner: F,
}

impl<F> Future for Tracked<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _enter = this.token.enter();
        this.inner.poll(cx)
    }
}

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

/// Spawns a background reporter that prints component allocation statistics every 5 seconds.
pub fn spawn_background_reporter() {
    std::thread::spawn(|| {
        let registry = ComponentRegistry::global();

        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));

            println!("Component allocation statistics:");
            registry.visit_components(|name, stats| {
                let live_bytes = stats.allocated_bytes() - stats.deallocated_bytes();
                let live_objects = stats.allocated_objects() - stats.deallocated_objects();

                println!("  {}: {} live ({} objects)", name, live_bytes.bytes(), live_objects);
            });
        }
    });
}

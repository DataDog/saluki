//! Global allocator implementation that allows tracking allocations on a per-group basis.

// TODO: The current design does not allow for deregistering groups, which is currently fine and
// likely will be for a while, but would be a limitation in a world where we dynamically launched
// data pipelines and wanted to clean up removed components, and so on.

use std::alloc::{GlobalAlloc, Layout};

use crate::{groups::CURRENT_GROUP, ResourceStats};

const STATS_LAYOUT: Layout = Layout::new::<*const ResourceStats>();

/// A global allocator that tracks allocations on a per-group basis.
///
/// This allocator provides the ability to track the allocations/deallocations, both in bytes and objects, for
/// different, user-defined resource groups.
///
/// # Resource groups
///
/// Allocation (and deallocations) are tracked by **resource group**. When this allocator is used, every allocation is
/// associated with an resource group. Resource groups are user-defined, except for the default "root" resource
/// group which acts as a catch-all when a user-defined group isn't currently entered.
///
/// # Token guard
///
/// When an resource group is registered, an `ResourceGroupToken` is returned. This token can be used to "enter" the
/// group, which attribute all allocations on the current thread to that group. Entering the group returns a drop guard
/// that restores the previously entered allocation when it's dropped.
///
/// This allows for arbitrarily nested resource groups.
///
/// ## Changes to memory layout
///
/// In order to associate an allocation with the current resource group, a small trailer is added to the requested
/// allocation layout, in the form of a pointer to the statistics for the resource group. This allows updating the
/// statistics directly when an allocation is deallocated, without having to externally keep track of what group a given
/// allocation belongs to. These statistics are updated directly when the allocation is initially made, and when it's
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

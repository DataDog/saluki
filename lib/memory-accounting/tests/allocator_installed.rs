//! Integration test for ensuring that we can detect when `TrackingAllocator` is installed.
//!
//! Note: This is an integration test as we must override the global allocator, which must be done for the entire
//! binary, so this will allow doing so solely for this test.

use std::alloc::System;

use memory_accounting::allocator::{AllocationGroupRegistry, TrackingAllocator};

#[global_allocator]
static ALLOC: TrackingAllocator<System> = TrackingAllocator::new(System);

#[test]
fn detects_allocator_installed() {
    assert!(AllocationGroupRegistry::allocator_installed());
}

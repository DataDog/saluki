//! Integration test for ensuring that we can detect when `TrackingAllocator` is installed.
//!
//! Note: This is an integration test as we must override the global allocator, which must be done for the entire
//! binary, so this will allow doing so solely for this test.

use std::alloc::System;

use saluki_common::resource_tracking::{ResourceGroupRegistry, TrackingAllocator};

#[global_allocator]
static ALLOC: TrackingAllocator<System> = TrackingAllocator::new(System);

#[test]
fn detects_allocator_installed() {
    assert!(ResourceGroupRegistry::allocator_installed());
}

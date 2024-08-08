//! Integration test for ensuring that we can detect when `TrackingAllocator` is _not_ installed.
//!
//! Note: This is an integration test because while we aren't actually overriding the global allocator here, we want to
//! be sure that some future code change doesn't manage to install it and break the test. Essentially, while the normal
//! unit tests in the crate would theoretically be a fine place to put this, we're simply being extra sure about the
//! cleanliness of the environment for this test.

use memory_accounting::allocator::AllocationGroupRegistry;

#[test]
fn detects_allocator_not_installed() {
    assert!(!AllocationGroupRegistry::allocator_installed());
}

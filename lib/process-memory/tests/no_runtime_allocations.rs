//! Allocation test for `Querier`.
//!
//! Note: this is an integration test as the global allocator must be overridden to track all allocations made, and
//! doing so in normal unit tests could interfere with other tests.

use dhat::{HeapStats, Profiler};
use process_memory::Querier;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn no_runtime_allocations() {
    // This test ensures that after initially creating `Querier`, there are _no_ runtime allocations made when calling
    // `resident_set_size`.
    //
    // This invariant should always hold, and should do so for all supported platforms.
    let mut querier = Querier::default();

    let _profiler = Profiler::builder().testing().build();
    let _rss = querier.resident_set_size().unwrap();
    let _rss = querier.resident_set_size().unwrap();
    let _rss = querier.resident_set_size().unwrap();
    let stats = HeapStats::get();

    dhat::assert_eq!(stats.total_blocks, 0);
    dhat::assert_eq!(stats.total_bytes, 0);
    dhat::assert_eq!(stats.max_blocks, 0);
    dhat::assert_eq!(stats.max_bytes, 0);
    dhat::assert_eq!(stats.curr_blocks, 0);
    dhat::assert_eq!(stats.curr_bytes, 0);
}

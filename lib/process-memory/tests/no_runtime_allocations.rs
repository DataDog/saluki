//! Allocation test for `Querier`.
//!
//! This is an integration test because the global allocator must be overridden to track all
//! allocations made, and doing so in normal unit tests could interfere with other tests.
//!
//! The binary is built with `harness = false` (see `Cargo.toml`) so that the libtest harness's
//! own runtime machinery (mpmc channel `Context`, `SyncWaker` mutex initialization, and other
//! lazy one-shot allocations performed by `test::run_tests`) cannot race with the dhat profiler
//! and be attributed to the code under test. See issue #625 for the original flake investigation.

#[cfg(not(target_os = "aix"))]
use dhat::{HeapStats, Profiler};
#[cfg(not(target_os = "aix"))]
use process_memory::Querier;

#[cfg(not(target_os = "aix"))]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(not(target_os = "aix"))]
    {
        // This test ensures that after initially creating `Querier`, calls to `resident_set_size` make no runtime
        // allocations.
        //
        // This invariant should hold on platforms that run this allocation profiler test.
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
}

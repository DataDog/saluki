#![cfg(not(target_os = "aix"))]

//! Allocation test for `Querier`.
//!
//! This is an integration test because the global allocator must be overridden to track all
//! allocations made, and doing so in normal unit tests could interfere with other tests.
//!
//! The binary is built with `harness = false` (see `Cargo.toml`) so that the libtest harness's
//! own runtime machinery (mpmc channel `Context`, `SyncWaker` mutex initialization, and other
//! lazy one-shot allocations performed by `test::run_tests`) cannot race with the dhat profiler
//! and be attributed to the code under test. See issue #625 for the original flake investigation.

use dhat::{HeapStats, Profiler};
use process_memory::Querier;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    // This test ensures that after initially creating `Querier`, there are _no_ runtime allocations made when calling
    // `resident_set_size`.
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

//! Allocation test for sketch insertion.
//!
//! Note: this is in an integration test so that it will run in its own process
//! and avoid interference from other tests. See notes at:
//! https://docs.rs/dhat/latest/dhat/#heap-usage-testing.

use crate::common::{insert_single_and_serialize, make_points, MathableHeapStats};

mod common;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_ten_single_points_ddsketch() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let points = make_points(10);

    let before: MathableHeapStats = dhat::HeapStats::get().into();
    insert_single_and_serialize(&points);
    let after: MathableHeapStats = dhat::HeapStats::get().into();

    let diff = after - before;
    dhat::assert_eq!(diff.total_blocks, 9);
    dhat::assert_eq!(diff.total_bytes, 336);
    dhat::assert_eq!(diff.max_blocks, 3);
    dhat::assert_eq!(diff.max_bytes, 192);
    dhat::assert_eq!(diff.curr_blocks, 0);
    dhat::assert_eq!(diff.curr_bytes, 0);
}

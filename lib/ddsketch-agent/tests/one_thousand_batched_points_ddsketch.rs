//! Allocation test for sketch insertion.
//!
//! Note: this is in an integration test so that it will run in its own process
//! and avoid interference from other tests. See notes at:
//! https://docs.rs/dhat/latest/dhat/#heap-usage-testing.

use crate::common::{insert_many_and_serialize, make_points};

mod common;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_one_thousand_batched_points_ddsketch() {
    let points = make_points(1000);

    let _profiler = dhat::Profiler::builder().testing().build();
    insert_many_and_serialize(&points);
    let stats = dhat::HeapStats::get();

    dhat::assert_eq!(stats.total_blocks, 14);
    dhat::assert_eq!(stats.total_bytes, 6149);
    dhat::assert_eq!(stats.max_blocks, 3);
    dhat::assert_eq!(stats.max_bytes, 4768);
    dhat::assert_eq!(stats.curr_blocks, 0);
    dhat::assert_eq!(stats.curr_bytes, 0);
}

//! Allocation test for sketch insertion.
//!
//! Note: this is in an integration test so that it will run in its own process
//! and avoid interference from other tests. See notes at:
//! https://docs.rs/dhat/latest/dhat/#heap-usage-testing.

use crate::common::{insert_single_and_serialize, insert_single_points, make_points};

mod common;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_one_thousand_single_points_ddsketch() {
    let points = make_points(1000);

    let _profiler = dhat::Profiler::builder().testing().build();
    // insert_single_and_serialize(&points);
    insert_single_points(&points);
    let stats = dhat::HeapStats::get();

    // Comments note serialization overhead (see `fn merge_to_dogsketch`)
    dhat::assert_eq!(stats.total_blocks, 13); // + 14
    dhat::assert_eq!(stats.total_bytes, 1600); // + 4064
    dhat::assert_eq!(stats.max_blocks, 4); // + 2
    dhat::assert_eq!(stats.max_bytes, 864); // + 2048
    dhat::assert_eq!(stats.curr_blocks, 0);
    dhat::assert_eq!(stats.curr_bytes, 0);

    // separate kv list branch shows (this includes serialization):
    // dhat::assert_eq!(stats.total_blocks, 14);
    // dhat::assert_eq!(stats.total_bytes, 3736);
    // dhat::assert_eq!(stats.max_blocks, 4);
    // dhat::assert_eq!(stats.max_bytes, 2744);

    // and main, without serialization:
    // dhat::assert_eq!(stats.total_blocks, 6);
    // dhat::assert_eq!(stats.total_bytes, 2016);
    // dhat::assert_eq!(stats.max_blocks, 1);
    // dhat::assert_eq!(stats.max_bytes, 1024);
}

use ddsketch_agent::DDSketch;

use crate::common::{insert_many_and_serialize, make_points, MathableHeapStats};

mod common;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_ten_batched_points_ddsketch() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let points = make_points(10);

    let before: MathableHeapStats = dhat::HeapStats::get().into();
    insert_many_and_serialize::<DDSketch>(&points);
    let after: MathableHeapStats = dhat::HeapStats::get().into();

    let diff = after - before;
    dhat::assert_eq!(diff.total_blocks, 10);
    dhat::assert_eq!(diff.total_bytes, 356);
    dhat::assert_eq!(diff.max_blocks, 3);
    dhat::assert_eq!(diff.max_bytes, 192);
    dhat::assert_eq!(diff.curr_blocks, 0);
    dhat::assert_eq!(diff.curr_bytes, 0);
}

use ddsketch_agent::BufferedDDSketch;

use crate::common::{insert_single_and_serialize, make_points, MathableHeapStats};

mod common;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn test_ten_single_points_buffered_ddsketch() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let points = make_points(10);

    let before: MathableHeapStats = dhat::HeapStats::get().into();
    insert_single_and_serialize::<BufferedDDSketch>(&points);
    let after: MathableHeapStats = dhat::HeapStats::get().into();

    let diff = after - before;
    dhat::assert_eq!(diff.total_blocks, 13);
    dhat::assert_eq!(diff.total_bytes, 412);
    dhat::assert_eq!(diff.max_blocks, 4);
    dhat::assert_eq!(diff.max_bytes, 224);
    dhat::assert_eq!(diff.curr_blocks, 0);
    dhat::assert_eq!(diff.curr_bytes, 0);
}

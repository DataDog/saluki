#![cfg(target_os = "aix")]

use process_memory::Querier;

#[test]
fn resident_set_size_returns_nonzero_value() {
    let mut querier = Querier::default();
    let rss = querier
        .resident_set_size()
        .expect("resident set size should be available on AIX");

    assert!(rss > 0, "resident set size should be greater than zero");
}

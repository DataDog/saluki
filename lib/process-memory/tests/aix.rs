#![cfg(target_os = "aix")]

use std::{fs, path::PathBuf};

use process_memory::Querier;

const KIB: u64 = 1024;
const PR_RSSIZE_OFFSET: usize = 104;
const PR_RSSIZE_SIZE: usize = std::mem::size_of::<u64>();

#[test]
fn resident_set_size_returns_nonzero_value() {
    let mut querier = Querier::default();
    let rss = querier
        .resident_set_size()
        .expect("resident set size should be available on AIX");

    assert!(rss > 0, "resident set size should be greater than zero");
}

#[test]
fn resident_set_size_is_close_to_proc_psinfo_rss_field() {
    let expected_before = current_process_psinfo_rss_bytes().expect("resident set size should parse from AIX psinfo");

    let mut querier = Querier::default();
    let actual = querier
        .resident_set_size()
        .expect("resident set size should be available on AIX");

    let expected_after = current_process_psinfo_rss_bytes().expect("resident set size should parse from AIX psinfo");
    let lower_bound = expected_before.min(expected_after).saturating_sub(4 * 1024 * 1024);
    let upper_bound = expected_before.max(expected_after).saturating_add(4 * 1024 * 1024);

    assert!(
        (lower_bound..=upper_bound).contains(&actual),
        "queried RSS {actual} should be close to raw psinfo RSS range {expected_before}..={expected_after}"
    );
}

fn current_process_psinfo_rss_bytes() -> Option<usize> {
    let psinfo_path = PathBuf::from(format!("/proc/{}/psinfo", std::process::id()));
    let psinfo = fs::read(psinfo_path).ok()?;
    let raw_rss_kib = psinfo.get(PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE)?;
    let rss_kib = u64::from_ne_bytes(raw_rss_kib.try_into().ok()?);
    let rss_bytes = rss_kib.checked_mul(KIB)?;

    usize::try_from(rss_bytes).ok()
}

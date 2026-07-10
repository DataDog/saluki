#[cfg_attr(not(miri), test)]
pub fn macros() {
    let t = trybuild::TestCases::new();
    t.pass("tests/macros/01_basic_usage.rs");
    t.pass("tests/macros/02_kinds_and_levels.rs");
    t.pass("tests/macros/03_no_labels.rs");
    t.compile_fail("tests/macros/04_no_metrics.rs");
    t.compile_fail("tests/macros/05_invalid_level_prefix.rs");
}

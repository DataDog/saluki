#[cfg_attr(not(miri), test)]
pub fn macros() {
    let t = trybuild::TestCases::new();
    t.pass("tests/macros/01_basic_usage.rs");
    t.pass("tests/macros/02_no_labels.rs");
    t.pass("tests/macros/03_levels.rs");
    t.pass("tests/macros/04_multi_label.rs");
    t.pass("tests/macros/10_mapped_bare.rs");
    t.pass("tests/macros/11_mapped_typed.rs");
    t.pass("tests/macros/12_mapped_mixed.rs");
    t.pass("tests/macros/13_mapped_concurrent.rs");
    t.compile_fail("tests/macros/05_empty_struct.rs");
    t.compile_fail("tests/macros/06_invalid_level.rs");
    t.compile_fail("tests/macros/07_not_a_struct.rs");
    t.compile_fail("tests/macros/08_bad_field_type.rs");
    t.compile_fail("tests/macros/09_missing_prefix.rs");
}

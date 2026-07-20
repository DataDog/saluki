//! Tests for the per-kind ground metric and the named-rule layer.

use ddsketch::canonical::IndexMapping;
use ddsketch::DDSketch;
use proptest::prelude::*;

use super::{
    bins_total, d_v, quantile_from_bins, value_for_key, Band, GroundError, GroundRule, MissingCounterZero, RuleContext,
    RuleSet, SketchAdjacentBinQuantization, UnknownKindPresenceOnly,
};
use crate::contexts::{BucketValue, SketchValue};

fn scalar(value: f64) -> BucketValue {
    BucketValue::Scalar(value)
}

fn sketch(bins: Vec<(i32, u32)>) -> BucketValue {
    let count = bins.iter().map(|(_, n)| i64::from(*n)).sum();
    BucketValue::Sketch(SketchValue {
        count,
        sum: 0.0,
        min: 0.0,
        max: 0.0,
        bins,
    })
}

fn ctx<'a>(reference: &'a [f64], sut: &'a [f64], ref_index: usize, sut_index: usize, window: usize) -> RuleContext<'a> {
    RuleContext {
        reference,
        sut,
        ref_index,
        sut_index,
        band: Band::ddsketch(),
        window,
    }
}

#[test]
fn band_derives_alpha_and_floor_from_the_ddsketch_api() {
    // The band is the ddsketch mapping's relative accuracy and its value_for_key(1) floor, read from
    // the public API rather than any baked-in constant.
    let band = Band::ddsketch();
    assert_eq!(band.alpha, DDSketch::remap_mapping().relative_accuracy());
    assert_eq!(band.floor, DDSketch::value_for_key(1));
    assert!(band.alpha > 0.0 && band.floor > 0.0);
}

#[test]
fn ground_source_carries_no_hardcoded_ddsketch_literal() {
    // The derivation must not smuggle the mapping's gamma, alpha, or eps in as a numeric literal. Each
    // needle is split so this test file itself does not contain the forbidden literal verbatim.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ground.rs")).expect("read ground.rs source");
    let needles = [["1.015", "625"], ["0.015", "625"], ["0.0077", "519"]];
    for parts in needles {
        let literal = parts.concat();
        assert!(
            !source.contains(&literal),
            "ground.rs hardcodes the ddsketch literal {literal}"
        );
    }
}

#[test]
fn identical_scalars_give_a_zero_leash() -> Result<(), GroundError> {
    let leash = d_v(&scalar(42.0), &scalar(42.0), Band::ddsketch())?;
    assert_eq!(leash.gap, 0.0);
    assert!(!leash.over_band());
    Ok(())
}

#[test]
fn identical_sketches_give_a_zero_leash() -> Result<(), GroundError> {
    let value = sketch(vec![(5, 3), (7, 2)]);
    let leash = d_v(&value, &value, Band::ddsketch())?;
    assert_eq!(leash.gap, 0.0);
    assert!(!leash.over_band());
    Ok(())
}

#[test]
fn different_kinds_are_a_kind_mismatch() {
    let error = d_v(&scalar(1.0), &sketch(vec![(1, 1)]), Band::ddsketch()).unwrap_err();
    assert_eq!(error, GroundError::KindMismatch);
}

#[test]
fn scalar_band_uses_the_larger_magnitude() -> Result<(), GroundError> {
    // Per-kind band: a scalar leash bands on max(|reference|,|sut|) so the leash is symmetric.
    let band = Band::ddsketch();
    let leash = d_v(&scalar(3.0), &scalar(-5.0), band)?;
    assert_eq!(leash.gap, -8.0);
    assert_eq!(leash.band, band.at(5.0));
    Ok(())
}

#[test]
fn sketch_sup_catches_a_p99_blowup() -> Result<(), GroundError> {
    // The reference concentrates at a low key; the SUT pushes a tenth of its mass into a far higher
    // key so its upper quantiles blow up. The sup over displayed quantiles trips even though p50/p90
    // agree.
    let band = Band::ddsketch();
    let reference = sketch(vec![(10, 100)]);
    let sut = sketch(vec![(10, 90), (500, 10)]);
    let leash = d_v(&reference, &sut, band)?;
    assert!(leash.over_band());
    Ok(())
}

#[test]
fn sketch_leash_bands_on_the_winning_quantile_values() -> Result<(), GroundError> {
    // Per-kind band: the sketch leash reports the band at the quantile that won the sup, not a summary
    // band. p95 is the first quantile to cross into the high bin, so its values set the band.
    let band = Band::ddsketch();
    let reference_bins = vec![(10, 100)];
    let sut_bins = vec![(10, 90), (500, 10)];
    let leash = d_v(&sketch(reference_bins.clone()), &sketch(sut_bins.clone()), band)?;

    let reference_q = quantile_from_bins(&reference_bins, 0.95).expect("reference p95");
    let sut_q = quantile_from_bins(&sut_bins, 0.95).expect("sut p95");
    assert_eq!(leash.gap, sut_q - reference_q);
    assert_eq!(leash.band, band.at(reference_q.abs().max(sut_q.abs())));
    Ok(())
}

#[test]
fn count_mismatch_snaps_to_an_infinite_leash() -> Result<(), GroundError> {
    // A strict total-count gate, computed from the bins: one extra sample snaps the leash to infinity
    // regardless of quantile agreement.
    let band = Band::ddsketch();
    let reference = sketch(vec![(10, 100)]);
    let sut = sketch(vec![(10, 101)]);
    let leash = d_v(&reference, &sut, band)?;
    assert!(leash.gap.is_infinite());
    assert!(leash.over_band());
    Ok(())
}

#[test]
fn bins_total_sums_the_bin_counts() {
    assert_eq!(bins_total(&[(1, 2), (3, 5)]), 7);
    assert_eq!(bins_total(&[]), 0);
}

#[test]
fn default_rules_registers_the_three_named_rules_in_order() {
    assert_eq!(
        RuleSet::default_rules().names(),
        vec![
            "missing_counter_zero",
            "unknown_kind_presence_only",
            "sketch_adjacent_bin_quantization"
        ]
    );
}

#[test]
fn empty_ruleset_suppresses_nothing() {
    let reference = [0.0, 1000.0];
    let sut = [1000.0, 0.0];
    assert!(!RuleSet::new().suppressed(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn missing_counter_zero_suppresses_a_phase_shifted_increment() {
    // The reference is zero at the coupled column but carries the SUT's value one bucket over: the
    // counter increment slid into an adjacent flush, a benign phase shift.
    let reference = [0.0, 1000.0];
    let sut = [1000.0, 0.0];
    assert!(MissingCounterZero.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn missing_counter_zero_keeps_a_real_drop() {
    // The reference is zero and never carries the SUT's value in the window: real invented data.
    let reference = [0.0, 0.0];
    let sut = [1000.0, 0.0];
    assert!(!MissingCounterZero.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn unknown_kind_presence_only_suppresses_a_non_finite_column() {
    let reference = [f64::NAN];
    let sut = [5.0];
    assert!(UnknownKindPresenceOnly.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn unknown_kind_presence_only_keeps_a_finite_column() {
    let reference = [1.0];
    let sut = [2.0];
    assert!(!UnknownKindPresenceOnly.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn sketch_adjacent_bin_quantization_suppresses_one_bin_apart() {
    // One multiplicative gamma step is exactly one bin up on the log grid: a quantization boundary,
    // not a real divergence.
    let gamma = DDSketch::remap_mapping().gamma();
    let reference = [100.0];
    let sut = [100.0 * gamma];
    assert!(SketchAdjacentBinQuantization.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn sketch_adjacent_bin_quantization_keeps_a_multi_bin_gap() {
    let reference = [100.0];
    let sut = [1_000_000.0];
    assert!(!SketchAdjacentBinQuantization.suppresses(&ctx(&reference, &sut, 0, 0, 2)));
}

#[test]
fn value_for_key_saturates_out_of_range_keys() {
    // A key past the i16 range must saturate rather than panic, and must never read back as NaN: a NaN
    // magnitude would make every gap comparison false and silently mask a trip. An extreme key may
    // legitimately overflow to a saturated infinite magnitude, which only ever trips more.
    assert!(!value_for_key(i32::MAX).is_nan());
    assert!(!value_for_key(i32::MIN).is_nan());
}

proptest! {
    /// Swapping the two lanes negates the gap and leaves the band unchanged: the scalar leash is
    /// symmetric.
    #[test]
    fn property_test_d_v_scalar_symmetric(a in -1.0e6f64..1.0e6, s in -1.0e6f64..1.0e6) {
        let band = Band::ddsketch();
        let forward = d_v(&scalar(a), &scalar(s), band).expect("forward scalar leash");
        let reverse = d_v(&scalar(s), &scalar(a), band).expect("reverse scalar leash");
        prop_assert_eq!(forward.gap, -reverse.gap);
        prop_assert_eq!(forward.band, reverse.band);
    }

    /// An empty rule set suppresses nothing, and registering more rules only ever widens `same`: any
    /// single rule that suppresses a column is subsumed by the full default set.
    #[test]
    fn property_test_rules_only_widen(
        reference in prop::collection::vec(-1.0e3f64..1.0e3, 1..8),
        sut in prop::collection::vec(-1.0e3f64..1.0e3, 1..8),
        ri in 0usize..8,
        si in 0usize..8,
    ) {
        let ref_index = ri % reference.len();
        let sut_index = si % sut.len();
        let context = ctx(&reference, &sut, ref_index, sut_index, 2);

        prop_assert!(!RuleSet::new().suppressed(&context));

        let full = RuleSet::default_rules();
        for rule in [
            Box::new(MissingCounterZero) as Box<dyn GroundRule>,
            Box::new(UnknownKindPresenceOnly),
            Box::new(SketchAdjacentBinQuantization),
        ] {
            let mut single = RuleSet::new();
            let name = rule.name();
            single.register(rule);
            if single.suppressed(&context) {
                prop_assert!(full.suppressed(&context), "rule {} suppressed but the default set did not", name);
            }
        }
    }
}

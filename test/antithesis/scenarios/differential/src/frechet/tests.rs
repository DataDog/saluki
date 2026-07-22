//! Tests for the banded discrete-Frechet aligner: the sup fold, the J1 envelope guard, the
//! present/absent bottom trip, and the length-independence of the anytime verdict.

use harness::SAKOE_CHIBA_BAND;
use proptest::prelude::*;

use super::{AlignerError, Frontier};
use crate::contexts::{Bucket, BucketValue};
use crate::ground::{Band, RuleSet};

/// A scalar curve: one bucket per value, ten wire-seconds apart in bucket-start order.
fn scalar_curve(values: &[f64]) -> Vec<Bucket> {
    values
        .iter()
        .enumerate()
        .map(|(index, &value)| Bucket {
            bucket_start: index as u64 * 10,
            value: BucketValue::Scalar(value),
            conflict: false,
        })
        .collect()
}

/// The launch aligner: the provisional Sakoe-Chiba band, the ddsketch-derived value band, no operator
/// rules so the sup fold and the baked-in J1 guard alone decide.
fn aligner() -> Frontier {
    Frontier::new(SAKOE_CHIBA_BAND)
}

#[test]
fn an_empty_curve_is_an_error() {
    let rules = RuleSet::new();
    let present = scalar_curve(&[1.0]);
    assert!(matches!(
        aligner().align(&[], &present, Band::ddsketch(), &rules),
        Err(AlignerError::EmptyCurve)
    ));
    assert!(matches!(
        aligner().align(&present, &[], Band::ddsketch(), &rules),
        Err(AlignerError::EmptyCurve)
    ));
}

#[test]
fn identical_curves_do_not_trip() {
    let rules = RuleSet::new();
    let curve = scalar_curve(&[1.0, 2.0, 3.0, 4.0]);
    let verdict = aligner()
        .align(&curve, &curve, Band::ddsketch(), &rules)
        .expect("identical curves align");
    assert!(!verdict.tripped());
}

#[test]
fn a_sup_trip_names_the_sup_fold() {
    let rules = RuleSet::new();
    let reference = scalar_curve(&[10.0, 10.0, 10.0]);
    let sut = scalar_curve(&[10.0, 9000.0, 10.0]);
    let verdict = aligner()
        .align(&reference, &sut, Band::ddsketch(), &rules)
        .expect("aligns");
    assert!(verdict.tripped());
    assert_eq!(verdict.trips[0].rule, "sup_fold");
}

proptest! {
    /// The worst-gap verdict is LENGTH INDEPENDENT: the same localized spike embedded in a three-bucket
    /// curve and in an up-to-three-thousand-bucket curve produces the identical verdict. The sup fold is
    /// an anytime quantity, never diluted by panel length the way a length-normalized accumulator would
    /// be.
    #[test]
    fn property_test_worst_gap_length_independence(n in 3usize..=3000, spike in 1_000.0f64..1.0e6) {
        let rules = RuleSet::new();
        let band = Band::ddsketch();

        let small_ref = scalar_curve(&[100.0, 100.0, 100.0]);
        let small_sut = scalar_curve(&[100.0, spike, 100.0]);
        let small = aligner().align(&small_ref, &small_sut, band, &rules).expect("small aligns");

        let big_ref = scalar_curve(&vec![100.0; n]);
        let mut big_sut_values = vec![100.0; n];
        big_sut_values[n / 2] = spike;
        let big_sut = scalar_curve(&big_sut_values);
        let big = aligner().align(&big_ref, &big_sut, band, &rules).expect("big aligns");

        prop_assert!(small.tripped());
        prop_assert_eq!(small, big);
    }

    /// A time-slide of at most the band is ZERO COST: the same step-shaped curve shifted by up to `B`
    /// buckets aligns green. The band absorbs benign flush-phase skew and nothing else.
    #[test]
    fn property_test_small_slide_within_band_is_zero_cost(
        slide in 0usize..=SAKOE_CHIBA_BAND,
        low in -50.0f64..50.0,
        delta in 100.0f64..1_000.0,
    ) {
        let rules = RuleSet::new();
        let high = low + delta;
        let length = 8usize;

        let mut reference = vec![low; length];
        for value in reference.iter_mut().skip(4) {
            *value = high;
        }
        let mut sut = vec![low; length];
        for value in sut.iter_mut().skip(4 + slide) {
            *value = high;
        }

        let verdict = aligner()
            .align(&scalar_curve(&reference), &scalar_curve(&sut), Band::ddsketch(), &rules)
            .expect("slide aligns");
        prop_assert!(!verdict.tripped(), "a <=B slide must be absorbed, slide={}", slide);
    }

    /// A double burst the SUT collapsed to nothing reds AT THE PEAK GAP: the sup reports the burst height
    /// itself, not a length-averaged dilution of it.
    #[test]
    fn property_test_double_burst_collapse_reds_at_the_peak_gap(height in 50.0f64..1.0e5) {
        let rules = RuleSet::new();
        let reference = scalar_curve(&[0.0, height, 0.0, height, 0.0]);
        let sut = scalar_curve(&[0.0, 0.0, 0.0, 0.0, 0.0]);

        let verdict = aligner()
            .align(&reference, &sut, Band::ddsketch(), &rules)
            .expect("burst aligns");
        prop_assert!(verdict.tripped());
        let worst = verdict.worst_leash.expect("a tripped curve reports its worst leash");
        prop_assert_eq!(worst.gap.abs(), height);
    }

    /// A context present on one lane and absent on the other PAST THE BAND is an infinite-leash trip; a
    /// shorter absence the band can absorb is not.
    #[test]
    fn property_test_present_absent_bottom_trips(
        beyond in (SAKOE_CHIBA_BAND + 1)..20,
        within in 0usize..=SAKOE_CHIBA_BAND,
    ) {
        let rules = RuleSet::new();
        let band = Band::ddsketch();
        let reference = scalar_curve(&[10.0, 10.0, 10.0]);

        let long = scalar_curve(&vec![10.0; 3 + beyond]);
        let tripped = aligner().align(&reference, &long, band, &rules).expect("long aligns");
        prop_assert!(tripped.tripped());
        prop_assert_eq!(tripped.trips[0].rule.as_str(), "present_absent_bottom");
        prop_assert!(tripped.worst_leash.expect("infinite leash").gap.is_infinite());

        let absorbed = scalar_curve(&vec![10.0; 3 + within]);
        let green = aligner().align(&reference, &absorbed, band, &rules).expect("absorbed aligns");
        prop_assert!(!green.tripped(), "a <=B absence is a transient the band absorbs");
    }

    /// The J1 envelope guard is GREEN-WARD ONLY: it never green-washes an interior spike. One lane
    /// spikes for a single bucket while the other stays flat; the guard declines and the spike trips.
    #[test]
    fn property_test_envelope_guard_rejects_an_interior_spike(pos in 1usize..9, height in 100.0f64..1.0e6) {
        let rules = RuleSet::new();
        let length = 10usize;
        let reference = scalar_curve(&vec![10.0; length]);
        let mut sut_values = vec![10.0; length];
        sut_values[pos] = 10.0 + height;
        let sut = scalar_curve(&sut_values);

        let verdict = aligner()
            .align(&reference, &sut, Band::ddsketch(), &rules)
            .expect("spike aligns");
        prop_assert!(verdict.tripped(), "an interior spike must survive the guard");
    }
}

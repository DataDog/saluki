//! Tests for the online decision layer: the clipped signed CUSUM's debounce, the sub-band-bias repair
//! (C1), the triage extent that never feeds the verdict, the per-lane absolute-law detectors, and the
//! bounded sorted triage sample.

use super::{bounded_triage, defect_count, evaluate, ContextVerdict, DecisionParams, Extent};
use crate::contexts::{Bucket, BucketValue, SketchValue};
use crate::frechet::Trip;
use crate::ground::RuleSet;

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

/// A single-bucket sketch curve carrying the given summary and one log-grid bin.
fn sketch_curve(count: i64, sum: f64, min: f64, max: f64, bins: Vec<(i32, u32)>) -> Vec<Bucket> {
    vec![Bucket {
        bucket_start: 0,
        value: BucketValue::Sketch(SketchValue {
            count,
            sum,
            min,
            max,
            bins,
        }),
        conflict: false,
    }]
}

/// Whether any trip carries the named mechanism.
fn has_trip(verdict: &ContextVerdict, rule: &str) -> bool {
    verdict.trips.iter().any(|trip| trip.rule == rule)
}

#[test]
fn lone_straddle_is_suppressed() {
    // A single column deviates by just under its band; the rest are identical. The sup does not fire
    // (sub-band) and the CUSUM's single excursion decays below `h`: the debounce the CUSUM subsumes.
    let params = DecisionParams::launch();
    let base = 1000.0;
    let band = params.band.at(base);
    let mut sut_values = vec![base; 12];
    sut_values[5] = base + 0.99 * band;
    let verdict = evaluate(
        &scalar_curve(&[base; 12]),
        &scalar_curve(&sut_values),
        &RuleSet::default_rules(),
        &params,
    );
    assert!(
        !verdict.tripped,
        "a lone sub-band straddle must be debounced away: {:?}",
        verdict.trips
    );
}

#[test]
fn sustained_divergence_fires() {
    // The SUT sits far over band on every column. The sup fold trips at the first over-band column.
    let params = DecisionParams::launch();
    let verdict = evaluate(
        &scalar_curve(&[100.0; 8]),
        &scalar_curve(&[200.0; 8]),
        &RuleSet::default_rules(),
        &params,
    );
    assert!(verdict.tripped);
    assert!(
        has_trip(&verdict, "sup_fold"),
        "sustained over-band divergence trips the sup: {:?}",
        verdict.trips
    );
}

#[test]
fn sustained_sub_band_offset_fires() {
    // The C1 repair: every column carries the same sub-band bias, so the sup never fires, but the
    // signed CUSUM accumulates the consistent drift past `h`. A length-normalized accumulator would
    // dilute this to nothing; the anytime CUSUM does not.
    let params = DecisionParams::launch();
    let base = 1000.0;
    let bias = 0.5 * params.band.at(base);
    let verdict = evaluate(
        &scalar_curve(&[base; 16]),
        &scalar_curve(&[base + bias; 16]),
        &RuleSet::default_rules(),
        &params,
    );
    assert!(verdict.tripped, "a sustained sub-band offset must fire the CUSUM");
    assert!(
        has_trip(&verdict, "value_cusum"),
        "the sub-band offset fires the value CUSUM: {:?}",
        verdict.trips
    );
    assert!(
        !has_trip(&verdict, "sup_fold"),
        "no single column is over band, so the sup stays green"
    );
}

#[test]
fn extent_never_flips_the_verdict() {
    // A counter increment that slid one flush over: the reference is zero at the coupled column and
    // carries the SUT's value one bucket on. The missing-counter-zero rule suppresses the sup and the
    // CUSUM, so the context is green, yet the triage extent still records the large raw gap. Extent is
    // metadata; it must never turn the verdict red.
    let params = DecisionParams::launch();
    let reference = scalar_curve(&[0.0, 1000.0, 0.0, 0.0]);
    let sut = scalar_curve(&[1000.0, 0.0, 0.0, 0.0]);
    let verdict = evaluate(&reference, &sut, &RuleSet::default_rules(), &params);
    assert!(
        !verdict.tripped,
        "the phase-shifted increment is benign: {:?}",
        verdict.trips
    );
    assert!(
        verdict.extent.worst_gap >= 1000.0,
        "extent still records the raw gap: {:?}",
        verdict.extent
    );
}

#[test]
fn absolute_law_violation_reds() {
    // Both lanes carry identical bins and matching means, so the aligner and the CUSUMs stay green.
    // The SUT's count field disagrees with its own bins, an internal-consistency break the per-lane
    // absolute-law detector reds regardless of cross-lane agreement.
    let params = DecisionParams::launch();
    let reference = sketch_curve(5, 50.0, 10.0, 10.0, vec![(10, 5)]);
    let sut = sketch_curve(99, 990.0, 10.0, 10.0, vec![(10, 5)]);
    let verdict = evaluate(&reference, &sut, &RuleSet::default_rules(), &params);
    assert!(verdict.tripped);
    assert!(
        verdict.trips.iter().any(|t| t.rule.starts_with("absolute_law")),
        "the count/bins mismatch reds via an absolute law: {:?}",
        verdict.trips
    );
}

#[test]
fn defect_count_ignores_extent() {
    // A green context with a huge triage extent and a red context with none: the count follows only
    // `tripped`, never the extent magnitude.
    let green = ContextVerdict {
        tripped: false,
        trips: Vec::new(),
        extent: Extent {
            run_length: 9,
            worst_gap: 1.0e9,
            accumulated_gap: 1.0e12,
        },
    };
    let red = ContextVerdict {
        tripped: true,
        trips: vec![Trip {
            rule: "value_cusum".to_string(),
        }],
        extent: Extent::default(),
    };
    assert_eq!(defect_count(&[green, red]), 1);
}

#[test]
fn triage_sample_is_bounded_and_sorted() {
    // The triage sample orders by continuous worst_gap descending and bounds to the limit.
    let entries: Vec<f64> = vec![3.0, 100.0, 7.0, 42.0, 1.0];
    let sample = bounded_triage(entries, |gap| *gap, 3);
    assert_eq!(sample.len(), 3, "bounded to the limit");
    assert_eq!(sample, vec![100.0, 42.0, 7.0], "sorted by worst_gap descending");
}

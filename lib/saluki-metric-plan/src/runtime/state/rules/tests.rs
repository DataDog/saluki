use std::sync::Arc;

use saluki_core::execution::GenerationToken;

use super::*;

const A: AdmissionPointId = AdmissionPointId::new(0);
const B: AdmissionPointId = AdmissionPointId::new(1);
const OTHER: AdmissionPointId = AdmissionPointId::new(7);

fn secs(secs: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(secs)
}

fn millis(millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos(millis * 1_000_000)
}

fn generation(id: u64) -> GenerationRef {
    Arc::new(GenerationToken::new(GenerationId::new(id)))
}

/// A header stamped by `point` at `t_a` seconds in `epoch`, of generation `id`.
fn stamped(point: AdmissionPointId, t_a: u64, epoch: u32, id: u64) -> BatchHeader {
    BatchHeader {
        admitted_at: Some(secs(t_a)),
        admission_point: Some(point),
        epoch,
        generation: Some(generation(id)),
        entry: None,
    }
}

fn cut(at: u64, epochs: &[(AdmissionPointId, u32)]) -> CutBoundary {
    CutBoundary {
        at: secs(at),
        replaces: GenerationId::new(1),
        epochs: epochs.to_vec().into(),
    }
}

#[test]
fn a_tracker_follows_each_point_and_reports_resets_and_faults() {
    // Each case is a sequence of headers over the points A and B, with what each observation returns and the
    // watermark and latest time after it.
    struct Step {
        header: BatchHeader,
        observation: Observation,
        watermark: Option<u64>,
        max_latest: Option<u64>,
    }
    let step = |header, observation, watermark, max_latest| Step {
        header,
        observation,
        watermark,
        max_latest,
    };
    let reset = |point, previous, final_latest, pre: Option<u64>| Observation::EpochSwitched {
        point,
        previous,
        final_latest: secs(final_latest),
        pre: pre.map(secs),
    };
    let cases: Vec<(&str, &[AdmissionPointId], Vec<Step>)> = vec![
        (
            "one point: the watermark follows its latest time",
            &[A],
            vec![
                step(stamped(A, 10, 0, 1), Observation::Ok, Some(10), Some(10)),
                step(stamped(A, 12, 0, 1), Observation::Ok, Some(12), Some(12)),
                step(stamped(A, 12, 0, 1), Observation::Ok, Some(12), Some(12)),
            ],
        ),
        (
            "two points: the watermark is unknown until both sent, then the earliest",
            &[A, B],
            vec![
                step(stamped(A, 10, 0, 1), Observation::Ok, None, Some(10)),
                step(stamped(B, 7, 0, 1), Observation::Ok, Some(7), Some(10)),
                step(stamped(B, 15, 0, 1), Observation::Ok, Some(10), Some(15)),
                step(stamped(A, 20, 0, 1), Observation::Ok, Some(15), Some(20)),
            ],
        ),
        (
            "a new epoch is a reset, reported with the watermark before it",
            &[A],
            vec![
                step(stamped(A, 100, 0, 1), Observation::Ok, Some(100), Some(100)),
                step(stamped(A, 40, 1, 1), reset(A, 0, 100, Some(100)), Some(40), Some(40)),
                step(stamped(A, 41, 1, 1), Observation::Ok, Some(41), Some(41)),
            ],
        ),
        (
            "epochs wrap: the epoch after the largest is the smallest, and newer",
            &[A],
            vec![
                step(stamped(A, 100, u32::MAX, 1), Observation::Ok, Some(100), Some(100)),
                step(
                    stamped(A, 30, 0, 1),
                    reset(A, u32::MAX, 100, Some(100)),
                    Some(30),
                    Some(30),
                ),
            ],
        ),
        (
            "a reset of one point before the other sent has no watermark before it",
            &[A, B],
            vec![
                step(stamped(A, 100, 0, 1), Observation::Ok, None, Some(100)),
                step(stamped(A, 40, 3, 1), reset(A, 0, 100, None), None, Some(40)),
                step(stamped(B, 50, 0, 1), Observation::Ok, Some(40), Some(50)),
            ],
        ),
        (
            "unstamped headers and headers of other points are faults that change nothing",
            &[A],
            vec![
                step(BatchHeader::default(), Observation::Fault, None, None),
                step(stamped(OTHER, 10, 0, 1), Observation::Fault, None, None),
                step(
                    BatchHeader {
                        admission_point: None,
                        ..stamped(A, 10, 0, 1)
                    },
                    Observation::Fault,
                    None,
                    None,
                ),
                step(stamped(A, 10, 0, 1), Observation::Ok, Some(10), Some(10)),
                step(stamped(OTHER, 50, 0, 1), Observation::Fault, Some(10), Some(10)),
            ],
        ),
        (
            "a tracker of no point has no watermark",
            &[],
            vec![step(stamped(A, 10, 0, 1), Observation::Fault, None, None)],
        ),
    ];
    for (name, upstream, steps) in cases {
        let mut tracker = WatermarkTracker::new(upstream.iter().copied());
        assert_eq!(tracker.watermark(), None, "{}: before any header", name);
        for (index, step) in steps.into_iter().enumerate() {
            assert_eq!(
                tracker.observe(&step.header),
                step.observation,
                "{}: observation {}",
                name,
                index
            );
            assert_eq!(
                tracker.watermark(),
                step.watermark.map(secs),
                "{}: watermark after {}",
                name,
                index
            );
            assert_eq!(
                tracker.max_latest(),
                step.max_latest.map(secs),
                "{}: latest after {}",
                name,
                index
            );
        }
    }
}

#[test]
fn a_tracker_lists_its_points_once_in_id_order() {
    let tracker = WatermarkTracker::new([B, A, B]);
    assert_eq!(tracker.upstream().collect::<Vec<_>>(), [A, B]);
    assert_eq!(tracker.progress(A), None);

    let mut tracker = tracker;
    tracker.observe(&stamped(B, 5, 2, 3));
    assert_eq!(
        tracker.progress(B),
        Some(&PointProgress {
            epoch: 2,
            latest: secs(5),
            max_generation: Some(GenerationId::new(3)),
        })
    );
}

#[test]
fn a_tracker_has_switched_once_every_point_sent_the_generation_or_a_later_one() {
    // (headers, generation asked about, switched)
    let cases: Vec<(&str, Vec<BatchHeader>, u64, bool)> = vec![
        ("nothing sent", vec![], 2, false),
        (
            "one point sent the new generation",
            vec![stamped(A, 10, 0, 2)],
            2,
            false,
        ),
        (
            "both sent it",
            vec![stamped(A, 10, 0, 2), stamped(B, 10, 0, 2)],
            2,
            true,
        ),
        (
            "one sent a later one",
            vec![stamped(A, 10, 0, 3), stamped(B, 10, 0, 2)],
            2,
            true,
        ),
        (
            "an older header afterwards does not undo it",
            vec![stamped(A, 10, 0, 2), stamped(B, 10, 0, 2), stamped(A, 11, 0, 1)],
            2,
            true,
        ),
        (
            "one still sends the old one",
            vec![stamped(A, 10, 0, 2), stamped(B, 10, 0, 1)],
            2,
            false,
        ),
        (
            "a header without a generation does not switch",
            vec![
                stamped(A, 10, 0, 2),
                BatchHeader {
                    generation: None,
                    ..stamped(B, 10, 0, 2)
                },
            ],
            2,
            false,
        ),
        (
            "a reset keeps the generations seen",
            vec![stamped(A, 10, 0, 2), stamped(B, 10, 0, 2), stamped(A, 5, 1, 2)],
            2,
            true,
        ),
    ];
    for (name, headers, new, expected) in cases {
        let mut tracker = WatermarkTracker::new([A, B]);
        for header in &headers {
            tracker.observe(header);
        }
        assert_eq!(tracker.switched(GenerationId::new(new)), expected, "{}", name);
    }
    // A tracker of no point has trivially switched.
    assert!(WatermarkTracker::new([]).switched(GenerationId::new(9)));
}

#[test]
fn an_epoch_is_newer_than_another_when_it_is_less_than_half_the_range_ahead() {
    assert!(epoch_newer(1, 0));
    assert!(epoch_newer(7, 3));
    assert!(epoch_newer(0, u32::MAX), "epochs wrap");
    assert!(epoch_newer(2, u32::MAX - 1));
    assert!(!epoch_newer(0, 1));
    assert!(!epoch_newer(u32::MAX, 0));
    assert!(!epoch_newer(5, 5), "an epoch is not newer than itself");
    assert!(epoch_newer(i32::MAX as u32, 0));
    assert!(!epoch_newer(1 << 31, 0), "half the range ahead is not newer");
}

/// Observes a header of `point` in epoch 0 after one in epoch 1, which one path in stamping order never delivers.
fn observe_an_older_epoch() -> (Observation, WatermarkTracker) {
    let mut tracker = WatermarkTracker::new([A]);
    assert_eq!(tracker.observe(&stamped(A, 100, 0, 1)), Observation::Ok);
    assert!(matches!(
        tracker.observe(&stamped(A, 40, 1, 2)),
        Observation::EpochSwitched { previous: 0, .. }
    ));
    let observation = tracker.observe(&stamped(A, 120, 0, 3));
    (observation, tracker)
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "admission point 0 stamped epoch 0 after epoch 1")]
fn a_header_of_an_older_epoch_after_a_newer_one_fails_a_debug_assertion() {
    observe_an_older_epoch();
}

#[test]
#[cfg(not(debug_assertions))]
fn a_header_of_an_older_epoch_after_a_newer_one_is_a_fault_the_tracker_ignores() {
    let (observation, tracker) = observe_an_older_epoch();
    assert_eq!(observation, Observation::Fault);
    // The point stays in its newer epoch, at its time and generation there.
    let progress = tracker.progress(A).unwrap();
    assert_eq!(
        (progress.epoch, progress.latest, progress.max_generation),
        (1, secs(40), Some(GenerationId::new(2)))
    );
}

#[test]
fn a_tracker_is_in_the_publication_epoch_when_every_recorded_point_is() {
    // (case, headers, the cut's recorded epochs, in the publication epoch)
    type Case = (&'static str, Vec<BatchHeader>, Vec<(AdmissionPointId, u32)>, bool);
    let cases: Vec<Case> = vec![
        (
            "the recorded point is in its epoch",
            vec![stamped(A, 10, 4, 1)],
            vec![(A, 4)],
            true,
        ),
        (
            "the recorded point's latest header is of an earlier epoch",
            vec![stamped(A, 10, 3, 1)],
            vec![(A, 4)],
            false,
        ),
        (
            "the recorded point reset after publication",
            vec![stamped(A, 10, 4, 1), stamped(A, 3, 5, 1)],
            vec![(A, 4)],
            false,
        ),
        ("the recorded point sent nothing", vec![], vec![(A, 4)], false),
        (
            "a point the cut does not record does not count",
            vec![stamped(A, 10, 4, 1), stamped(B, 10, 9, 1)],
            vec![(A, 4)],
            true,
        ),
        (
            "every recorded point must be in its epoch",
            vec![stamped(A, 10, 4, 1), stamped(B, 10, 9, 1)],
            vec![(A, 4), (B, 8)],
            false,
        ),
    ];
    for (name, headers, epochs, expected) in cases {
        let mut tracker = WatermarkTracker::new([A, B]);
        for header in &headers {
            tracker.observe(header);
        }
        assert_eq!(tracker.in_publication_epoch(&cut(100, &epochs)), expected, "{}", name);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(1024))]

    #[test]
    fn property_test_an_epoch_switch_reports_what_the_tracker_knew_before_it(
        base in proptest::prelude::any::<u32>(),
        headers in proptest::collection::vec((0u32..2, 0u64..50, proptest::prelude::any::<bool>()), 1..24),
        recorded in proptest::collection::vec((0u32..2, 0u32..3), 0..3),
    ) {
        // A host decides whether a reset's watermark is in a cut's publication epoch from the observation alone, with
        // no copy of the tracker: the rule evaluated after the switch with the point's previous epoch is the rule as it
        // stood before the header, and the switch reports the point's last time in its previous epoch. Each point's
        // epochs start at `base`, which can be near the end of the range, and a header may move its point to the next
        // one, as a clock reset does; the cut records epochs from `base` on.
        let epochs = recorded
            .iter()
            .map(|(point, offset)| (AdmissionPointId::new(*point), base.wrapping_add(*offset)))
            .collect::<Vec<_>>();
        let cut = cut(100, &epochs);
        let mut tracker = WatermarkTracker::new([A, B]);
        let mut current = std::collections::BTreeMap::new();
        for (point, t_a, switch) in headers {
            let point = AdmissionPointId::new(point);
            let (epoch, latest) = current.entry(point).or_insert((base, 0));
            if switch {
                *epoch = epoch.wrapping_add(1);
                *latest = 0;
            }
            // Times only grow within one epoch of a point, as one path in stamping order delivers them.
            *latest = (*latest).max(t_a);
            let (epoch, t_a) = (*epoch, *latest);
            let before = tracker.clone();
            match tracker.observe(&stamped(point, t_a, epoch, 1)) {
                Observation::EpochSwitched { point: switched, previous, final_latest, pre } => {
                    let known = before.progress(switched).expect("a point switches only after it sent something");
                    proptest::prop_assert_eq!(switched, point);
                    proptest::prop_assert_eq!((previous, final_latest), (known.epoch, known.latest));
                    proptest::prop_assert_eq!(pre, before.watermark());
                    proptest::prop_assert_eq!(
                        tracker.in_publication_epoch_with(&cut, switched, previous),
                        before.in_publication_epoch(&cut)
                    );
                }
                Observation::Ok => {}
                Observation::Fault => proptest::prop_assert!(false, "every header is stamped by an upstream point"),
            }
        }
    }
}

#[test]
fn a_scheduled_tick_fires_at_its_second_once_the_watermark_reached_it_and_gates_otherwise() {
    let gate = TickGate::default();
    assert_eq!(gate.grace(), TICK_GRACE);
    // (case, bound, watermark, wall, last flush, decision)
    type Case = (
        &'static str,
        bool,
        Option<AdmissionTime>,
        AdmissionTime,
        Option<u64>,
        TickDecision,
    );
    let cases: Vec<Case> = vec![
        (
            "no published generation binds the lineage",
            false,
            Some(secs(20)),
            secs(10),
            None,
            TickDecision::Skip,
        ),
        (
            "the watermark is unknown",
            true,
            None,
            secs(10),
            None,
            TickDecision::Skip,
        ),
        (
            "the watermark is past the tick's second",
            true,
            Some(millis(10_500)),
            millis(10_200),
            None,
            TickDecision::Fire { flush: secs(10) },
        ),
        (
            "the watermark is within the tick's second",
            true,
            Some(millis(10_100)),
            millis(10_900),
            None,
            TickDecision::Fire { flush: secs(10) },
        ),
        (
            "the watermark lags the tick's second",
            true,
            Some(millis(9_999)),
            millis(10_000),
            None,
            TickDecision::Gate { second: 10 },
        ),
        (
            "a wall clock that stepped back fires at the previous flush time",
            true,
            Some(secs(100)),
            secs(50),
            Some(60),
            TickDecision::Fire { flush: secs(60) },
        ),
        (
            "an earlier previous flush time does not hold the flush back",
            true,
            Some(secs(100)),
            secs(70),
            Some(60),
            TickDecision::Fire { flush: secs(70) },
        ),
    ];
    for (name, bound, watermark, wall, last, expected) in cases {
        assert_eq!(gate.due(bound, watermark, wall, last.map(secs)), expected, "{}", name);
    }
}

#[test]
fn a_gated_tick_fires_when_the_watermark_reaches_its_second_or_at_the_watermark_after_the_grace() {
    let gate = TickGate::new(Duration::from_secs(2));
    // (case, second, watermark, last flush, flush when a header raises the watermark, flush when the grace ends)
    type Case = (
        &'static str,
        u64,
        Option<AdmissionTime>,
        Option<u64>,
        Option<u64>,
        Option<u64>,
    );
    let cases: Vec<Case> = vec![
        ("the watermark is still unknown", 10, None, None, None, None),
        ("the watermark still lags", 10, Some(millis(9_800)), None, None, Some(9)),
        (
            "the watermark reached the second",
            10,
            Some(secs(10)),
            None,
            Some(10),
            Some(10),
        ),
        (
            "the watermark passed the second",
            10,
            Some(millis(11_300)),
            None,
            Some(10),
            Some(11),
        ),
        (
            "a later previous flush time holds",
            10,
            Some(millis(9_800)),
            Some(9),
            None,
            Some(9),
        ),
        (
            "a previous flush time past the second holds",
            10,
            Some(secs(12)),
            Some(12),
            Some(12),
            Some(12),
        ),
    ];
    for (name, second, watermark, last, on_watermark, on_grace) in cases {
        assert_eq!(
            gate.on_watermark(second, watermark, last.map(secs)),
            on_watermark.map(secs),
            "{}: a header raises the watermark",
            name
        );
        assert_eq!(
            gate.on_grace(watermark, last.map(secs)),
            on_grace.map(secs),
            "{}: the grace ends",
            name
        );
    }
}

#[test]
fn widths_line_up_at_their_least_common_multiple_within_the_transition_bound() {
    let widths = |secs: &[u64]| secs.iter().map(|secs| Duration::from_secs(*secs)).collect::<Vec<_>>();
    // (widths, least common multiple)
    let cases: Vec<(Vec<Duration>, Result<u64, CutError>)> = vec![
        (widths(&[10]), Ok(10)),
        (widths(&[10, 30]), Ok(30)),
        (widths(&[4, 6]), Ok(12)),
        (widths(&[10, 15, 20]), Ok(60)),
        (widths(&[60, 300]), Ok(300)),
        (widths(&[5, 300, 300]), Ok(300)),
        (
            widths(&[120, 180]),
            Err(CutError::TooLong {
                widths: widths(&[120, 180]),
                lcm: 360,
            }),
        ),
        (
            widths(&[7, 300]),
            Err(CutError::TooLong {
                widths: widths(&[7, 300]),
                lcm: 2_100,
            }),
        ),
        (Vec::new(), Err(CutError::NoWidths)),
        (
            vec![Duration::from_secs(10), Duration::ZERO],
            Err(CutError::NotWholeSeconds { width: Duration::ZERO }),
        ),
        (
            vec![Duration::from_millis(1_500)],
            Err(CutError::NotWholeSeconds {
                width: Duration::from_millis(1_500),
            }),
        ),
    ];
    for (widths, expected) in cases {
        assert_eq!(lcm_secs(&widths).map(NonZeroU64::get), expected, "{:?}", widths);
    }
    let error = lcm_secs(&widths(&[120, 180])).unwrap_err().to_string();
    assert!(
        error.contains("120 s, 180 s line up only every 360 seconds"),
        "{}",
        error
    );
}

#[test]
fn a_cut_takes_effect_at_the_first_common_boundary_at_least_the_margin_away() {
    let lcm = |secs: u64| NonZeroU64::new(secs).unwrap();
    // (now, lcm, margin, cut time)
    let cases = [
        (secs(95), lcm(30), Duration::from_secs(1), secs(120)),
        (secs(105), lcm(10), Duration::from_secs(1), secs(110)),
        (millis(98_500), lcm(10), Duration::from_secs(1), secs(100)),
        // A boundary exactly the margin away is the cut.
        (secs(99), lcm(10), Duration::from_secs(1), secs(100)),
        (secs(100), lcm(10), Duration::from_secs(1), secs(110)),
        (secs(100), lcm(10), Duration::ZERO, secs(100)),
        (secs(1), lcm(300), Duration::from_secs(1), secs(300)),
        // Saturates at the largest admission time.
        (
            AdmissionTime::from_unix_nanos(u64::MAX - 1),
            lcm(10),
            Duration::from_secs(1),
            AdmissionTime::from_unix_nanos(u64::MAX),
        ),
    ];
    for (now, lcm, margin, expected) in cases {
        assert_eq!(cut_time(now, lcm, margin), expected, "now {}, lcm {}", now, lcm);
    }
}

#[test]
fn a_retiring_lineage_flushes_no_later_than_the_cut_in_its_publication_epoch() {
    let boundary = cut(100, &[(A, 0)]);
    // (flush, cut, in the publication epoch, flush time)
    let cases = [
        (secs(90), Some(&boundary), true, secs(90)),
        (secs(130), Some(&boundary), true, secs(100)),
        (secs(100), Some(&boundary), true, secs(100)),
        // An earlier epoch's times are pre-cut, whatever their value.
        (secs(130), Some(&boundary), false, secs(130)),
        // Before the cut is armed nothing clamps.
        (secs(130), None, true, secs(130)),
    ];
    for (flush, cut, in_epoch, expected) in cases {
        assert_eq!(
            retiring_flush_time(flush, cut, in_epoch),
            expected,
            "{} {:?} {}",
            flush,
            cut.map(|cut| cut.at),
            in_epoch
        );
    }
}

#[test]
fn a_drain_is_stamped_with_the_first_generation_that_binds_its_lineage() {
    let old = generation(1);
    let new = generation(2);
    let armed = generation(3);
    armed.set_cut(cut(100, &[(A, 0)])).unwrap();
    let unarmed = generation(4);
    let steady = AdmissionSchedule::Steady(old.clone());
    let cutting = AdmissionSchedule::Cut {
        old: old.clone(),
        new: new.clone(),
        at: secs(100),
    };
    let both = |_: GenerationId| true;
    let only = |id: u64| move |generation: GenerationId| generation == GenerationId::new(id);
    let none = |_: GenerationId| false;
    let switched = |_: GenerationId| true;
    let not_switched = |_: GenerationId| false;

    type Binds = Box<dyn Fn(GenerationId) -> bool>;
    type Retiring<'a> = Option<(&'a GenerationRef, &'a GenerationRef)>;
    // (case, schedule, binds, switched, retiring (owner, by), generation)
    type Case<'a> = (
        &'static str,
        &'a AdmissionSchedule,
        Binds,
        Binds,
        Retiring<'a>,
        Option<u64>,
    );
    let cases: Vec<Case<'_>> = vec![
        ("steady", &steady, Box::new(both), Box::new(switched), None, Some(1)),
        (
            "steady, not bound",
            &steady,
            Box::new(none),
            Box::new(switched),
            None,
            None,
        ),
        (
            "a cut before every point switched: old first",
            &cutting,
            Box::new(both),
            Box::new(not_switched),
            None,
            Some(1),
        ),
        (
            "a cut once every point switched: new first",
            &cutting,
            Box::new(both),
            Box::new(switched),
            None,
            Some(2),
        ),
        (
            "a lineage only the new generation binds, before the switch",
            &cutting,
            Box::new(only(2)),
            Box::new(not_switched),
            None,
            Some(2),
        ),
        (
            "a lineage only the old generation binds, after the switch",
            &cutting,
            Box::new(only(1)),
            Box::new(switched),
            None,
            Some(1),
        ),
        (
            "a lineage of a generation not yet published",
            &steady,
            Box::new(only(5)),
            Box::new(switched),
            None,
            None,
        ),
        (
            "a retiring lineage whose replacement is armed drains under its owner",
            &cutting,
            Box::new(only(2)),
            Box::new(switched),
            Some((&old, &armed)),
            Some(1),
        ),
        (
            "a retiring lineage whose replacement is not armed follows the schedule",
            &cutting,
            Box::new(both),
            Box::new(switched),
            Some((&old, &unarmed)),
            Some(2),
        ),
    ];
    for (name, schedule, binds, switched, retiring, expected) in cases {
        let retiring = retiring.map(|(owner, by)| RetiringRefs { owner, by });
        let stamped = stamping_generation(schedule, binds, switched, retiring);
        assert_eq!(
            stamped.map(|generation| generation.id()),
            expected.map(GenerationId::new),
            "{}",
            name
        );
    }
}

#[test]
fn input_ends_at_the_later_of_the_latest_admission_and_the_wall_clock() {
    let cases = [
        (Some(secs(10)), secs(5), secs(10)),
        (Some(secs(10)), secs(12), secs(12)),
        (None, secs(12), secs(12)),
    ];
    for (latest, wall, expected) in cases {
        assert_eq!(shutdown_time(latest, wall), expected, "{:?} {}", latest, wall);
    }
}

#[test]
fn time_driven_output_is_stamped_with_times_that_only_grow_within_an_epoch() {
    let point = AdmissionPointId::new(5);
    let mut clock = OutputClock::new(point);
    assert_eq!((clock.point(), clock.epoch(), clock.latest()), (point, 0, None));

    // (flush time, reset before it, stamped time, epoch)
    let cases = [
        (10, false, 10, 0),
        (20, false, 20, 0),
        // A flush time below the previous stamp, such as a reset drain's, keeps the stamp.
        (15, false, 20, 0),
        // After a reset the epoch moves on and times start over.
        (5, true, 5, 1),
        (6, false, 6, 1),
    ];
    for (flush, reset, stamped, epoch) in cases {
        if reset {
            clock.reset();
        }
        let header = clock.stamp(secs(flush), generation(3));
        assert_eq!(header.admitted_at, Some(secs(stamped)), "flush at {}", flush);
        assert_eq!(header.admission_point, Some(point));
        assert_eq!(header.epoch, epoch, "flush at {}", flush);
        assert_eq!(header.generation_id(), Some(GenerationId::new(3)));
        assert_eq!(header.entry, None);
        assert_eq!(clock.latest(), Some(secs(stamped)));
    }

    // Epochs wrap.
    let mut clock = OutputClock {
        epoch: u32::MAX,
        ..OutputClock::new(point)
    };
    clock.reset();
    assert_eq!(clock.epoch(), 0);
}

#[test]
fn a_trackers_view_of_one_epoch_counts_the_points_in_it() {
    let mut tracker = WatermarkTracker::new([A, B]);
    assert_eq!(tracker.watermark_in(0), None);
    assert!(
        !tracker.switched_epoch(0),
        "a point that sent nothing has switched to no epoch"
    );
    tracker.observe(&stamped(A, 100, 0, 1));
    tracker.observe(&stamped(B, 90, 0, 1));
    assert_eq!(tracker.watermark_in(0), Some(secs(90)));
    assert_eq!(tracker.max_latest_in(0), Some(secs(100)));
    assert!(tracker.switched_epoch(0));
    assert!(!tracker.switched_epoch(1));

    // A switches to epoch 1: until B does, epoch 1 has no watermark, and epoch 0's counts A's final latest time there.
    tracker.observe(&stamped(A, 5, 1, 1));
    assert_eq!(tracker.watermark_in(1), None);
    assert_eq!(tracker.max_latest_in(1), Some(secs(5)));
    assert_eq!(tracker.watermark_in(0), Some(secs(90)));
    assert_eq!(tracker.max_latest_in(0), Some(secs(100)));
    assert!(!tracker.switched_epoch(1));
    tracker.observe(&stamped(B, 95, 0, 1));
    assert_eq!(
        tracker.watermark_in(0),
        Some(secs(95)),
        "B's old-epoch batches still raise epoch 0's watermark"
    );
    tracker.observe(&stamped(B, 7, 1, 1));
    assert!(tracker.switched_epoch(1));
    assert!(tracker.switched_epoch(0), "every point is newer than epoch 0");
    assert_eq!(tracker.watermark_in(1), Some(secs(5)));
    assert_eq!(
        tracker.watermark_in(0),
        Some(secs(95)),
        "the final latest times are exact once both left"
    );
    assert_eq!(WatermarkTracker::new([]).watermark_in(0), None);
}

#[test]
fn a_point_that_skips_an_epoch_is_left_out_of_its_watermark_and_finals_are_kept_for_the_open_epochs_only() {
    let mut tracker = WatermarkTracker::new([A, B]);
    tracker.observe(&stamped(A, 100, 0, 1));
    tracker.observe(&stamped(B, 90, 0, 1));
    // A stamps in epochs 1 and 2; B goes from 0 to 2 directly, two resets while it was blocked.
    tracker.observe(&stamped(A, 10, 1, 1));
    tracker.observe(&stamped(A, 20, 2, 1));
    tracker.observe(&stamped(B, 30, 2, 1));
    assert_eq!(tracker.watermark_in(1), Some(secs(10)), "only A stamped in epoch 1");
    assert_eq!(tracker.max_latest_in(1), Some(secs(10)));
    assert_eq!(tracker.watermark_in(0), Some(secs(90)));
    assert_eq!(tracker.watermark_in(2), Some(secs(20)));

    // Past `MAX_OPEN_EPOCHS` switches, the oldest final time is no longer kept: the point is left out of it. B's time
    // in epoch 0 is later than A's, so only A's absence makes the watermark B's.
    let mut tracker = WatermarkTracker::new([A, B]);
    tracker.observe(&stamped(B, 150, 0, 1));
    for epoch in 0..=MAX_OPEN_EPOCHS as u32 {
        tracker.observe(&stamped(A, 100 + u64::from(epoch), epoch, 1));
    }
    assert_eq!(
        tracker.watermark_in(0),
        Some(secs(100)),
        "A's final time in 0 is still kept after `MAX_OPEN_EPOCHS` switches"
    );
    tracker.observe(&stamped(A, 200, MAX_OPEN_EPOCHS as u32 + 1, 1));
    assert_eq!(tracker.watermark_in(0), Some(secs(150)), "A's final time in 0 is gone");
    assert_eq!(tracker.watermark_in(1), None, "B has not reached epoch 1");
}

#[test]
fn points_with_consecutive_ids_are_found_by_index_and_others_by_search() {
    let lanes = (10..14).map(AdmissionPointId::new).collect::<Vec<_>>();
    let mut tracker = WatermarkTracker::new(lanes.iter().copied());
    for (offset, point) in lanes.iter().enumerate() {
        assert_eq!(
            tracker.observe(&stamped(*point, 100 + offset as u64, 0, 1)),
            Observation::Ok
        );
    }
    assert_eq!(
        tracker.observe(&stamped(AdmissionPointId::new(14), 1, 0, 1)),
        Observation::Fault
    );
    assert_eq!(
        tracker.observe(&stamped(AdmissionPointId::new(9), 1, 0, 1)),
        Observation::Fault
    );
    assert_eq!(tracker.watermark(), Some(secs(100)));
    assert_eq!(tracker.progress(AdmissionPointId::new(13)).unwrap().latest, secs(103));

    let mut sparse = WatermarkTracker::new([OTHER, A]);
    assert_eq!(sparse.observe(&stamped(OTHER, 5, 0, 1)), Observation::Ok);
    assert_eq!(sparse.observe(&stamped(B, 5, 0, 1)), Observation::Fault);
    assert_eq!(sparse.progress(OTHER).unwrap().latest, secs(5));
}

/// Observes `header` and steps `window` with it, as a state lane does.
fn step(tracker: &mut WatermarkTracker, window: &mut ResetWindow, header: BatchHeader) -> WindowStep {
    assert_ne!(tracker.observe(&header), Observation::Fault);
    window.step(tracker, &header)
}

#[test]
fn with_one_point_a_reset_window_opens_and_closes_on_one_header() {
    let mut tracker = WatermarkTracker::new([A]);
    let mut window = ResetWindow::default();
    assert_eq!(step(&mut tracker, &mut window, stamped(A, 100, 0, 1)), WindowStep::None);
    assert_eq!(step(&mut tracker, &mut window, stamped(A, 110, 0, 1)), WindowStep::None);
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 5, 1, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(110)))]
        },
        "epoch 0 closes at the point's final latest time, the watermark before the reset"
    );
    assert!(!window.is_open());
    assert_eq!(window.target(), 1);
    assert_eq!(step(&mut tracker, &mut window, stamped(A, 6, 1, 1)), WindowStep::None);
}

#[test]
fn a_reset_window_routes_each_batch_by_its_epoch_until_every_point_switched() {
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 5, 1, 1)),
        WindowStep::Opened { older: 0, target: 1 }
    );
    assert!(window.is_open() && window.holds(0) && window.holds(1));
    assert_eq!(
        step(&mut tracker, &mut window, stamped(B, 95, 0, 1)),
        WindowStep::Route(EpochSide::Older(0))
    );
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 6, 1, 1)),
        WindowStep::Route(EpochSide::Target)
    );
    assert_eq!(
        step(&mut tracker, &mut window, stamped(B, 7, 1, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(95)))]
        },
        "epoch 0 drains at the earliest final latest time: B's last old-epoch stamp"
    );
    assert!(!window.is_open() && !window.holds(0));
}

#[test]
fn a_reset_inside_a_window_moves_its_target_and_closes_every_older_epoch_together() {
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    step(&mut tracker, &mut window, stamped(A, 5, 1, 1));
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 50, 2, 1)),
        WindowStep::Opened { older: 1, target: 2 }
    );
    assert_eq!(window.older(), &[0, 1]);
    // B skips epoch 1: once it is in 2, both older epochs close, oldest first, epoch 1 at A's final time alone.
    assert_eq!(
        step(&mut tracker, &mut window, stamped(B, 60, 2, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(90))), (1, Some(secs(5)))]
        }
    );
}

#[test]
fn a_window_that_would_keep_more_than_the_most_epochs_open_closes_the_oldest_at_once() {
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    for epoch in 1..=MAX_OPEN_EPOCHS as u32 {
        assert!(matches!(
            step(&mut tracker, &mut window, stamped(A, u64::from(epoch), epoch, 1)),
            WindowStep::Opened { .. }
        ));
    }
    let overflow = MAX_OPEN_EPOCHS as u32 + 1;
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 50, overflow, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(90)))]
        },
        "epoch 0 closes at its watermark so far, B's latest there"
    );
    assert!(window.is_open() && !window.holds(0));
    assert_eq!(window.older().len(), MAX_OPEN_EPOCHS as usize);
}

#[test]
fn a_header_several_epochs_newer_holds_every_epoch_in_between_open() {
    // B skipped epoch 1, two resets while it was blocked, and its first header in epoch 2 reaches the lane before A's
    // batch of epoch 1: epoch 1 is held open all the same, and closes at A's final time there, B left out.
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    assert_eq!(
        step(&mut tracker, &mut window, stamped(B, 30, 2, 1)),
        WindowStep::Opened { older: 0, target: 2 }
    );
    assert_eq!(window.older(), &[0, 1]);
    assert!(window.holds(1));
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 10, 1, 1)),
        WindowStep::Route(EpochSide::Older(1))
    );
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 20, 2, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(90))), (1, Some(secs(10)))]
        }
    );
    assert!(!window.is_open());
}

#[test]
fn a_header_further_ahead_than_the_most_epochs_a_window_keeps_closes_the_oldest_between_at_once() {
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    let ahead = MAX_OPEN_EPOCHS as u32 + 2;
    // Epochs 0 to `ahead - 1` would be open: the two oldest close at once, epoch 1, which no point stamped in yet,
    // with no watermark, since B has not reached it.
    assert_eq!(
        step(&mut tracker, &mut window, stamped(A, 5, ahead, 1)),
        WindowStep::Closed {
            epochs: vec![(0, Some(secs(90))), (1, None)]
        }
    );
    assert!(window.is_open() && !window.holds(1) && window.holds(2));
    assert_eq!(window.older(), (2..ahead).collect::<Vec<_>>().as_slice());
}

#[test]
fn a_lane_whose_input_ends_inside_a_window_closes_every_older_epoch() {
    let mut tracker = WatermarkTracker::new([A, B]);
    let mut window = ResetWindow::default();
    step(&mut tracker, &mut window, stamped(A, 100, 0, 1));
    step(&mut tracker, &mut window, stamped(B, 90, 0, 1));
    step(&mut tracker, &mut window, stamped(A, 5, 1, 1));
    assert_eq!(window.close(&tracker), vec![(0, Some(secs(90)))]);
    assert!(!window.is_open());
    assert_eq!(window.target(), 1);
}

#[test]
fn an_epoch_is_the_publication_epoch_when_every_recorded_point_was_in_it() {
    let tracker = WatermarkTracker::new([A, B]);
    assert!(tracker.is_publication_epoch(&cut(100, &[(A, 3), (B, 3)]), 3));
    assert!(!tracker.is_publication_epoch(&cut(100, &[(A, 3), (B, 3)]), 4));
    assert!(
        tracker.is_publication_epoch(&cut(100, &[(OTHER, 1)]), 4),
        "a cut recording neither point"
    );
}

#[test]
fn a_tick_decided_at_a_shared_second_is_the_tick_decided_at_the_wall_clocks() {
    let gate = TickGate::default();
    for (watermark, second) in [(Some(secs(100)), 100), (Some(secs(99)), 100), (None, 100)] {
        assert_eq!(
            gate.due_at_second(true, watermark, second, Some(secs(80))),
            gate.due(true, watermark, millis(second * 1_000 + 400), Some(secs(80)))
        );
    }
    assert_eq!(
        gate.due_at_second(false, Some(secs(100)), 100, None),
        TickDecision::Skip
    );
}

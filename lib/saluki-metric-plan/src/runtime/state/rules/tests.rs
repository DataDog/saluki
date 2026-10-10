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
    let reset = |point, pre: Option<u64>| Observation::Reset {
        point,
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
                step(stamped(A, 40, 1, 1), reset(A, Some(100)), Some(40), Some(40)),
                step(stamped(A, 41, 1, 1), Observation::Ok, Some(41), Some(41)),
                // Epochs wrap: any change is a reset, even to a smaller epoch.
                step(stamped(A, 30, 0, 1), reset(A, Some(41)), Some(30), Some(30)),
            ],
        ),
        (
            "a reset of one point before the other sent has no watermark before it",
            &[A, B],
            vec![
                step(stamped(A, 100, 0, 1), Observation::Ok, None, Some(100)),
                step(stamped(A, 40, 3, 1), reset(A, None), None, Some(40)),
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

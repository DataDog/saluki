//! Tests of emission filters: what a flush skips because a filter downstream drops it, and how it counts what it
//! skipped.

use std::sync::Arc;

use super::*;

const B: u64 = 1_700_000_000;

/// A filter downstream of aggregation, for tests: it drops a record of one of `kinds` that has a name in `names`, the
/// tag `tag`, or the unit `unit`.
///
/// As an emission filter it is consulted for the kinds in `pushed` only, and for histograms only if `histograms` is set
/// and its decision reads only tags.
#[derive(Clone, Debug)]
pub(in crate::aggregate) struct TestFilter {
    pub(in crate::aggregate) site: SiteIndex,
    pub(in crate::aggregate) names: Vec<&'static str>,
    pub(in crate::aggregate) tag: Option<&'static str>,
    pub(in crate::aggregate) unit: Option<&'static str>,
    pub(in crate::aggregate) kinds: Vec<OutputKind>,
    pub(in crate::aggregate) pushed: Vec<OutputKind>,
    pub(in crate::aggregate) histograms: bool,
}

const ALL_KINDS: [OutputKind; 4] = [
    OutputKind::Rate,
    OutputKind::Gauge,
    OutputKind::Set,
    OutputKind::Distribution,
];

impl TestFilter {
    /// A filter that drops the records with any of `names`, of every kind, and is consulted for every kind.
    pub(in crate::aggregate) fn names(names: &[&'static str]) -> Self {
        Self {
            site: SiteIndex::new(7),
            names: names.to_vec(),
            tag: None,
            unit: None,
            kinds: ALL_KINDS.to_vec(),
            pushed: ALL_KINDS.to_vec(),
            histograms: false,
        }
    }

    fn matches(&self, context: &Context, kind: OutputKind, metadata: &MetricMetadata) -> bool {
        let name: &str = context.name();
        self.kinds.contains(&kind)
            && (self.names.contains(&name)
                || self
                    .tag
                    .is_some_and(|tag| context.tags().into_iter().any(|held| held.as_str() == tag))
                || self.unit.is_some_and(|unit| &*metadata.unit == unit))
    }

    /// Returns `true` if the filter, downstream, drops `record`: what it does with every record that reaches it.
    pub(in crate::aggregate) fn drops_record(&self, record: &Metric) -> bool {
        let kind = OutputKind::of(record.values()).expect("aggregation never emits histograms");
        self.matches(record.context(), kind, record.metadata())
    }

    /// Returns `true` if the filter decides by nothing but tags, so that it decides every record of a histogram's
    /// expansion alike.
    fn reads_only_tags(&self) -> bool {
        self.names.is_empty() && self.unit.is_none() && self.kinds.len() == ALL_KINDS.len()
    }
}

impl EmissionFilter for TestFilter {
    fn drops(&self, context: &Context, kind: OutputKind, metadata: &MetricMetadata) -> bool {
        self.pushed.contains(&kind) && self.matches(context, kind, metadata)
    }

    fn drops_histogram(&self, context: &Context, metadata: &MetricMetadata) -> bool {
        // Any kind would do: the filter reads none.
        self.histograms && self.reads_only_tags() && self.matches(context, OutputKind::Gauge, metadata)
    }

    fn site(&self) -> SiteIndex {
        self.site
    }
}

pub(in crate::aggregate) fn arb_filter() -> impl Strategy<Value = TestFilter> {
    let kinds = || {
        prop_oneof![
            2 => Just(ALL_KINDS.to_vec()),
            1 => proptest::sample::subsequence(&ALL_KINDS[..], 0..=4),
        ]
    };
    (
        proptest::sample::subsequence(&["a", "b", "c", "d"][..], 0..=2),
        proptest::option::of(proptest::sample::select(&["t:1", "t:2"][..])),
        proptest::option::weighted(0.2, Just("millisecond")),
        kinds(),
        kinds(),
        any::<bool>(),
    )
        .prop_map(|(names, tag, unit, kinds, pushed, histograms)| TestFilter {
            site: SiteIndex::new(3),
            names,
            tag,
            unit,
            kinds,
            pushed,
            histograms,
        })
}

/// Flushes `core` at `now` with `params`, and returns what it emitted and the outcome.
fn flush_with(
    core: &mut AggregateCore, now: u64, last: bool, params: &FlushParams,
) -> Option<(Vec<Metric>, FlushOutcome)> {
    let mut flush = core.begin_flush(now, last, params)?;
    let mut emitted = Vec::new();
    while core.flush_step(&mut flush, |metric| emitted.push(metric)) {}
    Some((emitted, core.end_flush(flush)))
}

fn drain_with(core: &mut AggregateCore, params: &FlushParams) -> Option<(Vec<Metric>, FlushOutcome)> {
    let mut drain = core.begin_drain(params)?;
    let mut emitted = Vec::new();
    while core.flush_step(&mut drain, |metric| emitted.push(metric)) {}
    Some((emitted, core.end_flush(drain)))
}

fn tagged_gauge(name: &'static str, tag: &'static str) -> Metric {
    Metric::gauge(
        Context::from_parts(name, [Tag::from(tag)].into_iter().collect::<TagSet>()),
        1.0,
    )
}

fn names(records: &[Metric]) -> Vec<String> {
    let mut names = records
        .iter()
        .map(|record| record.context().name().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn a_flush_skips_the_records_its_filter_drops_and_counts_them_by_site() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let mut core = AggregateCore::new(
        AggregateParams {
            window: BUCKET_WIDTH_SECS,
            context_limit: 100,
            counter_expiry: None,
            flush_open_windows: false,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::new(&MetricsBuilder::default()),
    );
    let mut params = core.flush.clone();
    params.filter = Some(Arc::new(TestFilter {
        // Only gauges and rates are judged at emission: the filter's set records stay for it to drop downstream.
        pushed: vec![OutputKind::Gauge, OutputKind::Rate],
        ..TestFilter::names(&["drop.g", "drop.c", "drop.s"])
    }));
    core.admit(
        B + 1,
        [
            Metric::gauge("drop.g", ScalarPoints::from([(B + 1, 1.0), (B + 2, 2.0)])),
            Metric::gauge("keep.g", 1.0),
            Metric::counter("drop.c", 1.0),
            Metric::set("drop.s", "member"),
            Metric::histogram("drop.h", [1.0, 2.0]),
        ],
    );

    let (emitted, outcome) = flush_with(&mut core, B + 10, false, &params).unwrap();
    assert_eq!(
        names(&emitted),
        vec![
            "drop.h.95percentile",
            "drop.h.avg",
            "drop.h.count",
            "drop.h.max",
            "drop.h.median",
            "drop.s",
            "keep.g",
        ]
    );
    // The gauge's two timestamped points keep their timestamps, so its record would have carried both.
    assert_eq!(
        outcome.suppressed.as_slice(),
        &[(SiteIndex::new(7), Suppressed { records: 2, points: 3 })]
    );
    // Every context counts as flushed, skipped or not, and is removed alike: all but the counter, which stays idle
    // without an expiry, skipped or not.
    assert_eq!(
        recorder.counter(("aggregate_flushed_total", &[("data_type", "series")])),
        Some(5)
    );
    assert_eq!(
        snapshot_set(core.snapshot_contexts()),
        vec!["drop.c None Counter None".to_string()]
    );

    // Without a filter, nothing is skipped and nothing is counted.
    core.admit(B + 11, [Metric::gauge("drop.g", 1.0)]);
    let unfiltered = core.flush.clone();
    let (emitted, outcome) = flush_with(&mut core, B + 20, false, &unfiltered).unwrap();
    assert_eq!(names(&emitted), vec!["drop.g"]);
    assert!(outcome.suppressed.is_empty());
}

#[test]
fn a_histogram_is_skipped_whole_only_by_a_filter_that_judges_its_whole_expansion() {
    let histograms =
        HistogramConfiguration::try_new(&["max".to_string(), "count".to_string()], &[], true, String::new()).unwrap();
    let make = || {
        AggregateCore::new(
            AggregateParams {
                window: BUCKET_WIDTH_SECS,
                context_limit: 100,
                counter_expiry: None,
                flush_open_windows: false,
                histograms: histograms.clone(),
            },
            AggregateTelemetry::noop(),
        )
    };
    let histogram = |tag| {
        Metric::histogram(
            Context::from_parts("h", [Tag::from(tag)].into_iter().collect::<TagSet>()),
            [1.0, 2.0],
        )
    };
    let by_tag = TestFilter {
        names: Vec::new(),
        tag: Some("t:1"),
        histograms: true,
        ..TestFilter::names(&[])
    };

    // A tag filter that judges histograms skips the distribution copy and both statistics of a tagged histogram.
    let mut core = make();
    let mut params = core.flush.clone();
    params.filter = Some(Arc::new(by_tag.clone()));
    core.admit(B + 1, [histogram("t:1"), histogram("t:2")]);
    let (emitted, outcome) = flush_with(&mut core, B + 10, false, &params).unwrap();
    assert_eq!(names(&emitted), vec!["h", "h.count", "h.max"]);
    assert!(emitted.iter().all(|record| !by_tag.drops_record(record)));
    assert_eq!(
        outcome.suppressed.as_slice(),
        &[(SiteIndex::new(7), Suppressed { records: 3, points: 3 })]
    );

    // The same filter, not judging histograms at emission, leaves every record of the expansion to itself downstream.
    let mut core = make();
    params.filter = Some(Arc::new(TestFilter {
        histograms: false,
        ..by_tag.clone()
    }));
    core.admit(B + 1, [histogram("t:1")]);
    let (emitted, outcome) = flush_with(&mut core, B + 10, false, &params).unwrap();
    assert_eq!(names(&emitted), vec!["h", "h.count", "h.max"]);
    assert!(outcome.suppressed.is_empty());
}

#[test]
fn a_drain_skips_what_its_filter_drops_whatever_the_bucket() {
    let mut core = AggregateCore::new(
        AggregateParams {
            window: BUCKET_WIDTH_SECS,
            context_limit: 100,
            counter_expiry: None,
            flush_open_windows: false,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::noop(),
    );
    let mut params = core.flush.clone();
    params.filter = Some(Arc::new(TestFilter {
        tag: Some("t:1"),
        ..TestFilter::names(&[])
    }));
    core.admit(B + 1, [tagged_gauge("a", "t:1"), tagged_gauge("b", "t:2")]);
    core.admit(B + 12, [tagged_gauge("a", "t:1"), tagged_gauge("c", "t:1")]);

    let (emitted, outcome) = drain_with(&mut core, &params).unwrap();
    assert_eq!(names(&emitted), vec!["b"]);
    assert_eq!(
        outcome.suppressed.as_slice(),
        &[(SiteIndex::new(7), Suppressed { records: 2, points: 3 })]
    );
    assert!(core.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn property_test_emission_filter_equals_post_filter(
        params in arb_params(),
        start in 0u64..3600,
        steps in arb_steps(),
        filter in arb_filter(),
    ) {
        // The frozen state machine without a filter, and a core whose flushes skip what `filter` drops. At every flush,
        // the core emits exactly the frozen machine's records less some that the filter drops downstream, and counts
        // those as suppressed: so the filter's output, and what it drops, are the same either way. Everything else,
        // admission, state, and telemetry, is the frozen machine's.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime builds");
        let reference_recorder = TestRecorder::default();
        let kernel_recorder = TestRecorder::default();
        let mut expected = reference_run_loop(&params, &reference_recorder);
        let mut actual = kernel_core(&params, &kernel_recorder);
        let mut flush_params = actual.flush.clone();
        flush_params.filter = Some(Arc::new(filter.clone()));

        let mut now = 1_700_000_000 + start;
        let last_step = steps.len();
        for (position, step) in steps.iter().chain([Step::Tick { after: 1 }].iter()).enumerate() {
            match step {
                Step::Admit { after, metrics } => {
                    now = now.saturating_add_signed(*after);
                    let metrics = metrics.iter().map(|metric| metric.build(now)).collect::<Vec<_>>();
                    let (inserted, _) = expected.events(now, metrics.iter().cloned().map(Event::Metric));
                    let outcome = actual.admit(now, metrics);
                    prop_assert_eq!(outcome.accepted, inserted.iter().filter(|inserted| **inserted).count());
                }
                Step::Tick { after } => {
                    now = now.saturating_add_signed(*after);
                    let last = position == last_step;
                    let reference_flush = runtime.block_on(reference_tick(&mut expected, now, last));
                    match (reference_flush, flush_with(&mut actual, now, last, &flush_params)) {
                        (None, None) => {}
                        (Some((Ok(recovered), unfiltered)), Some((emitted, outcome))) => {
                            prop_assert_eq!(outcome.recovered, recovered);
                            // What the core emitted, plus the records it skipped, which the filter drops, is
                            // everything.
                            let mut emitted_left = multiset(&emitted);
                            let mut skipped = Vec::new();
                            for record in &unfiltered {
                                let described = describe(record);
                                match emitted_left.iter().position(|emitted| *emitted == described) {
                                    Some(position) => {
                                        emitted_left.remove(position);
                                    }
                                    None => skipped.push(record),
                                }
                            }
                            prop_assert!(
                                emitted_left.is_empty(),
                                "records the machine did not emit: {:?}",
                                emitted_left
                            );
                            prop_assert!(skipped.iter().all(|record| filter.drops_record(record)), "flush at {}", now);
                            let mut counted = Suppressed::default();
                            for (site, counts) in &outcome.suppressed {
                                prop_assert_eq!(*site, filter.site);
                                counted.add(*counts);
                            }
                            prop_assert_eq!(counted.records, skipped.len() as u64, "flush at {}", now);
                            prop_assert_eq!(
                                counted.points,
                                skipped.iter().map(|record| record.values().len() as u64).sum::<u64>()
                            );
                        }
                        (expected, actual) => {
                            return Err(TestCaseError::fail(format!(
                                "flush at {}: the frozen state machine flushed: {:?}, the core flushed: {:?}",
                                now,
                                expected.map(|(outcome, _)| outcome),
                                actual.map(|(_, outcome)| outcome)
                            )));
                        }
                    }
                }
            }
            prop_assert_eq!(expected.state.context_limit_breached(), actual.context_limit_breached());
            prop_assert_eq!(
                snapshot_set(expected.state.snapshot_contexts()),
                snapshot_set(actual.snapshot_contexts())
            );
        }
        prop_assert_eq!(telemetry_values(&reference_recorder), telemetry_values(&kernel_recorder));
    }
}

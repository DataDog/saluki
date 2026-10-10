//! Tests of budgets of contexts shared between cores, of kind conflicts, and of dropping a core.

use saluki_core::execution::StateBudget;

use super::*;

const B: u64 = 1_700_000_000;

fn params(context_limit: usize) -> AggregateParams {
    AggregateParams {
        window: BUCKET_WIDTH_SECS,
        context_limit,
        counter_expiry: None,
        flush_open_windows: false,
        histograms: HistogramConfiguration::default(),
    }
}

fn shared_core(context_limit: usize, global: &StateBudget, node: &StateBudget) -> AggregateCore {
    let budget = ContextBudget {
        global: global.clone(),
        node: node.clone(),
    };
    AggregateCore::with_budget(params(context_limit), budget, AggregateTelemetry::noop())
}

fn gauges(names: &[&'static str]) -> Vec<Metric> {
    names.iter().map(|name| Metric::gauge(*name, 1.0)).collect()
}

/// A metric for context `x` of the given shape, with one point at `ts`.
fn shaped(shape: &str, ts: u64) -> Metric {
    let mut values = match shape {
        "counter" => MetricValues::counter(1.0),
        "rate/10s" => MetricValues::rate(1.0, Duration::from_secs(10)),
        "rate/20s" => MetricValues::rate(1.0, Duration::from_secs(20)),
        "gauge" => MetricValues::gauge(1.0),
        "set" => MetricValues::set("member"),
        "histogram" => MetricValues::histogram([1.0]),
        "distribution" => MetricValues::distribution(&[1.0][..]),
        other => panic!("no shape {}", other),
    };
    values.set_timestamp(ts);
    Metric::from_parts(Context::from_static_name("x"), values, MetricMetadata::default())
}

/// The timestamps, relative to `B`, of every point the records carry.
fn timestamps(records: &[Metric]) -> BTreeSet<u64> {
    let mut timestamps = BTreeSet::new();
    for record in records {
        let points = match record.values() {
            MetricValues::Counter(points) | MetricValues::Gauge(points) | MetricValues::Rate(points, _) => {
                points.into_iter().map(|(ts, _)| ts).collect::<Vec<_>>()
            }
            MetricValues::Set(points) => points.members().map(|(ts, _)| ts).collect(),
            MetricValues::Distribution(points) => points.into_iter().map(|(ts, _)| ts).collect(),
            MetricValues::Histogram(_) => panic!("aggregation never emits histograms"),
        };
        timestamps.extend(
            points
                .into_iter()
                .map(|ts| ts.expect("flushed points are timestamped").get() - B),
        );
    }
    timestamps
}

#[test]
fn a_kind_or_interval_conflict_counts_the_points_it_replaces() {
    // Every pair of shapes: a context holding two points of the first shape receives a point of the second. Values of
    // another kind, or a rate over another interval, replace both held points, and only the new point is flushed;
    // otherwise all three are.
    let shapes = [
        "counter",
        "rate/10s",
        "rate/20s",
        "gauge",
        "set",
        "histogram",
        "distribution",
    ];
    for held in shapes {
        for incoming in shapes {
            let mut core = AggregateCore::new(params(10), AggregateTelemetry::noop());
            let outcome = core.admit(B + 101, [shaped(held, B + 100), shaped(held, B + 110)]);
            assert_eq!(
                (outcome.replaced, outcome.replaced_points),
                (0, 0),
                "{} into {}",
                held,
                held
            );

            let outcome = core.admit(B + 121, [shaped(incoming, B + 120)]);
            let conflict = held != incoming;
            let expected = if conflict { (1, 2) } else { (0, 0) };
            assert_eq!(
                (outcome.replaced, outcome.replaced_points),
                expected,
                "{} into {}",
                incoming,
                held
            );
            assert_eq!(outcome.accepted, 1);

            let mut records = Vec::new();
            core.flush_into(B + 200, false, |record| records.push(record));
            let expected = if conflict {
                BTreeSet::from([120])
            } else {
                BTreeSet::from([100, 110, 120])
            };
            assert_eq!(timestamps(&records), expected, "{} into {}", incoming, held);
        }
    }
}

#[test]
fn a_conflict_with_an_idle_counter_replaces_nothing() {
    // An idle counter holds no points once flushed, so a gauge for its context replaces nothing.
    let mut core = AggregateCore::new(
        AggregateParams {
            counter_expiry: NonZeroU64::new(300),
            ..params(10)
        },
        AggregateTelemetry::noop(),
    );
    core.admit(B + 1, [Metric::counter("x", 1.0)]);
    core.flush_into(B + 10, false, |_| {});
    assert_eq!(core.len(), 1, "the idle counter is kept");

    let outcome = core.admit(B + 11, [Metric::gauge("x", 1.0)]);
    assert_eq!((outcome.accepted, outcome.replaced, outcome.replaced_points), (1, 0, 0));
}

#[test]
fn cores_sharing_a_global_budget_hold_at_most_its_capacity_together() {
    let global = StateBudget::new(3);
    let mut a = shared_core(10, &global, &StateBudget::unlimited());
    let mut b = shared_core(10, &global, &StateBudget::unlimited());

    assert_eq!(a.admit(B + 1, gauges(&["a1", "a2"])).accepted, 2);
    let outcome = b.admit(B + 1, gauges(&["b1", "b2"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert!(outcome.limit_reached, "the global budget refused a new context");
    assert_eq!(global.used(), 3);
    assert_eq!((a.budget().node.used(), b.budget().node.used()), (2, 1));

    // A held context is always admitted, a new one is not.
    let outcome = b.admit(B + 12, gauges(&["b1", "b3"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));

    // `b`'s flush at 15 closes `b1`'s bucket at 0, but its point at 10 keeps it: the global budget stays full, and the
    // breach stays.
    let outcome = b.flush_into(B + 15, false, |_| {}).unwrap();
    assert!(!outcome.recovered);
    assert!(b.context_limit_breached());
    assert_eq!(global.used(), 3);

    // `a`'s flush removes both of its contexts and gives their units back, which lets `b`'s next flush recover.
    a.flush_into(B + 15, false, |_| {});
    assert!(a.is_empty());
    assert_eq!(global.used(), 1);
    let outcome = b.flush_into(B + 25, false, |_| {}).unwrap();
    assert!(outcome.recovered);
    assert_eq!(global.used(), 0);
    assert_eq!(b.admit(B + 26, gauges(&["b4", "b5", "b6"])).accepted, 3);
    assert_eq!(a.admit(B + 26, gauges(&["a3"])).dropped, 1);
}

#[test]
fn a_node_budget_counts_the_contexts_of_every_core_of_its_node() {
    // Two cores of one node: each batch's context limit caps the contexts of both together.
    let node = StateBudget::unlimited();
    let global = StateBudget::unlimited();
    let mut a = shared_core(3, &global, &node);
    let mut b = shared_core(3, &global, &node);

    assert_eq!(a.admit(B + 1, gauges(&["a1", "a2"])).accepted, 2);
    let outcome = b.admit(B + 1, gauges(&["b1", "b2"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert_eq!((node.used(), global.used()), (3, 3));

    // A batch with a higher limit admits more, into either core.
    let outcome = b.admit_with(B + 2, gauges(&["b2", "b3"]), &AdmitParams { context_limit: 4 });
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert_eq!(node.used(), 4);

    // A drain of one core releases its units, and the other core admits again.
    let mut drain = b.begin_drain(&params(3).split().2).unwrap();
    while b.flush_step(&mut drain, |_| {}) {}
    let outcome = b.end_flush(drain);
    assert!(outcome.recovered, "the drain leaves the node below the limit");
    assert_eq!(node.used(), 2);
    assert_eq!(a.admit(B + 3, gauges(&["a3"])).accepted, 1);
}

#[test]
fn a_shared_core_keeps_contexts_above_a_lowered_limit_within_its_budget_bounds() {
    // A later revision may lower a node's limit below the contexts it already holds. They stay, held contexts are still
    // admitted, and no bound the core asserts in debug builds, or for Antithesis, trips: those of a core that shares
    // budgets are its budgets', not its limit's.
    let global = StateBudget::new(10);
    let node = StateBudget::unlimited();
    let mut core = shared_core(5, &global, &node);
    assert_eq!(core.admit(B + 1, gauges(&["a", "b", "c", "d", "e"])).accepted, 5);

    let lowered = AdmitParams { context_limit: 2 };
    let outcome = core.admit_with(B + 2, gauges(&["a", "f"]), &lowered);
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert!(core.context_limit_breached());

    // A flush with the lowered limit that leaves every context in place keeps the breach, and checks the bounds with
    // five contexts against a limit of two.
    let mut flush_params = params(2).split().2;
    flush_params.context_limit = 2;
    let mut flush = core.begin_flush(B + 5, false, &flush_params).unwrap();
    while core.flush_step(&mut flush, |_| {}) {}
    assert!(!core.end_flush(flush).recovered);
    assert_eq!(core.len(), 5);
    assert!(global.used() <= global.capacity());
    assert!(core.len() as u64 <= node.used());

    // The flush that closes their bucket removes them all, and the breach clears below the lowered limit.
    let mut flush = core.begin_flush(B + 10, false, &flush_params).unwrap();
    while core.flush_step(&mut flush, |_| {}) {}
    assert!(core.end_flush(flush).recovered);
    assert_eq!((core.len(), node.used(), global.used()), (0, 0, 0));
}

#[test]
fn dropping_a_core_that_shares_budgets_releases_its_units_and_its_gauges() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let active = || recorder.gauge("aggregate_active_contexts");
    let global = StateBudget::new(100);
    let node = StateBudget::unlimited();
    let mut core = AggregateCore::with_budget(
        params(10),
        ContextBudget {
            global: global.clone(),
            node: node.clone(),
        },
        AggregateTelemetry::new(&MetricsBuilder::default()),
    );
    core.admit(B + 1, gauges(&["a", "b"]));
    core.admit(B + 12, gauges(&["c"]));
    assert_eq!(active(), Some(3.0));

    // An abandoned flush leaves `a` and `b` queued for removal, already out of the gauges, but still held.
    let mut flush = core.flush(B + 15, false).unwrap();
    while flush.emit_next(|_| {}) {}
    flush.abandon();
    assert_eq!(active(), Some(1.0));
    assert_eq!((core.len(), global.used(), node.used()), (3, 3, 3));

    // Dropping the core takes `c` out of the gauges, without counting `a` and `b` twice, and releases all three units.
    drop(core);
    assert_eq!(active(), Some(0.0));
    assert_eq!((global.used(), node.used()), (0, 0));

    // A native core leaves its gauges as they are when it is dropped, as native aggregation does when it stops.
    let mut native = AggregateCore::new(params(10), AggregateTelemetry::new(&MetricsBuilder::default()));
    native.admit(B + 1, gauges(&["a", "b"]));
    assert!(!native.shares_budget());
    drop(native);
    assert_eq!(active(), Some(2.0));
}

#[test]
fn a_drain_or_a_discard_removes_and_releases_each_context_as_it_visits_it() {
    // A drain or a discard spreads its removals over its steps, as a drain does its emissions: each step takes the
    // context it visits out of the table, the gauges, and both budgets. A host that drops the core part way through,
    // with the pass, releases the rest without counting any twice, and the counts a host takes as the pass goes and
    // the outcome of its end add up to what the whole pass did.
    let flush_params = params(10).split().2;
    for discard in [false, true] {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let active = || recorder.gauge("aggregate_active_contexts").unwrap_or(0.0);
        let (global, node) = (StateBudget::new(100), StateBudget::unlimited());
        let build = || {
            let budget = ContextBudget {
                global: global.clone(),
                node: node.clone(),
            };
            AggregateCore::with_budget(params(10), budget, AggregateTelemetry::new(&MetricsBuilder::default()))
        };
        let begin = |core: &mut AggregateCore| {
            if discard {
                core.begin_discard(&flush_params)
            } else {
                core.begin_drain(&flush_params)
            }
            .expect("the core holds contexts")
        };

        let mut core = build();
        core.admit(B + 1, gauges(&["a", "b", "c", "d"]));
        let mut pass = begin(&mut core);
        let mut emitted = 0;
        for visited in 1..=2 {
            assert!(core.flush_step(&mut pass, |_| emitted += 1));
            let held = 4 - visited;
            assert_eq!(
                (core.len(), global.used(), node.used()),
                (held, held as u64, held as u64)
            );
            assert_eq!(active(), held as f64);
        }
        assert_eq!(emitted, if discard { 0 } else { 2 });
        drop(pass);
        drop(core);
        assert_eq!((global.used(), node.used()), (0, 0));
        assert_eq!(active(), 0.0);

        // The whole pass, taking the counts after every other step.
        let mut core = build();
        core.admit(B + 1, gauges(&["a", "b", "c", "d", "e"]));
        let mut pass = begin(&mut core);
        let mut taken = Vec::new();
        let mut steps = 0;
        while core.flush_step(&mut pass, |_| {}) {
            steps += 1;
            if steps % 2 == 0 {
                taken.push(pass.take_counted());
            }
        }
        let outcome = core.end_flush(pass);
        assert!(core.is_empty());
        assert_eq!(global.used(), 0);
        let discarded = taken
            .iter()
            .chain([&outcome])
            .map(|outcome| outcome.discarded)
            .sum::<usize>();
        assert_eq!(discarded, if discard { 5 } else { 0 });
        assert!(taken.iter().all(|outcome| !outcome.recovered));
    }
}

/// One step of a run of two cores that share a global budget.
#[derive(Clone, Debug)]
enum SharedStep {
    /// A gauge for context `name` arrives at core `core`, with the batch's context limit.
    Admit { core: usize, name: u8, limit: usize },
    /// Core `core` flushes, `after` seconds later.
    Flush { core: usize, after: u64 },
    /// Core `core` drains.
    Drain { core: usize },
    /// Core `core` is dropped and replaced by an empty one, as a host rebuilds a kernel after a panic.
    Rebuild { core: usize },
}

fn arb_shared_steps() -> impl Strategy<Value = Vec<SharedStep>> {
    let step = prop_oneof![
        6 => (0usize..2, 0u8..8, 0usize..6).prop_map(|(core, name, limit)| SharedStep::Admit { core, name, limit }),
        2 => (0usize..2, 0u64..25).prop_map(|(core, after)| SharedStep::Flush { core, after }),
        1 => (0usize..2).prop_map(|core| SharedStep::Drain { core }),
        1 => (0usize..2).prop_map(|core| SharedStep::Rebuild { core }),
    ];
    vec(step, 1..60)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_shared_budget_matches_native_with_one_lineage(
        params in arb_params(),
        start in 0u64..3600,
        steps in arb_steps(),
        capacity in proptest::option::of(0u64..8),
    ) {
        // One core drawing on a global budget, and a node budget, of its own is native aggregation whose limit is the
        // smaller of the context limit and the global capacity.
        let global = capacity.map_or_else(StateBudget::unlimited, StateBudget::new);
        let node = StateBudget::unlimited();
        let effective = params.limited_to(capacity);
        let reference_recorder = TestRecorder::default();
        let kernel_recorder = TestRecorder::default();
        let mut expected = reference_run_loop(&effective, &reference_recorder);
        let mut actual = {
            let _local = metrics::set_default_local_recorder(&kernel_recorder);
            AggregateCore::with_budget(
                aggregate_params(&params),
                ContextBudget { global: global.clone(), node: node.clone() },
                AggregateTelemetry::new(&MetricsBuilder::default()),
            )
        };

        run_against_reference(&mut expected, &mut actual, start, &steps, |core| {
            prop_assert_eq!(global.used(), core.len() as u64);
            prop_assert_eq!(node.used(), core.len() as u64);
            Ok(())
        })?;
        prop_assert_eq!(telemetry_values(&reference_recorder), telemetry_values(&kernel_recorder));

        // Dropping the core gives back every unit, and takes its contexts out of its gauges.
        drop(actual);
        prop_assert_eq!((global.used(), node.used()), (0, 0));
        let active = kernel_recorder.gauge("aggregate_active_contexts");
        prop_assert!(active.is_none() || active == Some(0.0), "active contexts after the drop: {:?}", active);
    }

    #[test]
    fn property_test_cores_sharing_budgets_admit_by_the_budget_rule(
        capacity in 0u64..6,
        one_node in any::<bool>(),
        steps in arb_shared_steps(),
    ) {
        // Two cores share a global budget, and either one node budget or one each. A new context is admitted exactly
        // while its node's budget counts fewer contexts than the batch's limit and the global budget has room; the
        // budgets always count exactly the contexts held.
        let global = StateBudget::new(capacity);
        let shared_node = StateBudget::unlimited();
        let nodes = if one_node {
            [shared_node.clone(), shared_node]
        } else {
            [StateBudget::unlimited(), StateBudget::unlimited()]
        };
        let build = |core: usize| shared_core(0, &global, &nodes[core]);
        let mut cores = [build(0), build(1)];
        let mut now = [B, B];

        for step in &steps {
            match *step {
                SharedStep::Admit { core, name, limit } => {
                    let name = format!("c{}", name);
                    let held = cores[core]
                        .snapshot_contexts()
                        .iter()
                        .any(|entry| &**entry.context().name() == name.as_str());
                    let room = nodes[core].used() < limit as u64 && global.used() < global.capacity();
                    let metric = Metric::gauge(Context::from_parts(name, TagSet::default()), 1.0);
                    let params = AdmitParams { context_limit: limit };
                    let outcome = cores[core].admit_with(now[core] + 1, [metric], &params);
                    prop_assert_eq!(outcome.accepted == 1, held || room, "{:?}", step);
                }
                SharedStep::Flush { core, after } => {
                    now[core] += after;
                    cores[core].flush_into(now[core], false, |_| {});
                }
                SharedStep::Drain { core } => {
                    if let Some(mut drain) = cores[core].begin_drain(&params(0).split().2) {
                        while cores[core].flush_step(&mut drain, |_| {}) {}
                        cores[core].end_flush(drain);
                    }
                }
                SharedStep::Rebuild { core } => cores[core] = build(core),
            }

            let held = [cores[0].len() as u64, cores[1].len() as u64];
            prop_assert_eq!(global.used(), held[0] + held[1]);
            prop_assert!(global.used() <= capacity);
            if one_node {
                prop_assert_eq!(nodes[0].used(), held[0] + held[1]);
            } else {
                prop_assert_eq!((nodes[0].used(), nodes[1].used()), (held[0], held[1]));
            }
        }

        drop(cores);
        prop_assert_eq!(global.used(), 0);
        prop_assert_eq!((nodes[0].used(), nodes[1].used()), (0, 0));
    }
}

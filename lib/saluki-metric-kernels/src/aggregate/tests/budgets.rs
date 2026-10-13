//! Tests of budgets of contexts shared between cores, of kind conflicts, and of dropping a core.

use std::sync::Arc;

use saluki_core::execution::{BudgetOperation, StateBudget};

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

#[test]
fn a_new_context_takes_its_nodes_unit_first_and_gives_it_back_when_the_global_budget_is_full() {
    // A global budget of one context, and a node under a limit of five: the second new context takes a node unit,
    // finds the global budget full, and gives the node unit back. The node's unit is bounded by the limit in the same
    // compare-and-swap that takes it.
    let (global, global_ops) = StateBudget::recorded(1);
    let (node, node_ops) = StateBudget::recorded(u64::MAX);
    let mut core = shared_core(5, &global, &node);
    let outcome = core.admit(B + 1, gauges(&["a", "b"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert_eq!((global.used(), node.used()), (1, 1));

    let acquire = |bound, observed, granted| BudgetOperation::Acquire {
        units: 1,
        bound,
        observed,
        granted,
    };
    assert_eq!(
        node_ops.operations(),
        vec![
            acquire(5, 0, true),
            acquire(5, 1, true),
            BudgetOperation::Release { units: 1, observed: 2 },
        ]
    );
    assert_eq!(global_ops.operations(), vec![acquire(1, 0, true), acquire(1, 1, false)]);
}

/// Runs one lane of a node: a core sharing the node's count and the global budget, which admits `per_round` new gauges
/// each round under the context limit `limit` and then flushes them away, checking after every admission that the
/// node holds at most `limit` contexts and the global budget at most its capacity. Returns the first violation seen.
fn run_lane(
    lane: usize, global: &StateBudget, node: &StateBudget, limit: usize, rounds: usize, per_round: usize,
) -> Result<(), String> {
    let mut core = shared_core(limit, global, node);
    let params = AdmitParams { context_limit: limit };
    for round in 0..rounds {
        let at = B + 100 * round as u64;
        let metrics = (0..per_round)
            .map(|index| {
                let name = format!("lane{}.round{}.{}", lane, round, index);
                Metric::gauge(Context::from_parts(name, TagSet::default()), 1.0)
            })
            .collect::<Vec<_>>();
        for metric in metrics {
            core.admit_with(at, [metric], &params);
            let (held, used) = (node.used(), global.used());
            if held > limit as u64 {
                return Err(format!("the node holds {} contexts, over its limit of {}", held, limit));
            }
            if used > global.capacity() {
                return Err(format!("the global budget counts {}, over its capacity", used));
            }
        }
        // Flushing at the next bucket emits the gauges and removes them, releasing their units.
        core.flush_into(at + 50, false, |_| {});
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn property_test_bounded_node_acquire_never_exceeds_the_limit(
        lanes in 2usize..5,
        limit in 1usize..6,
        spare in 0u64..6,
        per_round in 1usize..4,
    ) {
        // Lanes on threads of their own admit new contexts of one node at once, under one context limit, while a lane
        // of another node shares the global budget, which leaves `spare` units beyond the limit. However the lanes'
        // admissions interleave, the node never holds more than its limit, nor the global budget more than its
        // capacity, and once every lane drops its core, nothing is held.
        const ROUNDS: usize = 300;
        let global = StateBudget::new(limit as u64 + spare);
        let node = StateBudget::unlimited();
        let other = StateBudget::unlimited();
        let start = std::sync::Barrier::new(lanes + 1);
        let outcomes = std::thread::scope(|scope| {
            let mut workers = (0..lanes)
                .map(|lane| {
                    let (global, node, start) = (&global, &node, &start);
                    scope.spawn(move || {
                        start.wait();
                        run_lane(lane, global, node, limit, ROUNDS, per_round)
                    })
                })
                .collect::<Vec<_>>();
            let (global, other, start) = (&global, &other, &start);
            workers.push(scope.spawn(move || {
                start.wait();
                run_lane(lanes, global, other, usize::MAX, ROUNDS, per_round)
            }));
            workers.into_iter().map(|worker| worker.join().expect("a lane panicked")).collect::<Vec<_>>()
        });
        for outcome in outcomes {
            prop_assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
        }
        prop_assert_eq!((global.used(), node.used(), other.used()), (0, 0, 0));
    }
}

#[test]
fn a_lanes_batch_takes_its_new_contexts_units_in_blocks_and_returns_the_ones_it_did_not_use() {
    // A lane of a state owner on two lanes. A batch of three new contexts and one held context takes blocks of one,
    // one, and two units of each budget, uses one unit of the last block, and returns the other when the batch ends.
    // The next batch reserves afresh, starting with a block as large as the previous batch's three new contexts: with
    // the node's limit lowered to five, two units fit, and its third new context finds the node full, and is dropped.
    let (global, global_ops) = StateBudget::recorded(100);
    let (node, node_ops) = StateBudget::recorded(u64::MAX);
    let mut core = lane_core(100, &global, &node, &Arc::new(LaneShared::new(2)));
    let outcome = core.admit(B + 1, gauges(&["a", "b", "a", "c"]));
    assert_eq!((outcome.accepted, outcome.dropped), (4, 0));
    assert_eq!((core.len(), global.used(), node.used()), (3, 3, 3));

    let lowered = AdmitParams { context_limit: 5 };
    let outcome = core.admit_with(B + 2, gauges(&["d", "a", "e", "f"]), &lowered);
    assert_eq!((outcome.accepted, outcome.dropped), (3, 1));
    assert!(outcome.limit_reached);
    assert_eq!((core.len(), global.used(), node.used()), (5, 5, 5));

    let acquire = |units, bound, observed, granted| BudgetOperation::Acquire {
        units,
        bound,
        observed,
        granted,
    };
    let returned = BudgetOperation::Release { units: 1, observed: 4 };
    assert_eq!(
        node_ops.operations(),
        vec![
            acquire(1, 100, 0, true),
            acquire(1, 100, 1, true),
            acquire(2, 100, 2, true),
            returned,
            acquire(2, 5, 3, true),
            acquire(1, 5, 5, false),
        ]
    );
    assert_eq!(
        global_ops.operations(),
        vec![
            acquire(1, 100, 0, true),
            acquire(1, 100, 1, true),
            acquire(2, 100, 2, true),
            returned,
            acquire(2, 100, 3, true),
        ]
    );
}

#[test]
fn a_batch_on_one_lane_takes_one_unit_of_each_budget_per_new_context() {
    // The same batch on a core without lanes, or on a lineage's only lane: each new context takes its units as it is
    // admitted, so no unit is ever reserved, and nothing is returned at the end of the batch.
    for lanes in [None, Some(Arc::new(LaneShared::new(1)))] {
        let (global, global_ops) = StateBudget::recorded(100);
        let (node, node_ops) = StateBudget::recorded(u64::MAX);
        let mut core = shared_core(100, &global, &node);
        if let Some(lanes) = &lanes {
            core = core.with_lanes(Arc::clone(lanes));
        }
        let outcome = core.admit(B + 1, gauges(&["a", "b", "a", "c"]));
        assert_eq!((outcome.accepted, outcome.dropped), (4, 0));
        let acquire = |bound, observed| BudgetOperation::Acquire {
            units: 1,
            bound,
            observed,
            granted: true,
        };
        let per_context = |bound| (0..3).map(|observed| acquire(bound, observed)).collect::<Vec<_>>();
        assert_eq!(node_ops.operations(), per_context(100), "{:?}", lanes);
        assert_eq!(global_ops.operations(), per_context(100), "{:?}", lanes);
    }
}

/// Runs one lane of a node as [`run_lane`] does, except that each round's new gauges, `sizes[round]` of them, and two
/// held ones arrive in one batch, so that the lane's admission reserves budget units in blocks if the lane is one of
/// several, `lanes`. After every batch it checks that the node holds at most `limit` contexts, the global budget at
/// most its capacity, and the node's budget at least the lane's contexts; it flushes every other round. Returns the
/// lane's core, still holding what it admitted since its last flush.
fn run_batched_lane(
    lane: usize, global: &StateBudget, node: &StateBudget, limit: usize, sizes: &[usize],
    lanes: Option<&Arc<LaneShared>>,
) -> Result<AggregateCore, String> {
    let mut core = match lanes {
        Some(lanes) => lane_core(limit, global, node, lanes),
        None => shared_core(limit, global, node),
    };
    let params = AdmitParams { context_limit: limit };
    let gauge = |name: String| Metric::gauge(Context::from_parts(name, TagSet::default()), 1.0);
    for (round, size) in sizes.iter().enumerate() {
        let at = B + 100 * (round as u64 / 2);
        let held = (0..2).map(|index| gauge(format!("lane{}.held.{}", lane, index)));
        let new = (0..*size).map(|index| gauge(format!("lane{}.round{}.{}", lane, round, index)));
        core.admit_with(at, held.chain(new).collect::<Vec<_>>(), &params);
        let (held, used) = (node.used(), global.used());
        if held > limit as u64 {
            return Err(format!("the node holds {} contexts, over its limit of {}", held, limit));
        }
        if used > global.capacity() {
            return Err(format!("the global budget counts {}, over its capacity", used));
        }
        if (core.len() as u64) > held {
            return Err(format!(
                "the lane holds {} contexts, but its node counts {}",
                core.len(),
                held
            ));
        }
        if round % 2 == 1 {
            // Flushing at the next bucket emits the gauges and removes them, releasing their units.
            core.flush_into(at + 50, false, |_| {});
        }
    }
    Ok(core)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn property_test_batches_reserving_units_never_exceed_the_limit_and_return_every_unit(
        lanes in 2usize..5,
        limit in 1usize..400,
        spare in 0u64..300,
        sizes in vec(vec(0usize..150, 41), 5),
    ) {
        // Lanes on threads of their own admit batches of new contexts of one node at once, each batch through a
        // reservation of budget units taken in blocks, under one context limit, while a state owner of another node,
        // on one lane, shares the global budget, which leaves `spare` units beyond the limit. However their batches
        // interleave, the node never holds more than its limit, nor the global budget more than its capacity. Once
        // every batch ended, every unit counted is a context held, the units reserved and not used having gone back;
        // and once every lane drops its core, nothing is held.
        let global = StateBudget::new(limit as u64 + spare);
        let node = StateBudget::unlimited();
        let other = StateBudget::unlimited();
        let shared = Arc::new(LaneShared::new(u16::try_from(lanes).unwrap()));
        let start = std::sync::Barrier::new(lanes + 1);
        let outcomes = std::thread::scope(|scope| {
            let mut workers = (0..lanes)
                .map(|lane| {
                    let (global, node, start, sizes, shared) = (&global, &node, &start, &sizes[lane], &shared);
                    scope.spawn(move || {
                        start.wait();
                        run_batched_lane(lane, global, node, limit, sizes, Some(shared))
                    })
                })
                .collect::<Vec<_>>();
            let (global, other, start, sizes) = (&global, &other, &start, &sizes[lanes]);
            workers.push(scope.spawn(move || {
                start.wait();
                run_batched_lane(lanes, global, other, usize::MAX, sizes, None)
            }));
            workers.into_iter().map(|worker| worker.join().expect("a lane panicked")).collect::<Vec<_>>()
        });
        let mut cores = Vec::new();
        for outcome in outcomes {
            prop_assert!(outcome.is_ok(), "{}", outcome.as_ref().err().unwrap());
            cores.push(outcome.unwrap());
        }
        let held_by_node = cores[..lanes].iter().map(|core| core.len() as u64).sum::<u64>();
        let held_by_other = cores[lanes].len() as u64;
        prop_assert_eq!(
            (node.used(), other.used(), global.used()),
            (held_by_node, held_by_other, held_by_node + held_by_other)
        );
        drop(cores);
        prop_assert_eq!((global.used(), node.used(), other.used()), (0, 0, 0));
    }
}

/// A batch that runs `between` once its core has admitted the first `before` of its metrics, before it takes the next
/// one: as another task's work runs while this batch's admission is under way.
struct Pausing<F> {
    metrics: std::vec::IntoIter<Metric>,
    before: usize,
    between: Option<F>,
}

impl<F: FnOnce()> Iterator for Pausing<F> {
    type Item = Metric;

    fn next(&mut self) -> Option<Metric> {
        if self.before == 0 {
            if let Some(between) = self.between.take() {
                between();
            }
        } else {
            self.before -= 1;
        }
        self.metrics.next()
    }
}

/// Returns `metrics` as a batch that runs `between` once its core has admitted the first `before` of them, or after the
/// last one, before the batch ends, if there are no more.
fn pausing<F: FnOnce()>(metrics: Vec<Metric>, before: usize, between: F) -> Pausing<F> {
    Pausing {
        metrics: metrics.into_iter(),
        before,
        between: Some(between),
    }
}

/// Returns a core of a state owner on one of several lanes, which shares `global` and its node's `node` with the other
/// lanes' cores, built against the lanes' shared record.
fn lane_core(context_limit: usize, global: &StateBudget, node: &StateBudget, lanes: &Arc<LaneShared>) -> AggregateCore {
    shared_core(context_limit, global, node).with_lanes(Arc::clone(lanes))
}

#[test]
fn state_owners_on_one_lane_are_never_refused_for_units_another_ones_batch_has_not_used() {
    // Two state owners of one host, each on one lane and so on a task of its own, share a global budget of four
    // contexts. While the first one's batch of three new contexts is under way, the second admits a new context: each
    // new context takes one unit of each budget as it is admitted, so the fourth unit is free, as it is in either
    // order of the two batches, and as stage 6 admits.
    let global = StateBudget::new(4);
    let mut x = shared_core(10, &global, &StateBudget::unlimited());
    let mut y = shared_core(10, &global, &StateBudget::unlimited());
    let mut during = None;
    let outcome = x.admit(
        B + 1,
        pausing(gauges(&["a", "b", "c"]), 3, || {
            during = Some((y.admit(B + 1, gauges(&["y"])), global.used()));
        }),
    );
    assert_eq!((outcome.accepted, outcome.dropped), (3, 0));
    let (during, used) = during.expect("the batch pauses after its third context");
    assert_eq!(
        (during.accepted, during.dropped),
        (1, 0),
        "the global budget has a unit free"
    );
    assert_eq!((used, global.used()), (4, 4));
}

#[test]
fn a_lane_can_be_refused_for_units_another_lanes_batch_reserved_until_that_batch_ends() {
    // Two lanes of one state owner share its node's count and a global budget of four contexts. One lane's batch takes
    // its new contexts' units in blocks of one, one, and two units: while its third context's block holds a unit no
    // context uses, the other lane's new context finds the global budget full and is dropped, which is capacity
    // behavior. The end of the batch returns the unit, and the other lane's next batch admits the context.
    let lanes = Arc::new(LaneShared::new(2));
    let (global, node) = (StateBudget::new(4), StateBudget::unlimited());
    let mut x = lane_core(10, &global, &node, &lanes);
    let mut y = lane_core(10, &global, &node, &lanes);
    let mut during = None;
    let outcome = x.admit(
        B + 1,
        pausing(gauges(&["a", "b", "c"]), 3, || {
            during = Some((y.admit(B + 1, gauges(&["y"])), global.used()));
        }),
    );
    assert_eq!((outcome.accepted, outcome.dropped), (3, 0));
    let (during, used) = during.expect("the batch pauses after its third context");
    assert_eq!(
        (during.accepted, during.dropped),
        (0, 1),
        "the fourth unit sits in the first lane's batch"
    );
    assert!(during.limit_reached);
    assert_eq!(used, 4, "three held contexts and one reserved unit");
    assert_eq!(
        (global.used(), node.used()),
        (3, 3),
        "the batch's end returned the unit"
    );

    let outcome = y.admit(B + 2, gauges(&["y"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 0));
    assert_eq!(global.used(), 4);
}

#[test]
fn a_flush_during_another_lanes_batch_keeps_the_breach_until_a_pass_after_that_batch() {
    // Two lanes of one node under a context limit of five. Lane 0 is breached, and its flush ends while lane 1's batch
    // holds a reserved unit that no context uses: the flush counts it, finds the node full, and keeps the breach,
    // where one acquisition per new context would have found four of five contexts held and cleared it. A drop before
    // lane 0's next pass is then part of the same episode, which warned once already. Passes never clear the flag
    // while held contexts fill the node, and the first pass that ends with room, once lane 1's batch has ended,
    // clears it.
    let lanes = Arc::new(LaneShared::new(2));
    let (global, node) = (StateBudget::unlimited(), StateBudget::unlimited());
    let mut lane0 = lane_core(5, &global, &node, &lanes);
    let mut lane1 = lane_core(5, &global, &node, &lanes);
    assert_eq!(lane1.admit(B + 1, gauges(&["x1", "x2", "x3", "x4"])).accepted, 4);
    assert_eq!(lane0.admit(B + 11, gauges(&["a"])).accepted, 1);
    let outcome = lane0.admit(B + 11, gauges(&["b"]));
    assert_eq!(outcome.dropped, 1);
    assert!(outcome.limit_reached, "the episode starts, and warns");

    // Lane 1's flush removes its four contexts, whose bucket closed: lane 0's `a` is the node's only context.
    lane1.flush_into(B + 15, false, |_| {});
    assert_eq!(node.used(), 1);

    // Lane 1's batch of three new contexts holds one more unit than it uses when lane 0's flush ends.
    let mut flushed = None;
    let outcome = lane1.admit(
        B + 16,
        pausing(gauges(&["p", "q", "r"]), 3, || {
            let outcome = lane0.flush_into(B + 16, false, |_| {}).expect("lane 0 holds `a`");
            flushed = Some((outcome.recovered, node.used(), lane0.context_limit_breached()));
        }),
    );
    assert_eq!(outcome.accepted, 3);
    assert_eq!(
        flushed,
        Some((false, 5, true)),
        "four contexts held and one unit reserved: the breach stays"
    );
    assert_eq!(node.used(), 4, "the batch's end returned the unit");

    // The episode goes on: a drop does not start another one.
    let outcome = lane0.admit(B + 17, gauges(&["c", "d"]));
    assert_eq!((outcome.accepted, outcome.dropped), (1, 1));
    assert!(
        !outcome.limit_reached,
        "the breach was never cleared, so no second warning"
    );

    // Five contexts held fill the node: a pass keeps the breach.
    assert!(!lane0.flush_into(B + 17, false, |_| {}).unwrap().recovered);
    // Once lane 1's flush removed its contexts, lane 0's next pass ends with room, and clears it.
    lane1.flush_into(B + 20, false, |_| {});
    assert_eq!(node.used(), 2);
    assert!(lane0.flush_into(B + 19, false, |_| {}).unwrap().recovered);
    assert!(!lane0.context_limit_breached());
}

/// One round of two cores that share budgets: core 0 admits `first`, and core 1 admits `second` once core 0 has
/// admitted the first `pause` metrics of `first`, while core 0's batch is under way; both batches carry gauges for the
/// contexts named, under their own context limits. Then, if `flush`, both cores flush everything they hold away.
#[derive(Clone, Debug)]
struct InterleavedRound {
    first: Vec<u8>,
    second: Vec<u8>,
    pause: usize,
    limits: [usize; 2],
    flush: bool,
}

fn arb_interleaved_rounds() -> impl Strategy<Value = Vec<InterleavedRound>> {
    let names = || vec(0u8..12, 0..10);
    let round = (names(), names(), 0usize..10, [0usize..10, 0usize..10], any::<bool>()).prop_map(
        |(first, second, pause, limits, flush)| InterleavedRound {
            first,
            second,
            pause,
            limits,
            flush,
        },
    );
    vec(round, 1..10)
}

/// Returns a batch of gauges for the contexts named.
fn named_gauges(names: &[u8]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| Metric::gauge(Context::from_parts(format!("c{}", name), TagSet::default()), 1.0))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_state_owners_on_one_lane_admit_by_the_budget_rule_however_their_batches_interleave(
        capacity in 0u64..12,
        one_node in any::<bool>(),
        rounds in arb_interleaved_rounds(),
    ) {
        // Two cores, each on one lane, share a global budget, and either one node budget or one each; one core's batch
        // runs in the middle of the other's. Every metric is admitted exactly as when the batches' metrics run one at
        // a time in that order: while its context is held, or while its node counts fewer contexts than its batch's
        // limit and the global budget has room. A unit sits in no batch, so no batch is refused for one.
        let global = StateBudget::new(capacity);
        let shared_node = StateBudget::unlimited();
        let nodes = if one_node {
            [shared_node.clone(), shared_node]
        } else {
            [StateBudget::unlimited(), StateBudget::unlimited()]
        };
        let [mut core0, mut core1] = [0, 1].map(|core| shared_core(0, &global, &nodes[core]));
        let mut held: [BTreeSet<u8>; 2] = Default::default();

        for (index, round) in rounds.iter().enumerate() {
            let at = B + 100 * index as u64;
            // The order the metrics run in: the first `pause` of core 0's, core 1's, then the rest of core 0's.
            let pause = round.pause.min(round.first.len());
            let order = round.first[..pause]
                .iter()
                .map(|name| (0, *name))
                .chain(round.second.iter().map(|name| (1, *name)))
                .chain(round.first[pause..].iter().map(|name| (0, *name)));
            let mut expected = [(0, 0); 2];
            for (core, name) in order {
                let node_count = if one_node { held[0].len() + held[1].len() } else { held[core].len() };
                let room = node_count < round.limits[core] && ((held[0].len() + held[1].len()) as u64) < capacity;
                if held[core].contains(&name) || room {
                    held[core].insert(name);
                    expected[core].0 += 1;
                } else {
                    expected[core].1 += 1;
                }
            }

            let mut second = None;
            let first = core0.admit_with(
                at,
                pausing(named_gauges(&round.first), pause, || {
                    let params = AdmitParams { context_limit: round.limits[1] };
                    second = Some(core1.admit_with(at, named_gauges(&round.second), &params));
                }),
                &AdmitParams { context_limit: round.limits[0] },
            );
            let second = second.expect("core 1's batch ran during core 0's");
            prop_assert_eq!(
                [(first.accepted, first.dropped), (second.accepted, second.dropped)],
                expected,
                "{:?}",
                round
            );
            prop_assert_eq!(global.used(), (held[0].len() + held[1].len()) as u64);

            if round.flush {
                // The gauges' bucket closes: the flush removes every context, and releases its units.
                core0.flush_into(at + 50, false, |_| {});
                core1.flush_into(at + 50, false, |_| {});
                held = Default::default();
                prop_assert_eq!(global.used(), 0);
            }
        }
    }
}

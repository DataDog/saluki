use proptest::prelude::*;
use serde_json::{json, Value};

use super::*;
use crate::{
    compile::{
        rewrite::{
            harness::{
                arb_plan_params, arb_shape, arb_trace, drop, edges, emit, ids, node, plan, registry, sample, sorted,
                source, split, with_scope, Case, TEST_KEEP, TEST_SHED, TEST_SIGNAL_MAP,
            },
            run_pipeline, Optimized, RewriteOutcome, RewriteRecord,
        },
        Frame,
    },
    ir::{InputName, ScopeDecl, ScopeName},
    operators::{names, OperatorRegistry},
    validate::Validator,
};

fn prefix(id: &str) -> Value {
    node(
        id,
        names::NAMESPACE_PREFIX,
        json!({"namespace": "ns", "exclusions": []}),
        true,
    )
}

fn tag_projection(id: &str) -> Value {
    node(
        id,
        names::TAG_PROJECTION,
        json!({"rules": [{"metric_name": "app.requests", "action": "exclude", "tags": ["env"]}], "value_rules": [],
               "cache_capacity": 0}),
        true,
    )
}

/// Runs R5a alone to a fixed point over a valid plan, framed by itself.
fn r5a(plan: Plan) -> (Case, Optimized) {
    let case = Case::of(plan, Vec::new()).unwrap();
    let optimized = case.optimize(&[&R5a]);
    assert!(optimized.converged);
    (case, optimized)
}

fn applied(optimized: &Optimized) -> Vec<&RewriteRecord> {
    optimized.records.iter().filter(|record| record.is_applied()).collect()
}

fn effects(record: &RewriteRecord) -> Vec<(String, TelemetryChange)> {
    record
        .telemetry
        .iter()
        .map(|effect| (effect.node.to_string(), effect.change))
        .collect()
}

#[test]
fn a_filter_moves_whole_above_a_kernel_that_writes_nothing_it_reads() {
    // The prefix writes `name`; the filter reads `host`.
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            prefix("p"),
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> p.in",
            "p.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/f.in",
            "policy/f.true -> p.in",
            "policy/f.false -> policy/f_drop.in",
            "p.out -> policy/e.in",
        ])
    );
    assert_eq!(
        ids(&optimized.plan),
        vec!["src", "policy/f", "policy/f_drop", "p", "policy/e"]
    );

    let records = applied(&optimized);
    assert_eq!(records.len(), 1);
    let record = records[0];
    assert_eq!(record.id, RewriteId::R5a);
    assert_eq!(record.nodes, vec![id("p"), id("policy/f"), id("policy/f_drop")]);
    assert_eq!(effects(record), vec![("p".to_string(), TelemetryChange::Changed)]);
    assert!(!record.capacity_rule);
    assert!(
        record.justification.contains("reads {host}"),
        "{}",
        record.justification
    );
    assert!(
        record.justification.contains("writes {name}"),
        "{}",
        record.justification
    );
    assert!(
        record.justification.contains("moved `policy/f` whole above `p`"),
        "{}",
        record.justification
    );
    case.check_structure(&optimized).unwrap();
}

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

#[test]
fn a_filter_stays_below_what_it_cannot_cross() {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    // (case, the operator above the filter, the filter's predicate)
    let cases: Vec<(&str, Value, Value)> = vec![
        (
            "a kernel that writes the field the filter reads",
            prefix("o"),
            json!({"intake_name": {"prefix": "app."}}),
        ),
        (
            "an opaque host, a fence",
            node(
                "o",
                names::OPAQUE_HOST,
                json!({"component": "o", "input": any, "outputs": {"out": any}}),
                true,
            ),
            json!({"host": {"eq": "web-1"}}),
        ),
        (
            "a kernel with two output ports",
            node("o", names::TIMESTAMP_SPLIT, json!({"window": "10s"}), true),
            json!({"host": {"eq": "web-1"}}),
        ),
        (
            "a tee, whose other port the filter does not apply to",
            node("o", names::TEE, json!({"ports": 2}), false),
            json!({"host": {"eq": "web-1"}}),
        ),
        (
            "another filter",
            node("o", TEST_KEEP, json!({"predicate": {"has_tag": "env"}}), false),
            json!({"host": {"eq": "web-1"}}),
        ),
    ];
    for (what, above, predicate) in cases {
        let above_id = above["id"].as_str().unwrap().to_string();
        let op = above["op"].as_str().unwrap().to_string();
        let second = match op.as_str() {
            names::TEE => "1",
            names::TIMESTAMP_SPLIT => "passthrough",
            _ => "",
        };
        let first = match op.as_str() {
            names::TEE => "0",
            names::TIMESTAMP_SPLIT => "default",
            _ => "out",
        };
        let mut nodes = vec![
            source("src", "otlp"),
            above,
            split("f", predicate, false),
            drop("f_drop", false),
            emit("e", "o0"),
        ];
        let mut edge_list = vec![
            format!("src.metrics -> {}.in", above_id),
            format!("{}.{} -> policy/f.in", above_id, first),
            "policy/f.false -> policy/f_drop.in".to_string(),
            "policy/f.true -> policy/e.in".to_string(),
        ];
        let mut outputs = vec!["o0"];
        if !second.is_empty() {
            nodes.push(emit("e1", "o1"));
            edge_list.push(format!("{}.{} -> policy/e1.in", above_id, second));
            outputs.push("o1");
        }
        let edge_refs = edge_list.iter().map(String::as_str).collect::<Vec<_>>();
        let (case, optimized) = r5a(plan(nodes, &edge_refs, &outputs));
        assert!(optimized.records.is_empty(), "{}: {:?}", what, optimized.records);
        assert_eq!(optimized.plan, case.plan, "{}", what);
    }
}

#[test]
fn a_filter_is_pushed_only_for_the_kinds_it_commutes_for_and_the_residual_stays() {
    // Tag projection writes tags only for counters and distributions; the filter, which keeps metrics tagged `env`,
    // commutes with it for OTLP's rates and gauges. A copy restricted to those kinds goes above, and the filter stays
    // below for the rest.
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            tag_projection("t"),
            split("f", json!({"has_tag": "env"}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> t.in",
            "t.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/f_r5a0.in",
            "policy/f_r5a0.true -> t.in",
            "policy/f_r5a0.false -> policy/f_drop_r5a0.in",
            "t.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ])
    );
    // The copy keeps what is not a rate or a gauge, or is tagged `env`, by its `true` port.
    let copy = optimized.plan.node(&id("policy/f_r5a0")).unwrap();
    assert_eq!(
        copy.params.get("predicate").unwrap(),
        &json!({"any": [{"kind_in": ["counter", "set", "histogram", "distribution"]}, {"has_tag": "env"}]})
    );

    // One application: the copy above is recognized, so the fixed point inserts no other.
    let records = applied(&optimized);
    assert_eq!(records.len(), 1, "{:?}", optimized.records);
    let record = records[0];
    assert_eq!(
        record.nodes,
        vec![
            id("t"),
            id("policy/f"),
            id("policy/f_drop"),
            id("policy/f_r5a0"),
            id("policy/f_drop_r5a0")
        ]
    );
    assert_eq!(
        effects(record),
        vec![
            ("policy/f".to_string(), TelemetryChange::Changed),
            ("policy/f_drop".to_string(), TelemetryChange::Changed),
            ("t".to_string(), TelemetryChange::Changed),
            ("policy/f_r5a0".to_string(), TelemetryChange::Added),
            ("policy/f_drop_r5a0".to_string(), TelemetryChange::Added),
        ]
    );
    assert!(
        record
            .justification
            .contains("for {counter, distribution} it writes a field the filter reads"),
        "{}",
        record.justification
    );
    case.check_structure(&optimized).unwrap();

    // Optimizing the rewritten plan again changes nothing.
    let again = Case::of(optimized.plan.clone(), Vec::new()).unwrap();
    assert!(again.optimize(&[&R5a]).records.is_empty());
}

#[test]
fn a_partial_push_down_keeps_the_order_in_which_an_output_receives_records() {
    // Two paths join in a merge: the `true` one through tag projection and a filter on tags, the `false` one through
    // two kernels. The partial push-down makes the `true` path one node longer. An output bounded to one record must
    // accept the same record from both plans: the `true` path's, which the interpreter delivers first because its
    // order follows ports where paths diverge, not their lengths.
    let enrich = |id: &str| node(id, names::HOST_ENRICHMENT, json!({}), true);
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"host": {"eq": "web-1"}}), false),
            tag_projection("t"),
            split("f", json!({"has_tag": "env"}), false),
            drop("f_drop", false),
            enrich("k1"),
            enrich("k2"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> policy/s.in",
            "policy/s.true -> t.in",
            "t.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/m.0",
            "policy/s.false -> k1.in",
            "k1.out -> k2.in",
            "k2.out -> policy/m.1",
            "policy/m.out -> policy/e.in",
        ],
        &["o0"],
    ));
    assert!(
        ids(&optimized.plan).contains(&"policy/f_r5a0".to_string()),
        "{:?}",
        optimized.records
    );

    let metric = |name: &'static str, host: &'static str| {
        let context = saluki_core::data_model::event::metric::context::Context::from_static_parts(name, &["env:prod"])
            .with_host(stringtheory::MetaString::from_static(host));
        saluki_core::data_model::event::metric::Metric::gauge(context, 1.0)
    };
    let output = crate::ir::OutputId::new("o0").unwrap();
    let events = vec![
        crate::interpret::TraceEvent::Capacity {
            output: output.clone(),
            limit: 1,
        },
        crate::interpret::TraceEvent::Admit {
            source: id("src"),
            lane: 0,
            admitted_at: saluki_core::execution::AdmissionTime::from_unix_secs(1_000),
            batch: vec![metric("other", "db"), metric("first", "web-1")],
        },
    ];
    let accepted = |plan: &Plan| {
        let run = case.run(plan, &events).unwrap();
        run.output(&output)
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(accepted(&case.plan), vec!["first"]);
    assert_eq!(accepted(&optimized.plan), vec!["first"]);
}

#[test]
fn a_filter_that_drops_its_true_port_is_restricted_with_all() {
    let (_, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            tag_projection("t"),
            split("f", json!({"has_tag": "env"}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> t.in",
            "t.out -> policy/f.in",
            "policy/f.true -> policy/f_drop.in",
            "policy/f.false -> policy/e.in",
        ],
        &["o0"],
    ));
    let copy = optimized.plan.node(&id("policy/f_r5a0")).unwrap();
    assert_eq!(
        copy.params.get("predicate").unwrap(),
        &json!({"all": [{"kind_in": ["rate", "gauge"]}, {"has_tag": "env"}]})
    );
    assert!(edges(&optimized.plan).contains(&"policy/f_r5a0.true -> policy/f_drop_r5a0.in".to_string()));
}

#[test]
fn a_filter_below_a_merge_is_copied_into_every_input() {
    let names_binding = json!({"intake_name": {"matches": {
        "values": {"config": "test.names", "resolved": ["app.requests"]}, "prefix": false}}});
    let (case, optimized) = r5a(plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            split("f", names_binding.clone(), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/m.1",
            "policy/m.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "dsd.metrics -> policy/f_r5a0.in",
            "policy/f_r5a0.true -> policy/m.0",
            "policy/f_r5a0.false -> policy/f_drop_r5a0.in",
            "otlp.metrics -> policy/f_r5a1.in",
            "policy/f_r5a1.true -> policy/m.1",
            "policy/f_r5a1.false -> policy/f_drop_r5a1.in",
            "policy/m.out -> policy/e.in",
        ])
    );
    assert_eq!(
        ids(&optimized.plan),
        vec![
            "dsd",
            "otlp",
            "policy/f_r5a0",
            "policy/f_drop_r5a0",
            "policy/f_r5a1",
            "policy/f_drop_r5a1",
            "policy/m",
            "policy/e"
        ]
    );
    // The copies keep the configuration binding, which the catalog still validates.
    for copy in ["policy/f_r5a0", "policy/f_r5a1"] {
        let node = optimized.plan.node(&id(copy)).unwrap();
        assert_eq!(node.params.get("predicate").unwrap(), &names_binding);
    }
    let record = applied(&optimized)[0];
    assert_eq!(
        effects(record),
        vec![
            ("policy/f".to_string(), TelemetryChange::Removed),
            ("policy/f_drop".to_string(), TelemetryChange::Removed),
            ("policy/f_r5a0".to_string(), TelemetryChange::Added),
            ("policy/f_drop_r5a0".to_string(), TelemetryChange::Added),
            ("policy/f_r5a1".to_string(), TelemetryChange::Added),
            ("policy/f_drop_r5a1".to_string(), TelemetryChange::Added),
        ]
    );
    assert!(record
        .justification
        .contains("copied `policy/f` into each of the 2 inputs of `policy/m`"));
    case.check_structure(&optimized).unwrap();
}

#[test]
fn compiling_a_plan_records_r5a_for_explain() {
    let case = Case::of(
        plan(
            vec![
                source("src", "otlp"),
                prefix("p"),
                split("f", json!({"host": {"eq": "web-1"}}), false),
                drop("f_drop", false),
                emit("e", "o0"),
            ],
            &[
                "src.metrics -> p.in",
                "p.out -> policy/f.in",
                "policy/f.false -> policy/f_drop.in",
                "policy/f.true -> policy/e.in",
            ],
            &["o0"],
        ),
        Vec::new(),
    )
    .unwrap();
    let compiled = crate::compile::compile(&case.plan, &case.report, registry(), &case.frame).unwrap();
    assert!(edges(&compiled.plan).contains(&"src.metrics -> policy/f.in".to_string()));
    let report = Validator::new(registry(), &crate::compile::rewrite::harness::catalog()).validate(&compiled.plan);
    let text = crate::explain::Explain::of(&compiled.plan, registry(), &report)
        .with_rewrites(&compiled.rewrites)
        .to_text();
    let section = &text[text.find("rewrites (1)").expect("a rewrites section")..];
    let lines = section.lines().take(4).collect::<Vec<_>>();
    assert_eq!(lines[1], "  R5a applied: p, policy/f, policy/f_drop");
    assert!(
        lines[2].starts_with("    justification reads and writes: `policy/f` (a `split` whose `false` port leads to "),
        "{}",
        lines[2]
    );
    assert!(lines[3].starts_with("    telemetry changed p: "), "{}", lines[3]);
}

#[test]
fn native_filters_move_only_whole() {
    // Below a merge, a native filter would have to be copied; below tag projection, partly pushed.
    let merged = plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), true),
            split("f", json!({"host": {"eq": "web-1"}}), true),
            drop("f_drop", true),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> m.0",
            "otlp.metrics -> m.1",
            "m.out -> f.in",
            "f.false -> f_drop.in",
            "f.true -> policy/e.in",
        ],
        &["o0"],
    );
    let (_, optimized) = r5a(merged);
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);

    let projected = plan(
        vec![
            source("src", "otlp"),
            tag_projection("t"),
            split("f", json!({"has_tag": "env"}), true),
            drop("f_drop", true),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> t.in",
            "t.out -> f.in",
            "f.false -> f_drop.in",
            "f.true -> policy/e.in",
        ],
        &["o0"],
    );
    let (_, optimized) = r5a(projected);
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);

    // Moving whole keeps a native filter's ID and counts, so it applies.
    let moved = plan(
        vec![
            source("src", "otlp"),
            prefix("p"),
            split("f", json!({"host": {"eq": "web-1"}}), true),
            drop("f_drop", true),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> p.in",
            "p.out -> f.in",
            "f.false -> f_drop.in",
            "f.true -> policy/e.in",
        ],
        &["o0"],
    );
    let (case, optimized) = r5a(moved);
    assert_eq!(applied(&optimized).len(), 1);
    assert!(edges(&optimized.plan).contains(&"src.metrics -> f.in".to_string()));
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_filter_operator_moves_for_every_kind_or_not_at_all() {
    let keep = |predicate: Value| node("k", TEST_KEEP, json!({"predicate": predicate}), false);
    let shaped = |above: Value, predicate: Value| {
        plan(
            vec![source("src", "otlp"), above, keep(predicate), emit("e", "o0")],
            &[
                "src.metrics -> o.in",
                "o.out -> policy/k.in",
                "policy/k.out -> policy/e.in",
            ],
            &["o0"],
        )
    };
    // Across the prefix, for every kind: moved whole.
    let (case, optimized) = r5a(shaped(prefix("o"), json!({"has_tag": "env"})));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/k.in",
            "policy/k.out -> o.in",
            "o.out -> policy/e.in"
        ])
    );
    case.check_structure(&optimized).unwrap();

    // Across tag projection, only for some kinds: a filter operator cannot be restricted to kinds.
    let (_, optimized) = r5a(shaped(tag_projection("o"), json!({"has_tag": "env"})));
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);
}

#[test]
fn nothing_moves_across_or_out_of_a_dynamic_operator_or_a_scope_boundary() {
    let filtered = |above: Value| {
        plan(
            vec![
                source("src", "otlp"),
                above,
                split("f", json!({"host": {"eq": "web-1"}}), false),
                drop("f_drop", false),
                emit("e", "o0"),
            ],
            &[
                "src.metrics -> o.in",
                "o.out -> policy/f.in",
                "policy/f.false -> policy/f_drop.in",
                "policy/f.true -> policy/e.in",
            ],
            &["o0"],
        )
    };
    let prefixed = prefix("o");

    // A signal reader cannot be placed until stage 10, so the frame comes from the same plan with a prefix, and only
    // R5a's own obligations decide.
    let dynamic = filtered(node("o", TEST_SIGNAL_MAP, json!({}), true));
    let report = Validator::new(registry(), &crate::compile::rewrite::harness::catalog()).validate(&dynamic);
    assert!(report.is_valid(), "{}", report);
    let boot = filtered(prefixed.clone());
    let frame = Frame::compute(&boot, registry(), &crate::compile::rewrite::harness::namer).unwrap();
    let ctx = RewriteContext::new(registry(), &frame, &report);
    assert!(R5a.apply(&ctx, &dynamic).is_none());

    // A filter at a scope's input stays in the scope; one below the first node of a scope moves above it, staying
    // first in the scope.
    let scoped = |input: &str| {
        let mut plan = filtered(prefixed.clone());
        plan.scopes.insert(
            ScopeName::new("post_enrich").unwrap(),
            ScopeDecl {
                inputs: [(InputName::new("metrics").unwrap(), input.parse().unwrap())].into(),
                exits: Default::default(),
            },
        );
        plan
    };
    let (_, optimized) = r5a(scoped("o.out"));
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);
    let (case, optimized) = r5a(scoped("src.metrics"));
    assert_eq!(applied(&optimized).len(), 1);
    assert!(edges(&optimized.plan).contains(&"src.metrics -> policy/f.in".to_string()));
    case.check_structure(&optimized).unwrap();
}

/// `src -> p -> f -> e`, where the prefix `p` writes `name` and the filter `f` drops what is not on host `web-1`.
fn prefixed_filter() -> Plan {
    plan(
        vec![
            source("src", "otlp"),
            prefix("p"),
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> p.in",
            "p.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    )
}

/// `dsd, otlp -> m -> f -> e`, where the filter `f` drops what is not on host `web-1`.
fn merged_filter() -> Plan {
    plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/m.1",
            "policy/m.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    )
}

#[test]
fn a_filter_whose_own_ports_bound_a_scope_stays_where_it_is() {
    // (case, the plan with its scope)
    let cases = [
        (
            "the scope begins at the filter's kept port: moving it would put the prefix into the scope",
            with_scope(prefixed_filter(), "policy/f.true", &[]),
        ),
        (
            "the scope ends at the filter's drop",
            with_scope(prefixed_filter(), "src.metrics", &[("dropped", "policy/f_drop.in")]),
        ),
        (
            "the scope begins at the kept port of a filter that copies across the merge would remove",
            with_scope(merged_filter(), "policy/f.true", &[]),
        ),
    ];
    for (what, scoped) in cases {
        let (case, optimized) = r5a(scoped);
        assert!(optimized.records.is_empty(), "{}: {:?}", what, optimized.records);
        assert_eq!(optimized.plan, case.plan, "{}", what);
    }

    // A scope that ends below the filter, at what its kept records enter, keeps it in the scope as it moves.
    let (case, optimized) = r5a(with_scope(
        prefixed_filter(),
        "src.metrics",
        &[("enriched", "policy/e.in")],
    ));
    assert_eq!(applied(&optimized).len(), 1, "{:?}", optimized.records);
    assert!(edges(&optimized.plan).contains(&"p.out -> policy/e.in".to_string()));
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_partial_push_down_inserts_one_copy_when_the_kinds_decide_the_predicate() {
    // The filter keeps gauges and counters tagged `env`. Tag projection writes the tags of counters and distributions,
    // so the copy goes above for DogStatsD's other kinds, and keeps no counter or distribution, nor a rate, which
    // DogStatsD has none of. Its kept port then carries gauges, counters, and distributions only, so a second
    // application would restrict a copy to gauges: the first copy covers it, and none is inserted.
    let (case, optimized) = r5a(plan(
        vec![
            node(
                "src",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            tag_projection("t"),
            split(
                "f",
                json!({"all": [{"kind_in": ["gauge", "counter"]}, {"has_tag": "env"}]}),
                false,
            ),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> t.in",
            "t.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    assert_eq!(applied(&optimized).len(), 1, "{:?}", optimized.records);
    assert_eq!(
        ids(&optimized.plan),
        vec![
            "src",
            "policy/f_r5a0",
            "policy/f_drop_r5a0",
            "t",
            "policy/f",
            "policy/f_drop",
            "policy/e"
        ]
    );
    let copy = optimized.plan.node(&id("policy/f_r5a0")).unwrap();
    assert_eq!(
        copy.params.get("predicate").unwrap(),
        &json!({"any": [{"kind_in": ["counter", "rate", "distribution"]},
                        {"all": [{"kind_in": ["gauge", "counter"]}, {"has_tag": "env"}]}]})
    );
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_filter_below_a_merge_is_copied_only_onto_inputs_it_can_remove_records_from() {
    // The filter drops rates, which DogStatsD never produces: only OTLP's input gets a copy.
    let (case, optimized) = r5a(plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            split("f", json!({"kind_in": ["rate"]}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/m.1",
            "policy/m.out -> policy/f.in",
            "policy/f.true -> policy/f_drop.in",
            "policy/f.false -> policy/e.in",
        ],
        &["o0"],
    ));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/f_r5a0.in",
            "policy/f_r5a0.false -> policy/m.1",
            "policy/f_r5a0.true -> policy/f_drop_r5a0.in",
            "policy/m.out -> policy/e.in",
        ])
    );
    let record = applied(&optimized)[0];
    assert!(
        record
            .justification
            .contains("copied `policy/f` into the inputs `1` of the 2 inputs of `policy/m`"),
        "{}",
        record.justification
    );
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_filter_crossing_a_one_port_tee_changes_the_tees_counts() {
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            node("t", names::TEE, json!({"ports": 1}), false),
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> policy/t.in",
            "policy/t.0 -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    let record = applied(&optimized)[0];
    assert_eq!(
        effects(record),
        vec![("policy/t".to_string(), TelemetryChange::Changed)]
    );
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_filter_never_moves_a_kernel_into_another_region() {
    // The prefix is the costliest native component, which makes R1's budget; with the filter on intake names after
    // it, the region would cost more, so R1 cuts between the host enrichment and the filter. Above the enrichment,
    // the filter would stay in its region, fed by the prefix's, and the enrichment would follow it there: it would be
    // built in another region's resource group. The pipeline skips that.
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            prefix("p"),
            node("h", names::HOST_ENRICHMENT, json!({}), true),
            split("f", json!({"intake_name": {"prefix": "app."}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> p.in",
            "p.out -> h.in",
            "h.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    let site = |node: &str| case.frame.site(&id(node));
    assert_eq!(site("p"), site("h"));
    assert_ne!(
        site("h"),
        site("policy/f"),
        "R1 cuts between the enrichment and the filter"
    );

    assert_eq!(optimized.records.len(), 1, "{:?}", optimized.records);
    let record = &optimized.records[0];
    assert_eq!(record.nodes, vec![id("h"), id("policy/f"), id("policy/f_drop")]);
    match &record.outcome {
        RewriteOutcome::Skipped { reason } => assert!(
            reason.starts_with("the rewritten plan would move the kernel `h` from `region_p` to `region_f`"),
            "{}",
            reason
        ),
        RewriteOutcome::Applied => panic!("the application was accepted: {:?}", record),
    }
    assert_eq!(optimized.plan, case.plan);
}

#[test]
fn a_filter_crosses_maps_and_merges_only() {
    // Test operators the registry has no example of: an expansion, a map with two inputs that is not a merge, and an
    // order-dependent filter operator. The first two are uncrossable; the third is no filter.
    use crate::{
        ir::DecodedParams,
        model::PortType,
        operators::{
            Cardinality, EffectContract, InputPort, OperatorDef, OrderContract, OutputPort, PortMapping, Ports,
        },
    };

    struct Expand;
    impl OperatorDef for Expand {
        fn name(&self) -> &'static str {
            "test_expand"
        }
        fn summary(&self) -> &'static str {
            "Makes up to two records of each."
        }
        fn params(&self) -> &'static [crate::ir::ParamSpec] {
            &[]
        }
        fn ports(&self, _: &DecodedParams) -> Ports {
            Ports::single(PortType::ANY, PortMapping::IDENTITY)
        }
        fn contract(&self, _: &DecodedParams) -> EffectContract {
            EffectContract {
                cardinality: Cardinality::Expand { max: Some(2) },
                ..EffectContract::passthrough()
            }
        }
    }

    struct Join;
    impl OperatorDef for Join {
        fn name(&self) -> &'static str {
            "test_join"
        }
        fn summary(&self) -> &'static str {
            "Joins two inputs, not as a merge does."
        }
        fn params(&self) -> &'static [crate::ir::ParamSpec] {
            &[]
        }
        fn ports(&self, _: &DecodedParams) -> Ports {
            let input = |name: &'static str| InputPort {
                name: name.parse().unwrap(),
                accepts: PortType::ANY,
            };
            Ports {
                inputs: vec![input("0"), input("1")],
                outputs: vec![OutputPort {
                    name: "out".parse().unwrap(),
                    mapping: PortMapping::IDENTITY,
                }],
            }
        }
        fn contract(&self, _: &DecodedParams) -> EffectContract {
            EffectContract::passthrough()
        }
    }

    struct First;
    impl OperatorDef for First {
        fn name(&self) -> &'static str {
            "test_first"
        }
        fn summary(&self) -> &'static str {
            "Keeps the first record of each name."
        }
        fn params(&self) -> &'static [crate::ir::ParamSpec] {
            &[]
        }
        fn ports(&self, _: &DecodedParams) -> Ports {
            Ports::single(PortType::ANY, PortMapping::IDENTITY)
        }
        fn contract(&self, _: &DecodedParams) -> EffectContract {
            EffectContract {
                cardinality: Cardinality::Filter,
                order: OrderContract::Dependent,
                ..EffectContract::passthrough()
            }
        }
    }

    let operators = OperatorRegistry::native()
        .iter()
        .chain([&Expand as &'static dyn OperatorDef, &Join, &First])
        .collect::<Vec<_>>();
    let registry = OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()));
    let filtered = |mut nodes: Vec<Value>, edges: &[&str]| {
        nodes.extend([
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ]);
        let mut edge_list = vec!["policy/f.false -> policy/f_drop.in", "policy/f.true -> policy/e.in"];
        edge_list.extend_from_slice(edges);
        plan(nodes, &edge_list, &["o0"])
    };
    let shapes = [
        (
            "an expansion",
            filtered(
                vec![source("otlp", "otlp"), node("o", "test_expand", json!({}), true)],
                &["otlp.metrics -> o.in", "o.out -> policy/f.in"],
            ),
        ),
        (
            "a map that joins two inputs, not a merge",
            filtered(
                vec![
                    source("otlp", "otlp"),
                    source("checks", "checks"),
                    node("o", "test_join", json!({}), true),
                ],
                &["otlp.metrics -> o.0", "checks.metrics -> o.1", "o.out -> policy/f.in"],
            ),
        ),
    ];
    for (what, shaped) in shapes {
        let report = Validator::new(&registry, &crate::compile::rewrite::harness::catalog()).validate(&shaped);
        assert!(report.is_valid(), "{}: {}", what, report);
        let frame = Frame::compute(&shaped, &registry, &crate::compile::rewrite::harness::namer).unwrap();
        let ctx = RewriteContext::new(&registry, &frame, &report);
        assert!(R5a.apply(&ctx, &shaped).is_none(), "{}", what);
    }

    // An order-dependent filter operator below a merge is not copied onto its inputs: it is no filter here.
    let first = plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            node("k", "test_first", json!({}), false),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/m.1",
            "policy/m.out -> policy/k.in",
            "policy/k.out -> policy/e.in",
        ],
        &["o0"],
    );
    let report = Validator::new(&registry, &crate::compile::rewrite::harness::catalog()).validate(&first);
    assert!(report.is_valid(), "{}", report);
    let frame = Frame::compute(&first, &registry, &crate::compile::rewrite::harness::namer).unwrap();
    assert!(R5a
        .apply(&RewriteContext::new(&registry, &frame, &report), &first)
        .is_none());
}

#[test]
fn a_filter_moved_above_an_operator_that_sheds_relies_on_the_capacity_rule() {
    let (case, optimized) = r5a(plan(
        vec![
            source("src", "otlp"),
            node("s", TEST_SHED, json!({"max_points": 1}), true),
            split("f", json!({"host": {"eq": "web-1"}}), false),
            drop("f_drop", false),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> s.in",
            "s.out -> policy/f.in",
            "policy/f.false -> policy/f_drop.in",
            "policy/f.true -> policy/e.in",
        ],
        &["o0"],
    ));
    let record = applied(&optimized)[0];
    assert!(record.capacity_rule);
    assert_eq!(
        effects(record),
        vec![
            ("policy/f".to_string(), TelemetryChange::Changed),
            ("policy/f_drop".to_string(), TelemetryChange::Changed),
            ("s".to_string(), TelemetryChange::Changed),
        ]
    );
    case.check_structure(&optimized).unwrap();

    // The filter now runs first: the shedding map no longer counts the large metric the filter drops, and still sheds
    // the one it keeps. Both plans deliver the small metric on `web-1`.
    let on = |name: &'static str, host: &'static str, points: &[f64]| {
        let context = saluki_core::data_model::event::metric::context::Context::from_static_name(name)
            .with_host(stringtheory::MetaString::from_static(host));
        saluki_core::data_model::event::metric::Metric::gauge(context, points)
    };
    let events = vec![crate::interpret::TraceEvent::Admit {
        source: id("src"),
        lane: 0,
        admitted_at: saluki_core::execution::AdmissionTime::from_unix_secs(1_000),
        batch: vec![
            on("kept", "web-1", &[1.0]),
            on("shed", "web-1", &[1.0, 2.0]),
            on("dropped", "db", &[1.0, 2.0, 3.0]),
        ],
    }];
    let output = crate::ir::OutputId::new("o0").unwrap();
    for (plan, shed) in [(&case.plan, (2, 5)), (&optimized.plan, (1, 2))] {
        let run = case.run(plan, &events).unwrap();
        let names = run
            .output(&output)
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["kept"]);
        let lost = run.loss(&id("s")).loss(saluki_core::execution::LossClass::Capacity);
        assert_eq!((lost.events, lost.points), shed);
    }

    // `explain` flags the reliance.
    let compiled = crate::compile::compile(&case.plan, &case.report, registry(), &case.frame).unwrap();
    let report = Validator::new(registry(), &crate::compile::rewrite::harness::catalog()).validate(&compiled.plan);
    let text = crate::explain::Explain::of(&compiled.plan, registry(), &report)
        .with_rewrites(&compiled.rewrites)
        .to_text();
    assert!(
        text.contains("R5a applied: s, policy/f, policy/f_drop [capacity rule]"),
        "{}",
        text
    );
}

#[test]
fn copies_of_a_filter_with_a_long_id_get_distinct_valid_ids() {
    let long = "f".repeat(60);
    let (case, optimized) = r5a(plan(
        vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            source("otlp", "otlp"),
            node("m", names::MERGE, json!({"inputs": 2}), false),
            split(&long, json!({"kind_in": ["gauge"]}), false),
            drop("d", false),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> policy/m.0",
            "otlp.metrics -> policy/m.1",
            &format!("policy/m.out -> policy/{}.in", long),
            &format!("policy/{}.false -> policy/d.in", long),
            &format!("policy/{}.true -> policy/e.in", long),
        ],
        &["o0"],
    ));
    let copies = ids(&optimized.plan)
        .into_iter()
        .filter(|id| id.contains("_r5a"))
        .collect::<Vec<_>>();
    assert_eq!(
        copies,
        vec![
            format!("policy/{}_r5a0", "f".repeat(58)),
            "policy/d_r5a0".to_string(),
            format!("policy/{}_r5a1", "f".repeat(58)),
            "policy/d_r5a1".to_string(),
        ]
    );
    case.check_structure(&optimized).unwrap();
}

#[test]
fn the_default_program_is_left_as_it_is() {
    // Its only filter R5a could reach, the post-aggregation filter below its merge, is native: it would be copied.
    let plan = Plan::from_json(crate::testing::DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &crate::testing::default_program_catalog()).validate(&plan);
    let frame = Frame::compute(&plan, &registry, &crate::testing::default_program_slot_name).unwrap();
    let optimized = run_pipeline(&plan, &RewriteContext::new(&registry, &frame, &report), &[&R5a]);
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn property_test_r5a_rewritten_plans_deliver_what_the_original_delivers(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let optimized = case.optimize(&[&R5a]);
        prop_assert!(optimized.converged, "R5a did not reach a fixed point: {:?}", optimized.records);
        let structure = case.check_structure(&optimized);
        prop_assert!(structure.is_ok(), "{:?}", structure.err());
        let equivalent = case.check_equivalent(&optimized.plan, &trace);
        prop_assert!(equivalent.is_ok(), "{:?}", equivalent.err());
    }

    #[test]
    fn property_test_r5a_only_reduces_capacity_loss(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let optimized = case.optimize(&[&R5a]);
        let capacity = case.check_capacity(&optimized.plan, &trace);
        prop_assert!(capacity.is_ok(), "{:?}", capacity.err());
    }
}

#[test]
fn r5a_applies_to_random_plans_in_every_form() {
    // The property tests would hold vacuously if R5a never applied: count each form over a sample.
    let (mut whole, mut copied, mut some_inputs, mut partial, mut shedding) = (0, 0, 0, 0, 0);
    let (mut skipped, mut pressured) = (0, 0);
    for (case, trace) in sample(256) {
        let optimized = case.optimize(&[&R5a]);
        for record in &optimized.records {
            let text = &record.justification;
            match &record.outcome {
                RewriteOutcome::Skipped { .. } => skipped += 1,
                RewriteOutcome::Applied if text.contains("whole above") => whole += 1,
                RewriteOutcome::Applied if text.contains("into each of") => copied += 1,
                RewriteOutcome::Applied if text.contains("into the inputs") => some_inputs += 1,
                RewriteOutcome::Applied => {
                    assert!(text.contains("a copy of"), "{}", text);
                    partial += 1;
                }
            }
            if record.is_applied() && record.capacity_rule {
                shedding += 1;
            }
        }
        if optimized.changed() && case.check_capacity(&optimized.plan, &trace).unwrap() {
            pressured += 1;
        }
    }
    eprintln!(
        "whole {} copied {} into some inputs {} partial {} across shedding {} skipped {} pressured {}",
        whole, copied, some_inputs, partial, shedding, skipped, pressured
    );
    assert!(whole > 10, "{} whole moves", whole);
    assert!(copied > 10, "{} copies across merges", copied);
    assert!(some_inputs > 3, "{} copies onto some inputs of merges", some_inputs);
    assert!(partial > 10, "{} partial push-downs", partial);
    assert!(shedding > 3, "{} moves across an operator that sheds", shedding);
    assert!(pressured > 10, "{} rewritten plans under capacity pressure", pressured);
    // Skips are allowed, but should be rare: every R5a site keeps work in the frame's structure.
    assert!(skipped < 20, "{} skipped applications", skipped);
}

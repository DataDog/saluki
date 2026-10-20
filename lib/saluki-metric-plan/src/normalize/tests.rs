use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{json, Value};

use super::*;
use crate::{
    compile::{
        rewrite::harness::{self, arb_plan_params, arb_shape, arb_trace, Case},
        KernelKey, LineageKey,
    },
    export::{export_as, ExportForm},
    ir::{decode_params, ArtifactEdge, ArtifactNode, Node, ParamValue, IR_VERSION},
    model::EventView,
    predicate::NoBindings,
    testing::{arb_metric, arb_predicate, default_program_catalog, DEFAULT_PROGRAM_JSON},
};

fn predicate(value: Value) -> Predicate {
    serde_json::from_value(value).expect("test predicates are valid")
}

fn params(value: Value) -> Params {
    serde_json::from_value(value).expect("test parameters are an object")
}

fn op(name: &str) -> OperatorName {
    OperatorName::new(name).expect("valid operator name")
}

/// Returns `plan`'s edges as `from -> to`, in plan order.
fn edges_of(plan: &Plan) -> Vec<String> {
    plan.edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect()
}

// Rule 1: defaults.

/// Every default a native operator declares is a value of its parameter's type, and a parameter with a default may be
/// left out: a default is what a node that leaves it out takes, never a value it must write.
#[test]
fn every_declared_default_decodes_as_its_parameter() {
    let mut defaults = 0;
    for def in OperatorRegistry::native().iter() {
        for spec in def.params() {
            let Some(default) = spec.default_value() else { continue };
            defaults += 1;
            let default = default.unwrap_or_else(|error| panic!("`{}.{}`: {}", def.name(), spec.name, error));
            assert!(
                !spec.required,
                "`{}.{}` has a default, so it is optional",
                def.name(),
                spec.name
            );
            ParamValue::decode(&spec.ty, &default, spec.name)
                .unwrap_or_else(|error| panic!("`{}.{}`: {}", def.name(), spec.name, error));
        }
    }
    assert_eq!(defaults, 12, "the native operators declare 12 defaults");
}

/// The parameters of an `aggregate` node that writes only its window.
fn aggregate_with_only_a_window() -> Params {
    params(json!({"window": "10s"}))
}

/// The same `aggregate` node with every default written, as native configuration's defaults lower it.
fn aggregate_with_every_default() -> Params {
    params(json!({
        "window": "10s",
        "flush_interval": "15s",
        "context_limit": 1_000_000,
        "counter_expiry": "5m",
        "flush_open_windows": false,
        "histogram_aggregates": ["max", "median", "avg", "count"],
        "histogram_percentiles": [95],
        "copy_to_distribution": false,
        "copy_to_distribution_prefix": "",
    }))
}

#[test]
fn defaults_are_written_in_and_decode_alike() {
    let aggregate = op(names::AGGREGATE);
    let (left_out, written) = (aggregate_with_only_a_window(), aggregate_with_every_default());
    assert_eq!(normalize_params(&aggregate, &left_out), written);
    // Nothing is written over a value the node gives, a default or not.
    assert_eq!(normalize_params(&aggregate, &written), written);
    let mut other = written.clone();
    other.insert("context_limit".parse().unwrap(), json!(5));
    assert_eq!(normalize_params(&aggregate, &other), other);

    // A parameter left out decodes as its default.
    let def = OperatorRegistry::native().get(names::AGGREGATE).unwrap();
    assert_eq!(
        decode_params(def.params(), &left_out).unwrap(),
        decode_params(def.params(), &written).unwrap()
    );

    // A required parameter without a default is still required.
    let errors = decode_params(def.params(), &Params::new()).unwrap_err();
    assert_eq!(errors.len(), 1, "{:?}", errors);
    assert_eq!(errors[0].path, "window");

    // The mapper's and tag projection's caches and value rules, likewise.
    let mapper = params(json!({"profiles": []}));
    assert_eq!(
        normalize_params(&op(names::DOGSTATSD_MAPPER), &mapper),
        params(json!({"profiles": [], "cache_size": 1000, "string_interner_bytes": 65536}))
    );
    let projection = params(json!({"rules": []}));
    assert_eq!(
        normalize_params(&op(names::TAG_PROJECTION), &projection),
        params(json!({"rules": [], "value_rules": [], "cache_capacity": 100000}))
    );
}

/// A node that writes its defaults and one that leaves them out build the same kernel: their kernel keys and their
/// lineages are equal, whether or not the node is normalized first.
#[test]
fn kernel_identity_of_normal_form_does_not_depend_on_whether_defaults_are_written() {
    let registry = OperatorRegistry::native();
    let node = |params: Params| Node {
        id: "dsd_agg".parse().unwrap(),
        op: op(names::AGGREGATE),
        params,
    };
    let written = node(aggregate_with_every_default());
    let left_out = node(aggregate_with_only_a_window());
    let normalized = node(normalize_params(&left_out.op, &left_out.params));
    // Kernels are keyed by the parameters of the core plan, which elaboration writes in normal form.
    let key = KernelKey::of(&written).unwrap();
    assert_eq!(KernelKey::of(&normalized).unwrap(), key);

    let lineage = |node: &Node| {
        let configured = registry.configure(node.op.as_str(), &node.params).unwrap();
        LineageKey::of(&node.id, &configured).unwrap()
    };
    assert_eq!(lineage(&left_out), lineage(&written));
    assert_eq!(lineage(&normalized), lineage(&written));
    // And they are kernels of the parameters they have: another value is another lineage.
    let mut other = aggregate_with_every_default();
    other.insert("window".parse().unwrap(), json!("20s"));
    assert_ne!(lineage(&node(other)), lineage(&written));
}

// Rules 2 and 3: predicates.

#[test]
fn predicates_normalize_to_one_form_keeping_their_order() {
    // (input, normal form)
    let cases = vec![
        // Literal set matches become the string match they are.
        (
            json!({"name": {"matches": {"values": ["a"], "prefix": false}}}),
            json!({"name": {"eq": "a"}}),
        ),
        (
            json!({"name": {"matches": {"values": ["a."], "prefix": true}}}),
            json!({"name": {"prefix": "a."}}),
        ),
        (
            json!({"intake_name": {"matches": {"values": ["b", "a"], "prefix": false}}}),
            json!({"intake_name": {"in": ["b", "a"]}}),
        ),
        (
            json!({"host": {"matches": {"values": ["b.", "a."], "prefix": true}}}),
            json!({"host": {"prefix_in": ["b.", "a."]}}),
        ),
        (
            json!({"name": {"matches": {"values": [], "prefix": true}}}),
            json!({"const": false}),
        ),
        // One value is an `eq` or a `prefix`, none matches nothing, and a value listed twice is kept where it is first.
        (json!({"name": {"in": ["a"]}}), json!({"name": {"eq": "a"}})),
        (
            json!({"name": {"prefix_in": ["a."]}}),
            json!({"name": {"prefix": "a."}}),
        ),
        (json!({"name": {"in": []}}), json!({"const": false})),
        (
            json!({"name": {"in": ["b", "a", "b"]}}),
            json!({"name": {"in": ["b", "a"]}}),
        ),
        (json!({"name": {"in": ["a", "a"]}}), json!({"name": {"eq": "a"}})),
        (
            json!({"tag": {"key": "env", "value": {"in": ["prod"]}}}),
            json!({"tag": {"key": "env", "value": {"eq": "prod"}}}),
        ),
        (
            json!({"origin_tag": {"key": "pod", "value": {"prefix_in": []}}}),
            json!({"const": false}),
        ),
        // Bound operands are left alone, whatever their value.
        (
            json!({"name": {"matches": {"values": {"config": "x.names", "resolved": []}, "prefix": false}}}),
            json!({"name": {"matches": {"values": {"config": "x.names", "resolved": []}, "prefix": false}}}),
        ),
        (
            json!({"name": {"matches": {"values": ["a"], "prefix": {"config": "x.prefix"}}}}),
            json!({"name": {"matches": {"values": ["a"], "prefix": {"config": "x.prefix"}}}}),
        ),
        (json!({"flag": {"config": "x.on"}}), json!({"flag": {"config": "x.on"}})),
        (
            json!({"is_empty": {"config": "x.names"}}),
            json!({"is_empty": {"config": "x.names"}}),
        ),
        // Literal flags and emptiness tests are constants.
        (json!({"flag": true}), json!({"const": true})),
        (json!({"is_empty": []}), json!({"const": true})),
        (json!({"is_empty": ["a"]}), json!({"const": false})),
        // Junctions of one kind are flattened, in order, and never sorted or deduplicated.
        (
            json!({"all": [{"has_tag": "b"}, {"all": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            json!({"all": [{"has_tag": "b"}, {"has_tag": "a"}, {"has_tag": "b"}, {"has_tag": "c"}]}),
        ),
        (
            json!({"any": [{"any": [{"has_tag": "b"}]}, {"all": [{"has_tag": "a"}, {"has_tag": "c"}]}]}),
            json!({"any": [{"has_tag": "b"}, {"all": [{"has_tag": "a"}, {"has_tag": "c"}]}]}),
        ),
        // Constants fold: neutral ones are dropped, decisive ones decide.
        (
            json!({"any": [{"name": {"matches": {"values": [], "prefix": false}}}, {"name": {"prefix_in": ["c."]}}]}),
            json!({"name": {"prefix": "c."}}),
        ),
        (
            json!({"all": [{"const": true}, {"has_tag": "a"}, {"flag": true}, {"has_tag": "b"}]}),
            json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}]}),
        ),
        (
            json!({"all": [{"has_tag": "a"}, {"is_empty": ["x"]}]}),
            json!({"const": false}),
        ),
        (
            json!({"any": [{"has_tag": "a"}, {"is_empty": []}]}),
            json!({"const": true}),
        ),
        (json!({"all": []}), json!({"const": true})),
        (json!({"any": []}), json!({"const": false})),
        (
            json!({"any": [{"const": false}, {"const": false}]}),
            json!({"const": false}),
        ),
        // Double negations go, and `not` of a constant is the other constant.
        (json!({"not": {"not": {"host_unset": {}}}}), json!({"host_unset": {}})),
        (
            json!({"not": {"not": {"not": {"host_unset": {}}}}}),
            json!({"not": {"host_unset": {}}}),
        ),
        (json!({"not": {"flag": false}}), json!({"const": true})),
        (
            json!({"not": {"all": [{"not": {"has_tag": "a"}}]}}),
            json!({"has_tag": "a"}),
        ),
        // `kind_in` is never folded: it reads the metric.
        (json!({"kind_in": "all"}), json!({"kind_in": "all"})),
    ];
    for (input, expected) in cases {
        let normal = normalize_predicate(&predicate(input.clone()));
        assert_eq!(normal, predicate(expected.clone()), "{}", input);
        assert_eq!(
            normalize_predicate(&normal),
            normal,
            "normal forms are fixed points: {}",
            input
        );
    }
}

#[test]
fn split_predicates_are_normalized_and_others_left_alone() {
    let split = op(names::SPLIT);
    let normal = normalize_params(&split, &params(json!({"predicate": {"name": {"in": ["a"]}}})));
    assert_eq!(normal, params(json!({"predicate": {"name": {"eq": "a"}}})));
    // A predicate that does not read, and an operator the registry does not know, are left for validation.
    let invalid = params(json!({"predicate": {"name": {"equals": "a"}}}));
    assert_eq!(normalize_params(&split, &invalid), invalid);
    let unknown = params(json!({"predicate": {"name": {"in": ["a"]}}}));
    assert_eq!(normalize_params(&op("test_keep"), &unknown), unknown);
}

// Rule 4: split polarity.

/// A plan of a `split` with predicate `predicate` whose `true` port leads to `on_true` and whose `false` port leads to
/// `on_false`, each a node of the given ID and operator.
fn split_plan(predicate: Value, on_true: (&str, &str), on_false: (&str, &str)) -> Plan {
    let terminal = |(id, op): (&str, &str)| match op {
        names::EMIT => harness::emit(id, "o0"),
        _ => harness::drop(id, false),
    };
    harness::plan(
        vec![
            harness::node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            harness::split("s", predicate, false),
            terminal(on_true),
            terminal(on_false),
        ],
        &[
            "dsd.metrics -> policy/s.in",
            &format!("policy/s.true -> policy/{}.in", on_true.0),
            &format!("policy/s.false -> policy/{}.in", on_false.0),
        ],
        &["o0"],
    )
}

#[test]
fn a_split_that_drops_its_true_port_is_turned_and_its_derived_terminals_renamed() {
    let plan = split_plan(
        json!({"name": {"in": ["a"]}}),
        ("s_true", names::DROP),
        ("s_false", names::EMIT),
    );
    let normal = normalize_plan(&plan);
    assert_eq!(
        normal.node(&"policy/s".parse().unwrap()).unwrap().params,
        params(json!({"predicate": {"not": {"name": {"eq": "a"}}}}))
    );
    // The edges keep their order and swap ports, and the drop and the emit trade derived IDs.
    assert_eq!(
        edges_of(&normal),
        [
            "dsd.metrics -> policy/s.in",
            "policy/s.false -> policy/s_false.in",
            "policy/s.true -> policy/s_true.in",
        ]
    );
    let ops = normal
        .nodes
        .iter()
        .map(|node| (node.id.to_string(), node.op.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        ops,
        [
            ("dsd".to_string(), "source".to_string()),
            ("policy/s".to_string(), "split".to_string()),
            ("policy/s_false".to_string(), "drop".to_string()),
            ("policy/s_true".to_string(), "emit".to_string()),
        ]
    );
    Case::of(normal.clone(), Vec::new()).expect("the normal form validates");
    assert_eq!(normalize_plan(&normal), normal, "normal forms are fixed points");
}

#[test]
fn turning_a_split_removes_a_double_negation_and_keeps_names_that_are_not_derived() {
    let plan = split_plan(
        json!({"not": {"has_tag": "debug"}}),
        ("debug_drop", names::DROP),
        ("kept", names::EMIT),
    );
    let normal = normalize_plan(&plan);
    assert_eq!(
        normal.node(&"policy/s".parse().unwrap()).unwrap().params,
        params(json!({"predicate": {"has_tag": "debug"}}))
    );
    assert_eq!(
        edges_of(&normal),
        [
            "dsd.metrics -> policy/s.in",
            "policy/s.false -> policy/debug_drop.in",
            "policy/s.true -> policy/kept.in",
        ]
    );
    assert_eq!(harness::ids(&normal), harness::ids(&plan));

    // Only the derived one of the two is renamed.
    let normal = normalize_plan(&split_plan(
        json!({"has_tag": "a"}),
        ("s_true", names::DROP),
        ("kept", names::EMIT),
    ));
    assert_eq!(
        harness::ids(&normal),
        ["dsd", "policy/s", "policy/s_false", "policy/kept"]
    );
}

#[test]
fn a_rename_that_would_take_another_nodes_id_is_not_made() {
    // The `false` port leads to a node named `s_false` that is not a terminal, so the drop keeps `s_true`.
    let plan = harness::plan(
        vec![
            harness::node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            harness::split("s", json!({"has_tag": "a"}), false),
            harness::drop("s_true", false),
            harness::node("s_false", names::HOST_ENRICHMENT, json!({}), false),
            harness::emit("out", "o0"),
        ],
        &[
            "dsd.metrics -> policy/s.in",
            "policy/s.true -> policy/s_true.in",
            "policy/s.false -> policy/s_false.in",
            "policy/s_false.out -> policy/out.in",
        ],
        &["o0"],
    );
    let normal = normalize_plan(&plan);
    assert_eq!(harness::ids(&normal), harness::ids(&plan));
    assert_eq!(
        edges_of(&normal)[1..3],
        [
            "policy/s.false -> policy/s_true.in",
            "policy/s.true -> policy/s_false.in"
        ]
    );
    Case::of(normal.clone(), Vec::new()).expect("the normal form validates");
    assert_eq!(normalize_plan(&normal), normal);
}

#[test]
fn splits_already_in_where_form_or_dropping_both_ports_are_left_alone() {
    for plan in [
        split_plan(
            json!({"has_tag": "a"}),
            ("s_true", names::EMIT),
            ("s_false", names::DROP),
        ),
        split_plan(
            json!({"has_tag": "a"}),
            ("s_true", names::DROP),
            ("s_false", names::DROP),
        ),
        split_plan(
            json!({"has_tag": "a"}),
            ("s_true", names::EMIT),
            ("s_false", names::EMIT),
        ),
    ] {
        assert_eq!(normalize_plan(&plan), plan);
    }
}

#[test]
fn renamed_terminals_are_renamed_in_provenance() {
    let mut plan = harness::plan(
        vec![
            harness::node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            harness::split("s", json!({"has_tag": "a"}), true),
            harness::drop("s_true", true),
            harness::node("s_false", names::EMIT, json!({"output": "o0"}), true),
        ],
        &["dsd.metrics -> s.in", "s.true -> s_true.in", "s.false -> s_false.in"],
        &["o0"],
    );
    plan.provenance = Some(crate::ir::PlanProvenance {
        native_components: [("s_true", "dsd_filter"), ("s_false", "dsd_out")]
            .into_iter()
            .map(|(node, component)| (node.parse().unwrap(), component.parse().unwrap()))
            .collect(),
        ..Default::default()
    });
    let normal = normalize_plan(&plan);
    let components = &normal.provenance.as_ref().unwrap().native_components;
    let components = components
        .iter()
        .map(|(node, component)| (node.to_string(), component.to_string()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        components,
        BTreeMap::from([
            ("s_false".to_string(), "dsd_filter".to_string()),
            ("s_true".to_string(), "dsd_out".to_string()),
        ])
    );
}

// The artifact form.

fn artifact_of(nodes: Vec<(&str, &str, Value)>, edges: &[&str], blobs: Blobs) -> PolicyArtifact {
    PolicyArtifact {
        ir_version: IR_VERSION,
        nodes: nodes
            .into_iter()
            .map(|(id, op, params)| ArtifactNode {
                id: id.parse().unwrap(),
                op: self::op(op),
                params: self::params(params),
            })
            .collect(),
        edges: edges
            .iter()
            .map(|edge| {
                let (from, to) = edge.split_once(" -> ").unwrap();
                ArtifactEdge {
                    from: from.parse().unwrap(),
                    to: to.parse().unwrap(),
                }
            })
            .collect(),
        blobs,
    }
}

#[test]
fn artifacts_normalize_as_plans_do() {
    let artifact = artifact_of(
        vec![
            ("agg", names::AGGREGATE, json!({"window": "10s"})),
            (
                "s",
                names::SPLIT,
                json!({"predicate": {"name": {"matches": {"values": ["a"], "prefix": true}}}}),
            ),
            ("s_true", names::DROP, json!({})),
            ("s_false", names::EMIT, json!({"output": "o0"})),
        ],
        &[
            "input.metrics -> agg.in",
            "agg.out -> s.in",
            "s.true -> s_true.in",
            "s.false -> s_false.in",
        ],
        Blobs::default(),
    );
    let normal = normalize_artifact(&artifact);
    let expected = artifact_of(
        vec![
            (
                "agg",
                names::AGGREGATE,
                Value::Object(
                    aggregate_with_every_default()
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.clone()))
                        .collect(),
                ),
            ),
            (
                "s",
                names::SPLIT,
                json!({"predicate": {"not": {"name": {"prefix": "a"}}}}),
            ),
            ("s_false", names::DROP, json!({})),
            ("s_true", names::EMIT, json!({"output": "o0"})),
        ],
        &[
            "input.metrics -> agg.in",
            "agg.out -> s.in",
            "s.false -> s_false.in",
            "s.true -> s_true.in",
        ],
        Blobs::default(),
    );
    assert_eq!(normal, expected);
    assert_eq!(normalize_artifact(&normal), normal);

    // A `false` port that leaves the scope is not a `drop`, so its `split` is turned too.
    let artifact = artifact_of(
        vec![
            ("s", names::SPLIT, json!({"predicate": {"has_tag": "a"}})),
            ("s_true", names::DROP, json!({})),
        ],
        &["input.metrics -> s.in", "s.true -> s_true.in", "s.false -> exit.kept"],
        Blobs::default(),
    );
    let normal = normalize_artifact(&artifact);
    let edges = normal
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    assert_eq!(
        edges,
        ["input.metrics -> s.in", "s.false -> s_false.in", "s.true -> exit.kept"]
    );
}

#[test]
fn artifact_predicates_are_read_through_blobs_and_written_inline_only_when_they_change() {
    let mut blobs = Blobs::default();
    let normal = json!({"name": {"in": ["a", "b"]}});
    let hash = blobs.insert(normal.clone()).unwrap();
    let reference = json!({"blob": hash.to_string()});

    // A predicate in normal form keeps its reference.
    let artifact = artifact_of(
        vec![
            ("s", names::SPLIT, json!({"predicate": reference})),
            ("s_true", names::EMIT, json!({"output": "o0"})),
            ("s_false", names::DROP, json!({})),
        ],
        &["input.metrics -> s.in", "s.true -> s_true.in", "s.false -> s_false.in"],
        blobs.clone(),
    );
    assert_eq!(normalize_artifact(&artifact), artifact);

    // One that a rule changes is written inline: here, turned, and in a list read through its reference.
    let list = blobs.insert(json!(["a"])).unwrap();
    let artifact = artifact_of(
        vec![
            (
                "s",
                names::SPLIT,
                json!({"predicate": {"name": {"in": {"blob": list.to_string()}}}}),
            ),
            ("s_true", names::DROP, json!({})),
            ("s_false", names::EMIT, json!({"output": "o0"})),
        ],
        &["input.metrics -> s.in", "s.true -> s_true.in", "s.false -> s_false.in"],
        blobs,
    );
    let normal = normalize_artifact(&artifact);
    assert_eq!(
        normal.nodes[0].params,
        params(json!({"predicate": {"not": {"name": {"eq": "a"}}}}))
    );
    assert_eq!(normal.blobs, artifact.blobs);
}

// The default program.

/// The default-program fixtures are in normal form, as lowering writes the default program, and normalization keeps
/// every node, its stateful aggregation included, under its ID.
#[test]
fn the_default_program_fixtures_are_in_normal_form() {
    for json in [DEFAULT_PROGRAM_JSON, crate::testing::NO_AGGREGATION_PIPELINE_OFF_JSON] {
        let plan = Plan::from_json(json).unwrap();
        assert_eq!(normalize_plan(&plan), plan);
    }
}

/// The detached export of the default program, whose filterlists are literals, is in normal form: its filters are
/// `where`-shaped, `false` to a drop, and the empty names list of the second routing group folds away.
#[test]
fn the_detached_export_of_the_default_program_is_in_normal_form() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let bundle = export_as(
        &plan,
        &default_program_catalog(),
        &BTreeMap::new(),
        ExportForm::Detached,
        &OperatorRegistry::native(),
    )
    .unwrap();
    let artifact = &bundle.artifact;
    assert_eq!(normalize_artifact(artifact), *artifact);

    let node = |id: &str| {
        artifact
            .nodes
            .iter()
            .find(|node| node.id.as_str() == id)
            .unwrap_or_else(|| panic!("the export has `{}`", id))
    };
    let target = |from: &str| {
        artifact
            .edges
            .iter()
            .find(|edge| edge.from.to_string() == from)
            .map(|edge| edge.to.to_string())
            .unwrap_or_else(|| panic!("an edge leaves `{}`", from))
    };
    for filter in ["dsd_prefix_filter", "dsd_post_agg_filter"] {
        let drop = format!("{}_false", filter);
        assert_eq!(target(&format!("{}.false", filter)), format!("{}.in", drop));
        assert_eq!(node(&drop).op.as_str(), names::DROP);
    }
    assert_eq!(
        node("dsd_prefix_filter").params,
        params(json!({"predicate": {"not": {"intake_name": {"eq": "tenant.raw.blocked"}}}}))
    );
    assert_eq!(
        node("dsd_post_agg_filter").params,
        params(json!({"predicate": {"not": {"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"intake_name": {"eq": "tenant.checkout.latency.max"}},
        ]}}}))
    );
    // The second routing group lists no names: `matches(values: [], prefix: false) or ...` folds to its prefixes.
    assert_eq!(
        node("metrics_routing_filter_1").params,
        params(json!({"predicate": {"intake_name": {"prefix": "catalog."}}}))
    );
    assert_eq!(
        target("metrics_routing_filter_1.false"),
        "metrics_routing_filter_1_false.in"
    );
}

/// Two copies that a pair of filters on one predicate send opposite ways reach an output once, and still do when
/// normalization turns one of the filters: the duplicate analysis reads `split(not p)` as `split(p)` with its ports
/// swapped, and `const(true)` as the negation of `const(false)`.
#[test]
fn complementary_filters_stay_complementary_when_one_is_turned() {
    for predicate in [
        json!({"has_tag": "a"}),
        json!({"const": false}),
        json!({"not": {"has_tag": "a"}}),
    ] {
        let plan = harness::plan(
            vec![
                harness::node(
                    "dsd",
                    names::SOURCE,
                    json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                    true,
                ),
                harness::node("t", names::TEE, json!({"ports": 2}), false),
                harness::split("a", predicate.clone(), false),
                harness::drop("a_false", false),
                harness::split("b", predicate.clone(), false),
                harness::drop("b_true", false),
                harness::node("m", names::MERGE, json!({"inputs": 2}), false),
                harness::emit("out", "o0"),
            ],
            &[
                "dsd.metrics -> policy/t.in",
                "policy/t.0 -> policy/a.in",
                "policy/t.1 -> policy/b.in",
                "policy/a.true -> policy/m.0",
                "policy/a.false -> policy/a_false.in",
                "policy/b.true -> policy/b_true.in",
                "policy/b.false -> policy/m.1",
                "policy/m.out -> policy/out.in",
            ],
            &["o0"],
        );
        Case::of(plan.clone(), Vec::new()).unwrap_or_else(|error| panic!("{}: {}", predicate, error));
        let normal = normalize_plan(&plan);
        assert!(
            harness::ids(&normal).contains(&"policy/b_false".to_string()),
            "{}",
            predicate
        );
        Case::of(normal, Vec::new()).unwrap_or_else(|error| panic!("{}: {}", predicate, error));
    }
}

// Properties.

/// Returns `plan` with every `drop` that a `split` feeds named with its derived ID, as lowering and the surface syntax
/// name them, when no other node has that ID.
fn with_derived_drops(plan: &Plan) -> Plan {
    let mut renames = BTreeMap::new();
    let mut taken = plan.nodes.iter().map(|node| node.id.clone()).collect::<BTreeSet<_>>();
    for edge in &plan.edges {
        let (Some(from), Some(to)) = (plan.node(&edge.from.node), plan.node(&edge.to.node)) else {
            continue;
        };
        if from.op.as_str() == names::SPLIT && to.op.as_str() == names::DROP {
            let derived = derived_terminal_id(&from.id, &edge.from.port);
            if taken.insert(derived.clone()) {
                renames.insert(to.id.clone(), derived);
            }
        }
    }
    let mut renamed = plan.clone();
    let rename = |id: &mut NodeId| {
        if let Some(new) = renames.get(id) {
            *id = new.clone();
        }
    };
    renamed.nodes.iter_mut().for_each(|node| rename(&mut node.id));
    for edge in &mut renamed.edges {
        rename(&mut edge.from.node);
        rename(&mut edge.to.node);
    }
    renamed
}

/// Returns `plan` as an artifact of local IDs, with the same nodes and edges.
fn as_artifact(plan: &Plan) -> PolicyArtifact {
    let local = |id: &NodeId| id.local().as_str().to_string();
    PolicyArtifact {
        ir_version: plan.ir_version,
        nodes: plan
            .nodes
            .iter()
            .map(|node| ArtifactNode {
                id: local(&node.id).parse().unwrap(),
                op: node.op.clone(),
                params: node.params.clone(),
            })
            .collect(),
        edges: plan
            .edges
            .iter()
            .map(|edge| ArtifactEdge {
                from: format!("{}.{}", local(&edge.from.node), edge.from.port)
                    .parse()
                    .unwrap(),
                to: format!("{}.{}", local(&edge.to.node), edge.to.port).parse().unwrap(),
            })
            .collect(),
        blobs: Blobs::default(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_normal_predicates_match_what_they_normalize_and_are_fixed_points(
        predicate in arb_predicate(),
        metrics in proptest::collection::vec(arb_metric(), 1..8),
    ) {
        let normal = normalize_predicate(&predicate);
        prop_assert_eq!(normalize_predicate(&normal), normal.clone(), "normalizing twice changes {:?}", predicate);
        let original = predicate.compile(&NoBindings).expect("generated predicates compile");
        let compiled = normal.compile(&NoBindings).expect("normal forms compile");
        for metric in &metrics {
            prop_assert_eq!(
                original.evaluate(&mut EventView::new(metric)),
                compiled.evaluate(&mut EventView::new(metric)),
                "normal form {:?} disagrees with {:?} on {:?}",
                normal,
                predicate,
                metric
            );
        }
        // A normal form reads no field its predicate does not.
        prop_assert_eq!(normal.reads().union(predicate.reads()), predicate.reads());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Normalizing a random plan, with its filters' drops named as lowering names them, gives a valid plan that
    /// delivers what the original delivers, in the reference interpreter; normalizing it again changes nothing; it has
    /// the same number of nodes, and every node but a renamed terminal keeps its ID, so no state owner ever changes its
    /// ID; and an artifact of the same graph normalizes alike.
    #[test]
    fn property_test_normal_forms_of_random_plans_deliver_what_the_originals_deliver(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let (plan, opaque) = harness::random_plan(&shape, &params);
        let plan = with_derived_drops(&plan);
        let case = Case::of(plan.clone(), opaque).map_err(TestCaseError::fail)?;
        let normal = normalize_plan(&plan);
        prop_assert_eq!(normalize_plan(&normal), normal.clone(), "normalizing twice changes the plan");
        Case::of(normal.clone(), case.opaque.clone()).map_err(TestCaseError::fail)?;
        case.check_equivalent(&normal, &trace).map_err(TestCaseError::fail)?;

        prop_assert_eq!(normal.nodes.len(), plan.nodes.len());
        for (before, after) in plan.nodes.iter().zip(&normal.nodes) {
            prop_assert_eq!(&before.op, &after.op);
            if before.id != after.id {
                prop_assert!(
                    matches!(before.op.as_str(), names::DROP | names::EMIT),
                    "`{}`, a `{}`, was renamed `{}`",
                    before.id,
                    before.op,
                    after.id
                );
            }
        }

        prop_assert_eq!(normalize_artifact(&as_artifact(&plan)), as_artifact(&normal));
    }
}

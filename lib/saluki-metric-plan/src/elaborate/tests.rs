//! Tests of elaboration: the core form, its one hash, and canonical order.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::*;
use crate::{
    compile::KernelKey,
    compose::{receive_artifact, tests::approval},
    export::export,
    ir::ParamName,
    testing::{catalog_for, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
    validate::MemoryCatalog,
};

/// The default-program fixtures, each with the host's catalog of it.
fn default_programs() -> Vec<(&'static str, Plan, MemoryCatalog)> {
    [
        ("default program", DEFAULT_PROGRAM_JSON),
        ("no-aggregation pipeline off", NO_AGGREGATION_PIPELINE_OFF_JSON),
    ]
    .into_iter()
    .map(|(name, json)| {
        let plan = Plan::from_json(json).unwrap();
        let catalog = catalog_for(&plan);
        (name, plan, catalog)
    })
    .collect()
}

/// Returns the bound export's artifact document of `plan`.
fn exported(plan: &Plan, catalog: &MemoryCatalog) -> Value {
    serde_json::to_value(export(plan, catalog, &BTreeMap::new()).unwrap().artifact).unwrap()
}

/// Receives and elaborates an artifact document against `plan`.
fn elaborated(plan: &Plan, catalog: &MemoryCatalog, document: Value) -> Elaborated {
    let received = receive_artifact(document).unwrap();
    elaborate(plan, &received, &approval(1), catalog, &OperatorRegistry::native()).unwrap()
}

/// Returns the artifact node with the given ID.
fn node<'a>(document: &'a mut Value, id: &str) -> &'a mut Value {
    document["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("the artifact has a node `{}`", id))
}

#[test]
fn the_unmodified_export_and_the_default_program_elaborate_to_one_plan_in_any_order() {
    let registry = OperatorRegistry::native();
    for (name, plan, catalog) in default_programs() {
        let default = elaborate_default(&plan, &catalog, &registry);
        assert!(default.report.errors().next().is_none(), "{}: {}", name, default.report);

        // The export, and the export with its nodes and edges listed backwards, as a printed program may list them.
        let mut reversed = exported(&plan, &catalog);
        reversed["nodes"].as_array_mut().unwrap().reverse();
        reversed["edges"].as_array_mut().unwrap().reverse();
        for document in [exported(&plan, &catalog), reversed] {
            let program = elaborated(&plan, &catalog, document);
            assert_eq!(program.plan.nodes, default.plan.nodes, "{}", name);
            assert_eq!(program.plan.edges, default.plan.edges, "{}", name);
            assert_eq!(program.plan.outputs, default.plan.outputs, "{}", name);
            assert_eq!(program.plan.resources, default.plan.resources, "{}", name);
            assert_eq!(program.hash().unwrap(), default.hash().unwrap(), "{}", name);
            assert_eq!(program.bindings, default.bindings, "{}", name);
            assert_eq!(program.report.findings(), default.report.findings(), "{}", name);
        }
    }
}

#[test]
fn the_core_form_is_a_fixed_point() {
    let registry = OperatorRegistry::native();
    for (name, plan, catalog) in default_programs() {
        let default = elaborate_default(&plan, &catalog, &registry);
        let again = elaborate_default(&default.plan, &catalog, &registry);
        assert_eq!(again.plan, default.plan, "{}", name);
        // Nothing is bound in a core plan, so nothing is recorded again.
        assert!(again.bindings.is_empty(), "{}", name);

        let program = elaborated(&plan, &catalog, exported(&plan, &catalog));
        assert_eq!(
            elaborate_default(&program.plan, &catalog, &registry).plan,
            program.plan,
            "{}",
            name
        );
    }
}

#[test]
fn a_binding_and_the_same_literal_elaborate_to_one_hash() {
    let (_, plan, catalog) = default_programs().remove(0);
    let bound = exported(&plan, &catalog);
    let program = elaborated(&plan, &catalog, bound.clone());

    // The export binds the host's tags, a fact, and a failover switch, a signal; an edit writes their values instead.
    let mut literal = bound;
    let tags = catalog
        .binding(&"host.tags".parse().unwrap())
        .and_then(|binding| binding.value.clone())
        .unwrap();
    node(&mut literal, "metrics")["params"]["tags"] = tags.clone();
    let failover = catalog
        .binding(&"autoscaling_failover.enabled".parse().unwrap())
        .and_then(|binding| binding.value.clone())
        .unwrap();
    let predicate = &mut node(&mut literal, "af_metrics_gateway")["params"]["predicate"];
    let operands = predicate["all"].as_array_mut().unwrap();
    for operand in operands {
        if operand["flag"]["config"] == "autoscaling_failover.enabled" {
            operand["flag"] = failover.clone();
        }
    }
    let written = elaborated(&plan, &catalog, literal);
    assert_eq!(written.plan.nodes, program.plan.nodes);
    assert_eq!(written.hash().unwrap(), program.hash().unwrap());

    // Only the annotation says which values came from bindings.
    let tags_node = NodeId::reserved("metrics".parse().unwrap());
    assert_eq!(
        program.bindings.of(&tags_node).last(),
        Some(&BindingUse {
            path: ParamPath("/tags".to_string()),
            binding: "host.tags".parse().unwrap(),
            value: tags,
        })
    );
    assert!(written
        .bindings
        .of(&tags_node)
        .iter()
        .all(|found| found.binding.as_str() != "host.tags"));
    let gateway = NodeId::reserved("af_metrics_gateway".parse().unwrap());
    assert_eq!(
        program
            .bindings
            .of(&gateway)
            .iter()
            .map(|found| (found.path.0.as_str(), found.binding.as_str()))
            .collect::<Vec<_>>(),
        [("/predicate/all/1/flag", "autoscaling_failover.enabled")]
    );
}

#[test]
fn a_parameter_left_out_and_its_default_elaborate_to_one_hash() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let written = exported(&plan, &catalog);
    let program = elaborated(&plan, &catalog, written.clone());

    // Every parameter the export writes at its default, left out.
    let mut left_out = written;
    let mut removed = 0;
    for node in left_out["nodes"].as_array_mut().unwrap() {
        let Some(def) = registry.get(node["op"].as_str().unwrap()) else {
            continue;
        };
        let Some(params) = node.get_mut("params").and_then(Value::as_object_mut) else {
            continue;
        };
        for spec in def.params() {
            let default = spec
                .default
                .map(|text| serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string())));
            if default.is_some() && params.get(spec.name) == default.as_ref() {
                params.remove(spec.name);
                removed += 1;
            }
        }
    }
    assert!(removed > 0, "the export writes defaults");
    let elaborated = elaborated(&plan, &catalog, left_out);
    assert_eq!(elaborated.plan.nodes, program.plan.nodes);
    assert_eq!(elaborated.hash().unwrap(), program.hash().unwrap());
}

#[test]
fn reordered_set_literals_elaborate_to_one_hash() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let mut first = plan.clone();
    let mut second = plan.clone();
    // A mapper with two tags in its one mapping, written in both orders: a set's elements are in no order.
    let mapper = first
        .nodes
        .iter()
        .position(|node| node.op.as_str() == names::DOGSTATSD_MAPPER)
        .expect("the default program has a mapper");
    let profiles = |tags: Value| {
        json!([{"name": "p", "prefix": "p.", "mappings": [
            {"match": "p.*", "match_type": "wildcard", "name": "q", "tags": tags}]}])
    };
    let name = "profiles".parse::<ParamName>().unwrap();
    first.nodes[mapper].params.insert(
        name.clone(),
        profiles(json!([{"name": "a", "value": "1"}, {"name": "b", "value": "2"}])),
    );
    second.nodes[mapper].params.insert(
        name,
        profiles(json!([{"name": "b", "value": "2"}, {"name": "a", "value": "1"}])),
    );
    let (first, second) = (
        elaborate_default(&first, &catalog, &registry),
        elaborate_default(&second, &catalog, &registry),
    );
    assert!(first.report.is_valid(), "{}", first.report);
    assert_eq!(first.hash().unwrap(), second.hash().unwrap());
}

#[test]
fn a_signal_change_changes_the_hash_and_only_that_nodes_kernel_key() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let mut off = plan.clone();
    let gateway = off
        .nodes
        .iter()
        .position(|node| node.id.local().as_str() == "af_metrics_gateway")
        .unwrap();
    let predicate = "predicate".parse::<ParamName>().unwrap();
    let mut changed = off.nodes[gateway].params.get(predicate.as_str()).unwrap().clone();
    for operand in changed["all"].as_array_mut().unwrap() {
        if operand["flag"]["config"] == "autoscaling_failover.enabled" {
            operand["flag"]["resolved"] = json!(false);
        }
    }
    off.nodes[gateway].params.insert(predicate, changed);

    let (on, off) = (
        elaborate_default(&plan, &catalog, &registry).plan,
        elaborate_default(&off, &catalog, &registry).plan,
    );
    assert_ne!(on.hash().unwrap(), off.hash().unwrap());
    let keys = |plan: &Plan| {
        plan.nodes
            .iter()
            .map(|node| (node.id.clone(), KernelKey::of(node).unwrap()))
            .collect::<BTreeMap<_, _>>()
    };
    let (on, off) = (keys(&on), keys(&off));
    let differing = on
        .iter()
        .filter(|(node, key)| off.get(*node) != Some(*key))
        .map(|(node, _)| node.local().to_string())
        .collect::<Vec<_>>();
    assert_eq!(differing, ["af_metrics_gateway"]);
}

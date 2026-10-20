//! Composition when outputs change under an artifact: each case of [the module
//! documentation](super#outputs-that-change-under-an-artifact), for a new artifact and for the artifact in effect.
//!
//! The default programs are small and written by hand: a source whose records the program copies to one `emit` per
//! output. Expected errors, outcomes, and notes are written by hand.

use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::{
    export::export,
    model::PortType,
    validate::{CatalogOutput, MemoryCatalog},
};

/// A default program delivering every record to each of `outputs`, and the host's catalog of it.
fn program(outputs: &[&str]) -> (Plan, MemoryCatalog) {
    let mut nodes = vec![json!({"id": "src", "op": "source", "params": {"binding": "otlp"}})];
    let mut edges = Vec::new();
    if let [only] = outputs {
        nodes.push(json!({"id": format!("emit_{}", only), "op": "emit", "params": {"output": only}}));
        edges.push(json!({"from": "src.metrics", "to": format!("emit_{}.in", only)}));
    } else {
        nodes.push(json!({"id": "fanout", "op": "tee", "params": {"ports": outputs.len()}}));
        edges.push(json!({"from": "src.metrics", "to": "fanout.in"}));
        for (port, output) in outputs.iter().enumerate() {
            nodes.push(json!({"id": format!("emit_{}", output), "op": "emit", "params": {"output": output}}));
            edges.push(json!({"from": format!("fanout.{}", port), "to": format!("emit_{}.in", output)}));
        }
    }
    let declared = outputs
        .iter()
        .map(|output| json!({"id": output, "accepts": PortType::ANY}))
        .collect::<Vec<_>>();
    let plan = Plan::from_value(json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": declared})).unwrap();
    let mut catalog = MemoryCatalog::new();
    for output in &plan.outputs {
        catalog = catalog.with_output(
            output.id.clone(),
            CatalogOutput {
                accepts: output.accepts,
                recipients: output.recipients.clone(),
            },
        );
    }
    (plan, catalog)
}

/// The export of a default program, as the artifact an author wrote against it.
fn artifact_of(plan: &Plan, catalog: &MemoryCatalog) -> ReceivedArtifact {
    let export = export(plan, catalog, &BTreeMap::new()).unwrap();
    receive_artifact(serde_json::to_value(&export.artifact).unwrap()).unwrap()
}

fn compose_into(
    artifact: &ReceivedArtifact, (plan, catalog): &(Plan, MemoryCatalog), in_effect: bool,
) -> Result<Composed, ComposeError> {
    let approval = Approval {
        revision: 7,
        approved_against: None,
        active_plan_hash: None,
        in_effect,
    };
    compose(plan, artifact, &approval, catalog, &OperatorRegistry::native())
}

fn output(id: &str) -> OutputId {
    id.parse().unwrap()
}

fn node_id(id: &str) -> NodeId {
    id.parse().unwrap()
}

/// The artifact written against outputs `a` and `b`, which it keeps as the default program has them.
fn written() -> ReceivedArtifact {
    let (plan, catalog) = program(&["a", "b"]);
    artifact_of(&plan, &catalog)
}

#[test]
fn a_declared_output_composes_whatever_the_artifacts_state() {
    let artifact = written();
    for in_effect in [false, true] {
        let composed = compose_into(&artifact, &program(&["a", "b"]), in_effect).unwrap();
        assert!(composed.report.is_valid(), "{}", composed.report);
        assert_eq!(composed.outcome, ComposeOutcome::Composed);
        assert_eq!(composed.notes(), None);
    }
}

#[test]
fn a_new_program_to_an_undeclared_output_is_refused_and_the_one_in_effect_drops_its_emits_until_it_returns() {
    let artifact = written();
    let without_b = program(&["a"]);
    let error = compose_into(&artifact, &without_b, false).unwrap_err();
    assert_eq!(
        error,
        ComposeError::UndeclaredOutput {
            node: "emit_b".parse().unwrap(),
            output: output("b"),
            declared: vec![output("a")],
        }
    );
    assert_eq!(
        error.to_string(),
        "Artifact node `emit_b` emits to output `b`, which the host does not declare. Emit to one of the outputs it \
         declares: `a`."
    );

    let composed = compose_into(&artifact, &without_b, true).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(
        composed.outcome,
        ComposeOutcome::Degraded(vec![DroppedEmits {
            target: DropTarget::Output(output("b")),
            reason: DropReason::Removed,
            // The default program no longer has the node, so the artifact's is a node of its own.
            nodes: vec![node_id("emit_b")],
        }])
    );
    assert_eq!(
        composed.notes().as_deref(),
        Some("output `b` is no longer declared; its emits are dropped until it returns")
    );
    assert_eq!(composed.plan.node(&node_id("emit_b")).unwrap().op.as_str(), names::DROP);

    // The output returns: the next composition restores the emit, as written.
    let restored = compose_into(&artifact, &program(&["a", "b"]), true).unwrap();
    assert_eq!(restored.outcome, ComposeOutcome::Composed);
    let emit = restored.plan.node(&node_id("emit_b")).unwrap();
    assert_eq!(emit.op.as_str(), names::EMIT);
    assert_eq!(emit.params.get("output"), Some(&json!("b")));
}

//! Composition when outputs change under an artifact: each row of the table in [the module
//! documentation](super#outputs-that-change-under-an-artifact), for a new artifact and for the artifact in effect.
//!
//! The default programs are small and written by hand: a source whose records the policy scope copies to one `emit`
//! per output, with fingerprints written as repeated digits (a declared output's, and a routing group's, is the digest
//! of its binding). Expected errors, outcomes, and notes are written by hand.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::*;
use crate::{
    export::export,
    install::correspondence::CorrespondenceKind,
    model::PortType,
    validate::{CatalogOutput, MemoryCatalog},
};

const SCOPE: &str = "post_enrich";

fn hash(digit: char) -> PlanHash {
    format!("sha256:{}", digit.to_string().repeat(64)).parse().unwrap()
}

/// The version 2 fingerprint of an output bound to `dd_out` with `selector`.
fn bound_fingerprint(selector: &Value) -> PlanHash {
    let host = crate::ir::HostId::new("dd_out").unwrap();
    crate::resources::fingerprint(&host, PortType::ANY, &serde_json::from_value(selector.clone()).unwrap())
}

/// The fingerprint the declaration of output `id` with digit `digit` has ([`declaration`]).
fn fingerprint_of(id: &str, digit: char) -> PlanHash {
    declaration(id, digit).0["fingerprint"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// The declaration of output `id` with digit `digit`, and of the recipient it declares, if any:
///
/// - an output named `metrics_routing_encode_<i>` is a routing group's: it is bound to `dd_out` with the group key the
///   digit gives, and its fingerprint is the version 2 digest of that binding;
/// - any other output is unbound, with the digit as its fingerprint.
fn declaration(id: &str, digit: char) -> (Value, Option<Value>) {
    if id.starts_with("metrics_routing_encode_") {
        let selector = json!({"group": hash(digit)});
        let output = json!({
            "id": id, "accepts": PortType::ANY, "fingerprint": bound_fingerprint(&selector),
            "binding": {"host": "dd_out", "selector": selector, "telemetry": id},
        });
        return (output, None);
    }
    (
        json!({"id": id, "accepts": PortType::ANY, "fingerprint": hash(digit)}),
        None,
    )
}

/// A default program delivering every record to each of `outputs`, each `(output, fingerprint digit)`, and the host's
/// catalog of it.
fn program(outputs: &[(&str, char)]) -> (Plan, MemoryCatalog) {
    let mut nodes = vec![json!({"id": "src", "op": "source", "params": {"binding": "otlp"}})];
    let mut edges = Vec::new();
    if let [(only, _)] = outputs {
        nodes.push(json!({"id": format!("emit_{}", only), "op": "emit", "params": {"output": only}}));
        edges.push(json!({"from": "src.metrics", "to": format!("emit_{}.in", only)}));
    } else {
        nodes.push(json!({"id": "fanout", "op": "tee", "params": {"ports": outputs.len()}}));
        edges.push(json!({"from": "src.metrics", "to": "fanout.in"}));
        for (port, (output, _)) in outputs.iter().enumerate() {
            nodes.push(json!({"id": format!("emit_{}", output), "op": "emit", "params": {"output": output}}));
            edges.push(json!({"from": format!("fanout.{}", port), "to": format!("emit_{}.in", output)}));
        }
    }
    let (declared, recipients): (Vec<_>, Vec<_>) = outputs
        .iter()
        .map(|(output, digit)| declaration(output, *digit))
        .unzip();
    let mut recipients = recipients.into_iter().flatten().collect::<Vec<_>>();
    recipients.dedup();
    let plan = Plan::from_value(json!({
        "ir_version": 1, "nodes": nodes, "edges": edges, "outputs": declared,
        "scopes": {SCOPE: {"inputs": {"metrics": "src.metrics"}}},
        "resources": {"recipients": recipients},
    }))
    .unwrap();
    let mut catalog =
        MemoryCatalog::new().with_scope_input(SCOPE.parse().unwrap(), "metrics".parse().unwrap(), PortType::ANY);
    for output in &plan.outputs {
        catalog = catalog.with_output(
            output.id.clone(),
            CatalogOutput {
                accepts: output.accepts,
                fingerprint: output.fingerprint,
                recipients: output.recipients.clone(),
            },
        );
    }
    (plan, catalog)
}

/// The export of the policy scope of a default program, as the artifact an author wrote against it.
fn artifact_of(plan: &Plan, catalog: &MemoryCatalog) -> ReceivedArtifact {
    let export = export(plan, &SCOPE.parse().unwrap(), catalog, &BTreeMap::new()).unwrap();
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

fn scope(composed: &Composed) -> &ScopeComposition {
    &composed.scopes[&SCOPE.parse::<ScopeName>().unwrap()]
}

fn output(id: &str) -> OutputId {
    id.parse().unwrap()
}

fn node_id(id: &str) -> NodeId {
    id.parse().unwrap()
}

/// The artifact written against outputs `a` and `b`, which it keeps as the default program has them.
fn written() -> ReceivedArtifact {
    let (plan, catalog) = program(&[("a", '1'), ("b", '2')]);
    artifact_of(&plan, &catalog)
}

#[test]
fn a_recorded_output_declared_with_the_recorded_fingerprint_composes_whatever_the_artifacts_state() {
    let artifact = written();
    for in_effect in [false, true] {
        let composed = compose_into(&artifact, &program(&[("a", '1'), ("b", '2')]), in_effect).unwrap();
        assert!(composed.report.is_valid(), "{}", composed.report);
        assert_eq!(scope(&composed).outcome, ComposeOutcome::Composed);
        assert_eq!(scope(&composed).notes(), None);
    }
}

#[test]
fn a_fingerprint_an_earlier_fingerprint_version_computed_is_an_output_that_delivers_elsewhere() {
    // An artifact kept from before fingerprint version 2 records output `a` under its version 1 digest, `1`; the host
    // now declares `a` with its version 2 fingerprint, `7`, and computes no version 1 digest. A new artifact is refused
    // with the advice to rebase, and the artifact in effect drops its emits to `a` until it is republished.
    let artifact = written();
    let host = program(&[("a", '7'), ("b", '2')]);
    let error = compose_into(&artifact, &host, false).unwrap_err();
    assert_eq!(
        error,
        ComposeError::ReboundOutput {
            output: output("a"),
            renumbered: None
        }
    );
    assert!(
        error
            .to_string()
            .ends_with("Rebase the artifact on the current export."),
        "{}",
        error
    );
    let composed = compose_into(&artifact, &host, true).unwrap();
    assert_eq!(
        scope(&composed).notes().as_deref(),
        Some("output `a` now delivers elsewhere; its emits are dropped; rebase")
    );
}

#[test]
fn a_rebound_output_rejects_a_new_artifact_and_drops_the_emits_of_the_one_in_effect() {
    let artifact = written();
    let host = program(&[("a", '1'), ("b", '9')]);
    let error = compose_into(&artifact, &host, false).unwrap_err();
    assert_eq!(
        error,
        ComposeError::ReboundOutput {
            output: output("b"),
            renumbered: None
        }
    );
    assert_eq!(
        error.to_string(),
        "The artifact emits to output `b`, which now delivers elsewhere than the artifact's base records. Rebase the \
         artifact on the current export."
    );

    let composed = compose_into(&artifact, &host, true).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    let scope = scope(&composed);
    assert_eq!(
        scope.outcome,
        ComposeOutcome::Degraded(vec![DroppedEmits {
            target: DropTarget::Output(output("b")),
            reason: DropReason::Rebound { renumbered: None },
            nodes: vec![node_id("emit_b")],
        }])
    );
    assert!(scope.outcome.is_degraded());
    assert_eq!(
        scope.notes().as_deref(),
        Some("output `b` now delivers elsewhere; its emits are dropped; rebase")
    );
    // The `emit` keeps its node ID and becomes a drop, so its loss is the program's policy loss there; the emit to `a`
    // is the default program's, unchanged.
    let dropped = composed.plan.node(&node_id("emit_b")).unwrap();
    assert_eq!(dropped.op.as_str(), names::DROP);
    assert!(dropped.params.is_empty());
    assert_eq!(composed.plan.provenance.as_ref().unwrap().artifacts[0].revision, 7);
    assert_eq!(composed.plan.node(&node_id("emit_a")), host.0.node(&node_id("emit_a")));
    assert_eq!(
        scope
            .correspondence
            .get(&"emit_b".parse().unwrap())
            .map(|found| found.kind),
        Some(CorrespondenceKind::Modified)
    );
}

#[test]
fn a_removed_output_holds_a_new_artifact_pending_and_drops_the_emits_of_the_one_in_effect_until_it_returns() {
    let artifact = written();
    let without_b = program(&[("a", '1')]);
    let error = compose_into(&artifact, &without_b, false).unwrap_err();
    assert_eq!(error, ComposeError::PendingOutput { output: output("b") });
    assert!(error.to_string().starts_with("Waiting for output `b`:"), "{}", error);

    let composed = compose_into(&artifact, &without_b, true).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(
        scope(&composed).outcome,
        ComposeOutcome::Degraded(vec![DroppedEmits {
            target: DropTarget::Output(output("b")),
            reason: DropReason::Removed,
            // The default program no longer has the node, so the artifact's is placed in the policy namespace.
            nodes: vec![node_id("policy/emit_b")],
        }])
    );
    assert_eq!(
        scope(&composed).notes().as_deref(),
        Some("output `b` is no longer declared; its emits are dropped until it returns")
    );
    assert_eq!(
        composed.plan.node(&node_id("policy/emit_b")).unwrap().op.as_str(),
        names::DROP
    );

    // The output returns with the recorded fingerprint: the next composition restores the emit, as written.
    let restored = compose_into(&artifact, &program(&[("a", '1'), ("b", '2')]), true).unwrap();
    assert_eq!(scope(&restored).outcome, ComposeOutcome::Composed);
    let emit = restored.plan.node(&node_id("emit_b")).unwrap();
    assert_eq!(emit.op.as_str(), names::EMIT);
    assert_eq!(emit.params.get("output"), Some(&json!("b")));
}

#[test]
fn a_renumbered_routing_output_names_the_output_that_now_carries_the_recorded_fingerprint() {
    // Two groups, keys `3` and `4`. Group 0 goes, and group 1 becomes index 0.
    let (plan, catalog) = program(&[("metrics_routing_encode_0", '3'), ("metrics_routing_encode_1", '4')]);
    let artifact = artifact_of(&plan, &catalog);
    let host = program(&[("metrics_routing_encode_0", '4')]);

    // A new artifact is rejected, the first output it emits to now delivering elsewhere.
    assert_eq!(
        compose_into(&artifact, &host, false).unwrap_err(),
        ComposeError::ReboundOutput {
            output: output("metrics_routing_encode_0"),
            renumbered: None,
        }
    );

    // The artifact in effect drops both emits: group 0's output now delivers to group 1's endpoints, and group 1's
    // output is gone, its recorded group now at index 0.
    let composed = compose_into(&artifact, &host, true).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(
        scope(&composed).notes().as_deref(),
        Some(
            "output `metrics_routing_encode_0` now delivers elsewhere; its emits are dropped; rebase; output \
             `metrics_routing_encode_1` renumbered: the recorded output is now `metrics_routing_encode_0`; its emits \
             are dropped; rebase"
        )
    );

    // An artifact that emits to group 1 only: rejected, not pending, since waiting would never end, and the message
    // names where the recorded group went.
    let mut document = serde_json::to_value(&artifact.artifact).unwrap();
    document["nodes"] = json!([{"id": "emit_metrics_routing_encode_1", "op": "emit",
                                "params": {"output": "metrics_routing_encode_1"}}]);
    document["edges"] = json!([{"from": "input.metrics", "to": "emit_metrics_routing_encode_1.in"}]);
    document["base"]["outputs"] = json!({"metrics_routing_encode_1": fingerprint_of("metrics_routing_encode_1", '4')});
    let only_one = receive_artifact(document).unwrap();
    let error = compose_into(&only_one, &host, false).unwrap_err();
    assert_eq!(
        error,
        ComposeError::ReboundOutput {
            output: output("metrics_routing_encode_1"),
            renumbered: Some(output("metrics_routing_encode_0")),
        }
    );
    assert!(
        error
            .to_string()
            .contains(": the outputs were renumbered, and the recorded one is now `metrics_routing_encode_0`"),
        "{}",
        error
    );
}

#[test]
fn a_new_artifact_waits_for_an_output_only_when_nothing_else_refuses_it() {
    // The artifact emits to `b` first, then to `a`.
    let (plan, catalog) = program(&[("b", '2'), ("a", '1')]);
    let artifact = artifact_of(&plan, &catalog);
    // Output `b` removed and `a` rebound: the rebinding, which a declaration cannot cure, is what is reported, though
    // the removed output's emit comes first.
    let host = program(&[("a", '9')]);
    assert_eq!(
        compose_into(&artifact, &host, false).unwrap_err(),
        ComposeError::ReboundOutput {
            output: output("a"),
            renumbered: None
        }
    );
}

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
/// - an output named `resources_<name>` is one a resources artifact declares: it is bound to `dd_out` with one declared
///   recipient, which the digit chooses (outputs with one digit name the same recipient), and its fingerprint is the
///   version 2 digest of that binding;
/// - an output named `metrics_routing_encode_<i>` is a routing group's: it is bound to `dd_out` with the group key the
///   digit gives, and its fingerprint is the version 2 digest of that binding;
/// - any other output is unbound, with the digit as its fingerprint.
fn declaration(id: &str, digit: char) -> (Value, Option<Value>) {
    if id.starts_with("metrics_routing_encode_") {
        let selector = json!({"group": hash(digit)});
        let output = json!({
            "id": id, "accepts": PortType::ANY, "fingerprint": bound_fingerprint(&selector),
            "binding": {"host": "dd_out", "selector": selector, "telemetry": id, "provenance": "native"},
        });
        return (output, None);
    }
    if OutputId::new(id).unwrap().declared_name().is_none() {
        return (
            json!({"id": id, "accepts": PortType::ANY, "fingerprint": hash(digit)}),
            None,
        );
    }
    let name = format!("team_{}", digit);
    let recipient = format!("dd_out/declared/{}/https://{}.example.com", name, name);
    let selector = json!({"declared": [recipient]});
    let provenance = json!({"declared": {"revision": 1, "hash": hash('c')}});
    let output = json!({
        "id": id, "accepts": PortType::ANY, "fingerprint": bound_fingerprint(&selector), "recipients": [recipient],
        "binding": {"host": "dd_out", "selector": selector, "telemetry": id, "provenance": provenance},
    });
    let declared = json!({
        "id": recipient, "host": "dd_out", "endpoint": format!("https://{}.example.com", name),
        "credential": format!("credentials/{}", name), "classes": "metrics_only", "provenance": provenance,
    });
    (output, Some(declared))
}

/// A default program delivering every record to each of `outputs`, each `(output, fingerprint digit)`, and the host's
/// catalog of it, whose outputs have the version 1 fingerprints `legacy` gives.
fn program(outputs: &[(&str, char)], legacy: &[(&str, char)]) -> (Plan, MemoryCatalog) {
    let mut nodes = vec![json!({"id": "src", "op": "source", "params": {"binding": "otlp"},
                                "provenance": {"native": "src"}})];
    let mut edges = Vec::new();
    if let [(only, _)] = outputs {
        nodes.push(json!({"id": format!("emit_{}", only), "op": "emit", "params": {"output": only}}));
        edges.push(json!({"from": "src.metrics", "to": format!("emit_{}.in", only)}));
    } else {
        nodes.push(json!({"id": "fanout", "op": "tee", "params": {"ports": outputs.len()},
                          "provenance": {"native": "fanout"}}));
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
        let legacy = legacy
            .iter()
            .find(|(id, _)| *id == output.id.as_str())
            .map(|(_, digit)| hash(*digit));
        catalog = catalog.with_output(
            output.id.clone(),
            CatalogOutput {
                accepts: output.accepts,
                fingerprint: output.fingerprint,
                recipients: output.recipients.clone(),
                legacy_fingerprint: legacy,
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
    artifact: &ReceivedArtifact, host: &(Plan, MemoryCatalog), in_effect: bool,
) -> Result<Composed, ComposeError> {
    compose_upgraded(artifact, host, in_effect, &BTreeMap::new())
}

/// Composes as [`compose_into`] does, with the version 2 fingerprints earlier compositions kept for the artifact's
/// version 1 base.
fn compose_upgraded(
    artifact: &ReceivedArtifact, (plan, catalog): &(Plan, MemoryCatalog), in_effect: bool,
    upgraded: &BTreeMap<OutputId, PlanHash>,
) -> Result<Composed, ComposeError> {
    let approval = Approval {
        revision: 7,
        approved_against: None,
        active_plan_hash: None,
        in_effect,
        upgraded: upgraded.clone(),
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
    let (plan, catalog) = program(&[("a", '1'), ("b", '2')], &[]);
    artifact_of(&plan, &catalog)
}

#[test]
fn a_recorded_output_declared_with_the_recorded_fingerprint_composes_whatever_the_artifacts_state() {
    let artifact = written();
    for in_effect in [false, true] {
        let composed = compose_into(&artifact, &program(&[("a", '1'), ("b", '2')], &[]), in_effect).unwrap();
        assert!(composed.report.is_valid(), "{}", composed.report);
        assert_eq!(scope(&composed).outcome, ComposeOutcome::Composed);
        assert_eq!(scope(&composed).notes(), None);
    }
}

#[test]
fn a_recorded_version_1_fingerprint_that_still_matches_composes_noted_for_a_rebase() {
    let artifact = written();
    // Output `a` now has the version 2 fingerprint `7`, and its version 1 fingerprint is the one recorded.
    for in_effect in [false, true] {
        let host = program(&[("a", '7'), ("b", '2')], &[("a", '1')]);
        let composed = compose_into(&artifact, &host, in_effect).unwrap();
        assert!(composed.report.is_valid(), "{}", composed.report);
        let scope = scope(&composed);
        assert_eq!(scope.outcome, ComposeOutcome::Composed);
        // Reported with the version 2 fingerprint the output has now, for the installer to keep.
        assert_eq!(scope.legacy_outputs, BTreeMap::from([(output("a"), hash('7'))]));
        assert_eq!(
            scope.notes().as_deref(),
            Some(
                "the base records `a` with version 1 fingerprints, which still match; rebase to fingerprint version 2"
            )
        );
    }
    // A version 1 fingerprint that no longer matches has the version 1 meaning: the output delivers elsewhere.
    let host = program(&[("a", '7'), ("b", '2')], &[("a", '8')]);
    assert_eq!(
        compose_into(&artifact, &host, false).unwrap_err(),
        ComposeError::ReboundOutput {
            output: output("a"),
            renumbered: None
        }
    );
}

#[test]
fn a_version_1_base_that_composed_follows_version_2_so_a_dual_ship_endpoint_added_under_a_role_drops_nothing() {
    let artifact = written();
    // At the upgrade, output `a` (the main output, say) has the version 2 fingerprint `7`, and the recorded version
    // 1 fingerprint `1` still matches: the artifact composes, and the installer keeps `a`'s version 2 fingerprint.
    let upgrade = program(&[("a", '7'), ("b", '2')], &[("a", '1')]);
    let composed = compose_into(&artifact, &upgrade, false).unwrap();
    let kept = scope(&composed).legacy_outputs.clone();
    assert_eq!(kept, BTreeMap::from([(output("a"), hash('7'))]));

    // A dual-ship endpoint is added under `a`'s role: its version 1 fingerprint, which covers its resolved recipients,
    // changes to `8`, and its version 2 fingerprint does not. Without what the installer kept, the artifact in effect
    // would drop every emit to `a`; with it, the artifact composes as written, still noted for a rebase, new or in
    // effect.
    let dual_ship = program(&[("a", '7'), ("b", '2')], &[("a", '8')]);
    assert!(compose_into(&artifact, &dual_ship, true)
        .unwrap()
        .scopes
        .values()
        .all(|scope| scope.outcome.is_degraded()));
    for in_effect in [false, true] {
        let composed = compose_upgraded(&artifact, &dual_ship, in_effect, &kept).unwrap();
        assert!(composed.report.is_valid(), "{}", composed.report);
        let scope = scope(&composed);
        assert_eq!(scope.outcome, ComposeOutcome::Composed);
        assert_eq!(scope.legacy_outputs, kept);
        assert_eq!(
            scope.notes().as_deref(),
            Some(
                "the base records `a` with version 1 fingerprints, which still match; rebase to fingerprint version 2"
            )
        );
        assert_eq!(
            composed.plan.node(&node_id("emit_a")).map(|node| node.op.as_str()),
            Some(names::EMIT)
        );
    }

    // From then on the version 2 meaning holds: `a` declared with another version 2 fingerprint now delivers elsewhere,
    // even with the recorded version 1 fingerprint matching again.
    let rebound = program(&[("a", '6'), ("b", '2')], &[("a", '1')]);
    assert_eq!(
        compose_upgraded(&artifact, &rebound, false, &kept).unwrap_err(),
        ComposeError::ReboundOutput {
            output: output("a"),
            renumbered: None
        }
    );
    assert_eq!(
        scope(&compose_upgraded(&artifact, &rebound, true, &kept).unwrap()).outcome,
        ComposeOutcome::Degraded(vec![DroppedEmits {
            output: output("a"),
            reason: DropReason::Rebound { renumbered: None },
            nodes: vec![node_id("emit_a")],
        }])
    );
}

#[test]
fn a_rebound_output_rejects_a_new_artifact_and_drops_the_emits_of_the_one_in_effect() {
    let artifact = written();
    let host = program(&[("a", '1'), ("b", '9')], &[]);
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
            output: output("b"),
            reason: DropReason::Rebound { renumbered: None },
            nodes: vec![node_id("emit_b")],
        }])
    );
    assert!(scope.outcome.is_degraded());
    assert_eq!(
        scope.notes().as_deref(),
        Some("output `b` now delivers elsewhere; its emits are dropped; rebase")
    );
    // The `emit` keeps its node ID and becomes a drop of the artifact's, so its loss is the artifact's policy loss
    // there; the emit to `a` is the default program's, unchanged.
    let dropped = composed.plan.node(&node_id("emit_b")).unwrap();
    assert_eq!(dropped.op.as_str(), names::DROP);
    assert!(dropped.params.is_empty());
    assert!(matches!(&dropped.provenance, Some(Provenance::Artifact(revision)) if revision.revision == 7));
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
    let without_b = program(&[("a", '1')], &[]);
    let error = compose_into(&artifact, &without_b, false).unwrap_err();
    assert_eq!(error, ComposeError::PendingOutput { output: output("b") });
    assert!(error.to_string().starts_with("Waiting for output `b`:"), "{}", error);

    let composed = compose_into(&artifact, &without_b, true).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(
        scope(&composed).outcome,
        ComposeOutcome::Degraded(vec![DroppedEmits {
            output: output("b"),
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
    let restored = compose_into(&artifact, &program(&[("a", '1'), ("b", '2')], &[]), true).unwrap();
    assert_eq!(scope(&restored).outcome, ComposeOutcome::Composed);
    let emit = restored.plan.node(&node_id("emit_b")).unwrap();
    assert_eq!(emit.op.as_str(), names::EMIT);
    assert_eq!(emit.params.get("output"), Some(&json!("b")));
}

#[test]
fn a_renumbered_routing_output_names_the_output_that_now_carries_the_recorded_fingerprint() {
    // Two groups, keys `3` and `4`. Group 0 goes, and group 1 becomes index 0.
    let (plan, catalog) = program(
        &[("metrics_routing_encode_0", '3'), ("metrics_routing_encode_1", '4')],
        &[],
    );
    let artifact = artifact_of(&plan, &catalog);
    let host = program(&[("metrics_routing_encode_0", '4')], &[]);

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
fn a_policy_routed_to_a_declared_output_waits_for_it_in_either_product_order() {
    // The author records the declared output's fingerprint, as the editor computes it.
    let declared = ("resources_team_d", '5');
    let (plan, catalog) = program(&[("a", '1'), declared], &[]);
    let artifact = artifact_of(&plan, &catalog);

    // Policy first: the resources artifact that declares the output has not arrived, so the policy waits.
    let before = program(&[("a", '1')], &[]);
    assert_eq!(
        compose_into(&artifact, &before, false).unwrap_err(),
        ComposeError::PendingOutput {
            output: output("resources_team_d")
        }
    );
    // The resources arrive, and the next composition installs it; resources first composes at once the same way.
    let after = program(&[("a", '1'), declared], &[]);
    let composed = compose_into(&artifact, &after, false).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(scope(&composed).outcome, ComposeOutcome::Composed);

    // Revoking the resources while the policy is in effect drops its emits to the declared output, and keeps the rest
    // of what it does.
    let revoked = compose_into(&artifact, &before, true).unwrap();
    assert!(revoked.report.is_valid(), "{}", revoked.report);
    assert_eq!(
        scope(&revoked).notes().as_deref(),
        Some("output `resources_team_d` is no longer declared; its emits are dropped until it returns")
    );
    assert_eq!(
        revoked.plan.node(&node_id("emit_a")).map(|node| node.op.as_str()),
        Some(names::EMIT)
    );
}

#[test]
fn a_policy_waiting_for_a_declared_output_is_pending_while_another_declared_output_has_the_same_recipients() {
    // Resources revision 2 is to declare team E's output with the recipient team D's has (digit `5`), so the editor
    // recorded the fingerprint team D's output carries: a declared output's covers its recipients, not its name.
    let (plan, catalog) = program(&[("a", '1'), ("resources_team_e", '5')], &[]);
    let artifact = artifact_of(&plan, &catalog);
    assert_eq!(
        fingerprint_of("resources_team_e", '5'),
        fingerprint_of("resources_team_d", '5')
    );

    // Revision 1 declares only team D's: team E's is not renumbered, only not declared yet, and the policy waits.
    let revision_1 = program(&[("a", '1'), ("resources_team_d", '5')], &[]);
    assert_eq!(
        compose_into(&artifact, &revision_1, false).unwrap_err(),
        ComposeError::PendingOutput {
            output: output("resources_team_e")
        }
    );
    let composed = compose_into(&artifact, &revision_1, true).unwrap();
    assert_eq!(
        scope(&composed).notes().as_deref(),
        Some("output `resources_team_e` is no longer declared; its emits are dropped until it returns")
    );

    // Revision 2 cures it.
    let revision_2 = program(&[("a", '1'), ("resources_team_d", '5'), ("resources_team_e", '5')], &[]);
    let composed = compose_into(&artifact, &revision_2, false).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(scope(&composed).outcome, ComposeOutcome::Composed);
}

#[test]
fn a_new_artifact_waits_for_an_output_only_when_nothing_else_refuses_it() {
    // The artifact emits to `b` first, then to `a`.
    let (plan, catalog) = program(&[("b", '2'), ("a", '1')], &[]);
    let artifact = artifact_of(&plan, &catalog);
    // Output `b` removed and `a` rebound: the rebinding, which a declaration cannot cure, is what is reported, though
    // the removed output's emit comes first.
    let host = program(&[("a", '9')], &[]);
    assert_eq!(
        compose_into(&artifact, &host, false).unwrap_err(),
        ComposeError::ReboundOutput {
            output: output("a"),
            renumbered: None
        }
    );
}

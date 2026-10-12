//! Hand-written diffs of resources: the bound default program (`fixtures/default-program-resources.json`), edited.

use serde_json::{json, Value};

use super::*;
use crate::testing::{with_resources, DEFAULT_PROGRAM_JSON};

/// The bound default program, edited.
fn bound(edit: impl FnOnce(&mut Value)) -> Plan {
    let mut value = with_resources(DEFAULT_PROGRAM_JSON);
    edit(&mut value);
    Plan::from_value(value).unwrap()
}

fn recipient_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["resources"]["recipients"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|recipient| recipient["id"] == id)
        .unwrap()
}

type Changes = (
    Vec<(String, OutputChange)>,
    Vec<(String, ResourceChange)>,
    Vec<(String, ResourceChange)>,
);

/// Returns every output, recipient, and host whose change is not `reused`.
fn changes(diff: &PlanDiff) -> Changes {
    let outputs = diff
        .outputs
        .iter()
        .filter(|output| output.change != OutputChange::Reused)
        .map(|output| (output.id.to_string(), output.change))
        .collect();
    let recipients = diff
        .recipients
        .iter()
        .filter(|recipient| recipient.change != ResourceChange::Reused)
        .map(|recipient| (recipient.id.to_string(), recipient.change))
        .collect();
    let hosts = diff
        .hosts
        .iter()
        .filter(|host| host.change != ResourceChange::Reused)
        .map(|host| (host.host.to_string(), host.change))
        .collect();
    (outputs, recipients, hosts)
}

const TEAM_D: &str = "dd_out/declared/team_d/http://127.0.0.1:17105";
const TEAM_E: &str = "dd_out/declared/team_e/https://e.example.com";

#[test]
fn a_resources_revision_declaring_the_same_resources_changes_nothing_but_provenance() {
    let registry = OperatorRegistry::native();
    let active = bound(|_| {});
    assert!(diff(&active, &active, &registry).is_empty());

    let redeclared = bound(|value| {
        let revision = json!({"declared": {"revision": 2, "hash": format!("sha256:{}", "e".repeat(64))}});
        for output in value["outputs"].as_array_mut().unwrap() {
            if output["id"] == "resources_team_d" {
                output["binding"]["provenance"] = revision.clone();
            }
        }
        recipient_mut(value, TEAM_D)["provenance"] = revision;
    });
    let unchanged = diff(&active, &redeclared, &registry);
    assert_eq!(changes(&unchanged), (Vec::new(), Vec::new(), Vec::new()));
    assert!(unchanged.is_empty());
    // The plans differ, so the candidate is a new plan, but its output keeps its encoder stream.
    assert_ne!(
        active.hash(crate::canonical::ResolvedValues::Include).unwrap(),
        redeclared.hash(crate::canonical::ResolvedValues::Include).unwrap()
    );
    let team_d = "resources_team_d".parse().unwrap();
    assert!(active.binding_digest(&team_d).is_some());
    assert_eq!(active.binding_digest(&team_d), redeclared.binding_digest(&team_d));
    assert!(active.binds_alike(&redeclared, &team_d));
    assert_eq!(active.binding_digest(&"absent".parse().unwrap()), None);
}

#[test]
fn a_recipient_whose_protocol_changes_rebinds_every_output_naming_it_and_its_other_settings_rebind_none() {
    let registry = OperatorRegistry::native();
    let active = bound(|_| {});
    let main = "dd_metrics_encode".parse().unwrap();
    let group = "metrics_routing_encode_0".parse().unwrap();

    // The primary intake now takes V3 series. The main output's declaration is unchanged, but the variants its stream
    // resolves the recipient for are not: a new stream, whose payloads name the recipients of their own variant.
    let flipped = bound(|value| recipient_mut(value, "dd_out/primary/0")["protocol"]["v3"]["series"] = json!(true));
    assert_eq!(active.output(&main), flipped.output(&main));
    assert_ne!(active.binding_digest(&main), flipped.binding_digest(&main));
    assert!(!active.binds_alike(&flipped, &main));
    assert_eq!(active.binding_digest(&group), flipped.binding_digest(&group));
    assert_eq!(
        changes(&diff(&active, &flipped, &registry)),
        (
            vec![("dd_metrics_encode".to_string(), OutputChange::Rebound)],
            vec![("dd_out/primary/0".to_string(), ResourceChange::Reconfigured)],
            Vec::new(),
        )
    );

    // A recipient's delivery policy changes how it sends, not which payloads reach it: its output keeps its stream.
    let shrunk = bound(|value| recipient_mut(value, TEAM_D)["delivery"]["bounded"]["queue_bytes"] = json!(1024));
    let team_d = "resources_team_d".parse().unwrap();
    assert_eq!(active.binding_digest(&team_d), shrunk.binding_digest(&team_d));
    assert_eq!(
        changes(&diff(&active, &shrunk, &registry)),
        (
            Vec::new(),
            vec![(TEAM_D.to_string(), ResourceChange::Reconfigured)],
            Vec::new()
        )
    );
}

#[test]
fn recipients_and_host_settings_are_reconfigured_added_or_removed() {
    let registry = OperatorRegistry::native();
    let active = bound(|_| {});
    // Team D's queue shrunk, `mrf_dd_out`'s timeout changed, team E revoked, and a dual-ship recipient added.
    let changed = bound(|value| {
        recipient_mut(value, TEAM_D)["delivery"]["bounded"]["queue_bytes"] = json!(1024);
        value["resources"]["hosts"][1]["settings"]["request_timeout"] = json!("5s");
        value["outputs"]
            .as_array_mut()
            .unwrap()
            .retain(|output| output["id"] != "resources_team_e");
        let recipients = value["resources"]["recipients"].as_array_mut().unwrap();
        recipients.retain(|recipient| recipient["id"] != TEAM_E);
        recipients.push(json!({
            "id": "dd_out/additional_2/0", "host": "dd_out", "endpoint": "https://dual.example.com",
            "credential": "native:additional/0000111122223333/0", "classes": "all", "provenance": "native"
        }));
    });
    let diff = diff(&active, &changed, &registry);
    assert!(!diff.is_empty());
    assert_eq!(
        changes(&diff),
        (
            vec![("resources_team_e".to_string(), OutputChange::Removed)],
            vec![
                (TEAM_D.to_string(), ResourceChange::Reconfigured),
                ("dd_out/additional_2/0".to_string(), ResourceChange::Added),
                (TEAM_E.to_string(), ResourceChange::Removed),
            ],
            vec![("mrf_dd_out".to_string(), ResourceChange::Reconfigured)],
        )
    );
    assert_eq!(ResourceChange::Reconfigured.as_str(), "reconfigured");

    // A rebound output is told apart from a reused one: the main output's encoder setting changed.
    let rebound = bound(|value| {
        for output in value["outputs"].as_array_mut().unwrap() {
            if output["id"] == "dd_metrics_encode" {
                output["binding"]["encoder"]["flush_timeout"] = json!("5s");
            }
        }
    });
    assert_eq!(
        changes(&super::diff(&active, &rebound, &registry)).0,
        vec![("dd_metrics_encode".to_string(), OutputChange::Rebound)]
    );
}

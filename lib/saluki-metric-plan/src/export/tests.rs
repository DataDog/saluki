use serde_json::{json, Value};

use super::*;
use crate::validate::Catalog;
use crate::{
    canonical::ResolvedValues,
    ir::ExitName,
    testing::{
        default_program_catalog, two_scope_catalog, two_scopes_of, with_opaque_gateways, with_two_scopes,
        DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON,
    },
};

/// The export of the default program's `post_enrich` scope, as it declared it through stage 9 ([`with_two_scopes`]),
/// written by hand from `fixtures/default-program.json`, with
/// its program hash computed independently of this crate, with Python: the SHA-256 digest of `json.dumps(document,
/// sort_keys=True, separators=(",", ":"), ensure_ascii=False)` of the document without `base.program_hash`.
const DEFAULT_PROGRAM_EXPORT_JSON: &str = include_str!("../../fixtures/export/default-program-post-enrich.json");

/// The export of the default program's one scope, `metrics`, derived from `fixtures/default-program.json` with Python
/// (every node but the sources, with its bound parameters unresolved and without provenance, and every edge, with each
/// source's port written as the scope's input), with its program hash computed as [`DEFAULT_PROGRAM_EXPORT_JSON`]'s is.
const METRICS_EXPORT_JSON: &str = include_str!("../../fixtures/export/default-program-metrics.json");

fn post_enrich() -> ScopeName {
    "post_enrich".parse().unwrap()
}

/// The default program with the two scopes it declared through stage 9, which most of these tests export: one ends at
/// exits, and each covers part of the plan.
fn default_program() -> Plan {
    two_scopes(DEFAULT_PROGRAM_JSON)
}

fn two_scopes(plan_json: &str) -> Plan {
    Plan::from_value(with_two_scopes(serde_json::from_str(plan_json).unwrap())).unwrap()
}

fn expected_export() -> PolicyArtifact {
    PolicyArtifact::from_json(DEFAULT_PROGRAM_EXPORT_JSON).unwrap()
}

#[test]
fn ir_versions_are_written_as_the_export_bundle_states_them() {
    let versions = IrVersions::supported();
    assert_eq!(versions.current, IR_VERSION);
    assert_eq!(
        serde_json::to_value(versions).unwrap(),
        serde_json::json!({"current": 1, "accepted": [1, 1]})
    );
}

#[test]
fn the_default_programs_policy_scope_exports_as_the_hand_written_artifact() {
    let catalog = two_scope_catalog();
    let labels = BTreeMap::from([("dd_metrics_encode".parse().unwrap(), "Primary".to_string())]);
    let bundle = export(&default_program(), &post_enrich(), &catalog, &labels).unwrap();

    assert_eq!(bundle.artifact, expected_export());
    assert_eq!(bundle.export_version, EXPORT_VERSION);
    assert_eq!(bundle.scope, post_enrich());
    assert_eq!(bundle.ir_versions, IrVersions::supported());
    assert_eq!(bundle.catalog, catalog);
    assert_eq!(bundle.labels, labels);

    // The bundle is written as the protocol describes it, and reads back.
    let document = serde_json::to_value(&bundle).unwrap();
    let members = document.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        members,
        [
            "artifact",
            "catalog",
            "export_version",
            "form",
            "ir_versions",
            "labels",
            "scope"
        ]
    );
    assert_eq!(document["labels"], json!({"dd_metrics_encode": "Primary"}));
    assert_eq!(document["catalog"], serde_json::to_value(&catalog).unwrap());
    assert_eq!(serde_json::from_value::<ExportBundle>(document).unwrap(), bundle);

    // Labels are left out when there are none.
    let unlabeled = export(&default_program(), &post_enrich(), &catalog, &BTreeMap::new()).unwrap();
    assert!(serde_json::to_value(&unlabeled).unwrap().get("labels").is_none());
}

#[test]
fn the_default_programs_one_scope_exports_the_whole_metrics_path_as_the_derived_artifact() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let catalog = default_program_catalog();
    let scope = "metrics".parse::<ScopeName>().unwrap();
    let bundle = export(&plan, &scope, &catalog, &BTreeMap::new()).unwrap();
    assert_eq!(bundle.artifact, PolicyArtifact::from_json(METRICS_EXPORT_JSON).unwrap());
    assert_eq!(base(&plan, &scope).unwrap(), bundle.artifact.base);

    // The scope covers every node but the sources, aggregation included, and every edge; the sources' edges leave its
    // inputs, written in the order the plan lists them.
    assert_eq!(bundle.artifact.nodes.len(), plan.nodes.len() - 5);
    assert!(bundle.artifact.nodes.iter().any(|node| node.id.as_str() == "dsd_agg"));
    assert_eq!(bundle.artifact.edges.len(), plan.edges.len());
    let inputs = edges(&bundle.artifact)
        .into_iter()
        .filter(|edge| edge.starts_with("input."))
        .collect::<Vec<_>>();
    assert_eq!(
        inputs,
        [
            "input.dogstatsd -> dsd_enrich.in",
            "input.otlp -> metrics_enrich_in.1",
            "input.checks -> metrics_enrich_in.2",
            "input.liveness -> metrics_enrich_in.3",
            "input.trace_sampler -> metrics_enrich_in.4",
        ]
    );

    // The catalog declares each input with what its source carries, and that the host runs one aggregation.
    let declared = |input: &str| catalog.scope_input(&scope, &input.parse().unwrap()).unwrap();
    assert_eq!(
        declared("liveness").timestamps,
        crate::model::TimestampForm::Timestamped
    );
    assert_eq!(
        declared("trace_sampler").timestamps,
        crate::model::TimestampForm::Untimestamped
    );
    assert!(catalog.single_aggregation());
    assert_eq!(
        serde_json::to_value(&bundle.catalog).unwrap()["single_aggregation"],
        json!(true)
    );
}

#[test]
fn bundles_are_read_ignoring_the_members_the_reader_does_not_know() {
    let catalog = catalog_with_values();
    let primary = "dd_metrics_encode".parse::<OutputId>().unwrap();
    let mirror = "mrf_metrics_encode".parse::<OutputId>().unwrap();
    let labels = BTreeMap::from([(primary.clone(), "Primary".to_string())]);
    let bundle = export(&default_program(), &post_enrich(), &catalog, &labels).unwrap();
    let written = serde_json::to_value(&bundle).unwrap();

    // Members added later, at every level of the bundle and its catalog, and labels written in the catalog's outputs,
    // as the stub protocol places them.
    let mut document = written.clone();
    document["future_member"] = json!({"anything": 1});
    document["catalog"]["future_member"] = json!([]);
    let outputs = &mut document["catalog"]["outputs"];
    outputs["mrf_metrics_encode"]["label"] = json!("Multi-region failover");
    outputs["mrf_metrics_encode"]["future_member"] = json!(true);
    outputs["dd_metrics_encode"]["label"] = json!("Not this one: `labels` has a label for the output");
    let bindings = document["catalog"]["bindings"].as_object_mut().unwrap();
    assert!(!bindings.is_empty());
    for binding in bindings.values_mut() {
        binding["future_member"] = json!(1);
    }
    let read = serde_json::from_value::<ExportBundle>(document).unwrap();
    assert_eq!(read.artifact, bundle.artifact);
    assert_eq!(read.catalog, catalog);
    assert_eq!(
        read.labels,
        BTreeMap::from([
            (primary, "Primary".to_string()),
            (mirror, "Multi-region failover".to_string())
        ])
    );

    // The reader keeps every member the writer writes.
    assert_eq!(serde_json::from_value::<ExportBundle>(written.clone()).unwrap(), bundle);

    // What the reader knows, it still checks: the artifact is read as artifacts are, and the catalog's members as
    // the catalog reads them.
    type Mutation = fn(&mut Value);
    let refused: [(&str, Mutation); 3] = [
        ("an unknown artifact member", |document| {
            document["artifact"]["future_member"] = json!(1)
        }),
        ("a malformed fingerprint", |document| {
            document["catalog"]["outputs"]["dd_metrics_encode"]["fingerprint"] = json!(3)
        }),
        ("a catalog that is not an object", |document| {
            document["catalog"] = json!("outputs")
        }),
    ];
    for (what, mutate) in refused {
        let mut document = written.clone();
        mutate(&mut document);
        assert!(serde_json::from_value::<ExportBundle>(document).is_err(), "{}", what);
    }
}

#[test]
fn a_bound_default_programs_export_records_version_2_fingerprints_but_not_host_settings() {
    let plan = Plan::from_value(with_two_scopes(crate::testing::with_resources(DEFAULT_PROGRAM_JSON))).unwrap();
    let main = "dd_metrics_encode".parse::<OutputId>().unwrap();
    let catalog = two_scopes_of(crate::testing::default_program_resources_catalog());
    let labels = BTreeMap::from([(main.clone(), "Primary".to_string())]);
    let bundle = export(&plan, &post_enrich(), &catalog, &labels).unwrap();

    // The base records the version 2 fingerprint of every output the scope emits to: the main output's is the digest
    // of its role, written out in `resources::tests`.
    assert_eq!(
        bundle
            .artifact
            .base
            .outputs
            .get(&main)
            .map(ToString::to_string)
            .as_deref(),
        Some("sha256:35b6c7483c20787a99a97a2a2bb2684fd1a0a8f0315e5bf73fad5b48b74595f7")
    );
    let written = serde_json::to_value(&bundle).unwrap();
    assert_eq!(written["labels"]["dd_metrics_encode"], json!("Primary"));
    // Host settings and recipients' declarations, credential references included, stay on the host: the catalog names
    // recipients by identity only.
    let text = written.to_string();
    for private in [
        "request_timeout",
        "endpoint_concurrency",
        "native:api_key",
        "datadog_intake",
    ] {
        assert!(!text.contains(private), "the export carries `{}`", private);
    }

    // A reader keeps what the writer writes.
    let read = serde_json::from_value::<ExportBundle>(written).unwrap();
    assert_eq!(read, bundle);
}

#[test]
fn annotations_and_the_recipient_directory_travel_with_the_bundle_and_change_nothing_in_the_artifact() {
    let plan = default_program();
    let recipient = crate::validate::CatalogRecipient {
        recipients: vec!["dd_out/primary/0/https://primary.example.com".parse().unwrap()],
        host: "dd_out".parse().unwrap(),
        endpoint: "https://primary.example.com".to_string(),
    };
    let catalog = two_scope_catalog().with_recipient("primary".parse().unwrap(), recipient.clone());
    let plain = export(&plan, &post_enrich(), &catalog, &BTreeMap::new()).unwrap();
    assert!(plain.annotations.is_empty());
    assert!(serde_json::to_value(&plain).unwrap().get("annotations").is_none());

    let mut bundle = plain.clone();
    bundle.annotations.nodes.insert(
        "metrics_policy".parse().unwrap(),
        "Policy: one branch per destination".to_string(),
    );
    bundle
        .annotations
        .bindings
        .insert("host.tags".parse().unwrap(), "The host's tags".to_string());
    let written = serde_json::to_value(&bundle).unwrap();
    assert_eq!(
        written["annotations"],
        json!({"nodes": {"metrics_policy": "Policy: one branch per destination"},
               "bindings": {"host.tags": "The host's tags"}})
    );
    assert_eq!(
        written["catalog"]["recipients"]["primary"],
        json!({"recipients": ["dd_out/primary/0/https://primary.example.com"], "host": "dd_out",
               "endpoint": "https://primary.example.com"})
    );
    // Readers keep both, and ignore what they do not know in annotations and in directory entries.
    let mut extended = written.clone();
    extended["annotations"]["outputs"] = json!({"dd_metrics_encode": "the main output"});
    extended["catalog"]["recipients"]["primary"]["label"] = json!("Primary");
    extended["catalog"]["recipients"]["primary"]["source"] = json!("native");
    let read = serde_json::from_value::<ExportBundle>(extended).unwrap();
    assert_eq!(read, bundle);
    assert_eq!(
        Catalog::recipient(&read.catalog, &"primary".parse().unwrap()),
        Some(&recipient)
    );
    // The artifact, and so its base, is the same with or without notes.
    assert_eq!(read.artifact, plain.artifact);
}

#[test]
fn a_dual_ship_endpoint_under_the_main_role_changes_no_base_and_renumbered_routing_groups_do() {
    fn bound(edit: impl FnOnce(&mut Value)) -> Plan {
        let mut value = with_two_scopes(crate::testing::with_resources(DEFAULT_PROGRAM_JSON));
        edit(&mut value);
        Plan::from_value(value).unwrap()
    }
    fn position(value: &Value, id: &str) -> usize {
        value["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .position(|output| output["id"] == id)
            .unwrap()
    }
    let before = bound(|_| {});

    // Native configuration gains a dual-ship endpoint: lowering resolves it under the main output's role, so the
    // output's recipients change and its fingerprint, the digest of the role, does not. Neither does any base.
    let dual = bound(|value| {
        let main = value["outputs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|output| output["id"] == "dd_metrics_encode")
            .unwrap();
        main["recipients"] = json!(["dd_out/additional_2/0", "dd_out/primary/0"]);
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "dd_out/additional_2/0", "host": "dd_out", "endpoint": "https://dual.example.com",
            "credential": "native:additional/0000111122223333/0", "classes": "all"
        }));
    });
    assert_ne!(
        before.output(&"dd_metrics_encode".parse().unwrap()),
        dual.output(&"dd_metrics_encode".parse().unwrap())
    );
    for scope in ["post_enrich", "dogstatsd_pre_aggregation"] {
        let scope = scope.parse::<ScopeName>().unwrap();
        assert_eq!(
            base(&before, &scope).unwrap(),
            base(&dual, &scope).unwrap(),
            "{}",
            scope
        );
    }

    // The routing groups renumbered: the group with key `b...` is at position 0 now. The outputs at each position now
    // deliver elsewhere, which their fingerprints say, and the policy scope's base changes.
    let renumbered = bound(|value| {
        let first = position(value, "metrics_routing_encode_0");
        let second = position(value, "metrics_routing_encode_1");
        let outputs = value["outputs"].as_array_mut().unwrap();
        for member in ["fingerprint", "recipients"] {
            let swapped = outputs[first][member].clone();
            outputs[first][member] = outputs[second][member].clone();
            outputs[second][member] = swapped;
        }
        let swapped = outputs[first]["binding"]["selector"].clone();
        outputs[first]["binding"]["selector"] = outputs[second]["binding"]["selector"].clone();
        outputs[second]["binding"]["selector"] = swapped;
    });
    let (was, is) = (
        base(&before, &post_enrich()).unwrap(),
        base(&renumbered, &post_enrich()).unwrap(),
    );
    assert_ne!(was.program_hash, is.program_hash);
    let group = |base: &ArtifactBase, id: &str| base.outputs[&id.parse::<OutputId>().unwrap()].to_string();
    assert_eq!(
        group(&is, "metrics_routing_encode_0"),
        "sha256:ccc6ed3c9012f1da6fe870fbf4a1c9e3357dfa31e85c0ba234a8d4ec7b740a99"
    );
    assert_eq!(
        group(&is, "metrics_routing_encode_1"),
        group(&was, "metrics_routing_encode_0")
    );
}

/// Returns the fixtures' catalog with a value for every binding it declares, as a host's exported catalog has.
fn catalog_with_values() -> MemoryCatalog {
    let mut document = serde_json::to_value(two_scope_catalog()).unwrap();
    for binding in document["bindings"].as_object_mut().unwrap().values_mut() {
        binding["value"] = json!(false);
    }
    serde_json::from_value(document).unwrap()
}

#[test]
fn a_change_outside_the_scope_changes_neither_the_export_nor_its_base() {
    // The fixture without the no-aggregation pipeline differs from the default program only before aggregation.
    let other = two_scopes(NO_AGGREGATION_PIPELINE_OFF_JSON);
    assert_ne!(
        other.hash(ResolvedValues::Exclude).unwrap(),
        default_program().hash(ResolvedValues::Exclude).unwrap()
    );
    let bundle = export(&other, &post_enrich(), &MemoryCatalog::new(), &BTreeMap::new()).unwrap();
    assert_eq!(bundle.artifact, expected_export());
}

#[test]
fn the_base_program_hash_covers_the_scopes_structure_and_outputs_but_not_its_values() {
    let scope = post_enrich();
    let plan = default_program();
    let expected = expected_export().base;
    assert_eq!(base(&plan, &scope).unwrap(), expected);
    assert_eq!(
        expected.program_hash.to_string(),
        "sha256:8b26675fd175dfa56ac0386830efa732d3a030ac745d7d909f95f74e09553327"
    );

    let index = |plan: &Plan, id: &str| plan.nodes.iter().position(|node| node.id.to_string() == id).unwrap();
    let with = |change: fn(&mut Plan, usize, usize)| {
        let mut changed = default_program();
        let inside = index(&changed, "af_metrics_gateway");
        let outside = index(&changed, "dsd_tag_filterlist");
        change(&mut changed, inside, outside);
        base(&changed, &scope).unwrap()
    };

    // A new resolved value of a binding inside the scope: the same base.
    let resolved = with(|plan, inside, _| {
        let predicate = json!({"all": [{"kind_in": ["counter", "rate", "gauge", "set"]},
            {"flag": {"config": "autoscaling_failover.enabled", "resolved": false}},
            {"name": {"matches": {"values": {"config": "autoscaling_failover.metrics", "resolved": ["other"]},
                                  "prefix": false}}}]});
        plan.nodes[inside]
            .params
            .insert("predicate".parse().unwrap(), predicate);
    });
    assert_eq!(resolved, expected);

    // A structural change outside the scope: the same base.
    let outside = with(|plan, _, outside| plan.nodes[outside].params = Params::new());
    assert_eq!(outside, expected);

    // A literal parameter inside the scope: another program hash, the same outputs.
    let literal = with(|plan, inside, _| {
        plan.nodes[inside]
            .params
            .insert("predicate".parse().unwrap(), json!({"const": true}));
    });
    assert_ne!(literal.program_hash, expected.program_hash);
    assert_eq!(literal.outputs, expected.outputs);

    // Another fingerprint of an output the scope emits to changes the export, so it changes the program hash too: a
    // client that keeps the export sees that it must fetch it again.
    let rebind = |plan: &mut Plan, id: &str, fingerprint: PlanHash| {
        plan.outputs
            .iter_mut()
            .find(|output| output.id.as_str() == id)
            .unwrap()
            .fingerprint = fingerprint;
    };
    let mut rebound = default_program();
    let fingerprint = PlanHash::from_digest([7; 32]);
    rebind(&mut rebound, "mrf_metrics_encode", fingerprint);
    let rebound = base(&rebound, &scope).unwrap();
    assert_ne!(rebound.program_hash, expected.program_hash);
    let mut outputs = expected.outputs.clone();
    outputs.insert("mrf_metrics_encode".parse().unwrap(), fingerprint);
    assert_eq!(rebound.outputs, outputs);

    // An output the scope does not emit to is not part of its export.
    let mut elsewhere = default_program();
    rebind(&mut elsewhere, "dsd_client_telemetry", fingerprint);
    assert_eq!(base(&elsewhere, &scope).unwrap(), expected);
}

#[test]
fn the_base_program_hash_is_the_hash_of_the_export_without_it() {
    // For every default program, the export's document without `base.program_hash` hashes to it, so a tool can tell an
    // unmodified export by its document alone.
    let plans = [
        default_program(),
        two_scopes(NO_AGGREGATION_PIPELINE_OFF_JSON),
        Plan::from_value(with_two_scopes(with_opaque_gateways(DEFAULT_PROGRAM_JSON))).unwrap(),
    ];
    for plan in plans {
        let artifact = export(&plan, &post_enrich(), &MemoryCatalog::new(), &BTreeMap::new())
            .unwrap()
            .artifact;
        let mut document = serde_json::to_value(&artifact).unwrap();
        document["base"].as_object_mut().unwrap().remove("program_hash");
        assert_eq!(PlanHash::of_value(&document).unwrap(), artifact.base.program_hash);
    }
}

#[test]
fn the_default_program_with_opaque_gateways_exports_them_as_opaque_hosts() {
    // Until the gateways lower to routing operators, the scope holds them as opaque hosts, each with the `emit` its
    // output feeds.
    let plan = Plan::from_value(with_two_scopes(with_opaque_gateways(DEFAULT_PROGRAM_JSON))).unwrap();
    let artifact = export(&plan, &post_enrich(), &MemoryCatalog::new(), &BTreeMap::new())
        .unwrap()
        .artifact;
    let nodes = artifact
        .nodes
        .iter()
        .map(|node| format!("{}={}", node.id, node.op))
        .collect::<Vec<_>>();
    assert_eq!(
        nodes,
        [
            "metrics_policy=tee",
            "metrics_policy_0=emit",
            "metrics_routing_filter_0=opaque_host",
            "metrics_routing_filter_0_out=emit",
            "metrics_routing_filter_1=opaque_host",
            "metrics_routing_filter_1_out=emit",
            "mrf_metrics_gateway=opaque_host",
            "mrf_metrics_gateway_out=emit",
            "af_metrics_gateway=opaque_host",
            "af_metrics_gateway_out=emit",
        ]
    );
    assert_eq!(artifact.edges.len(), 10);
    assert_eq!(artifact.base.outputs, expected_export().base.outputs);
    assert_ne!(artifact.base.program_hash, expected_export().base.program_hash);
}

#[test]
fn scopes_that_cannot_be_written_as_artifacts_are_not_exported() {
    let scope = post_enrich();
    type Mutation = fn(&mut Plan);
    // (what, mutation, fragment of the error)
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "two exits of one port",
            |plan| {
                let exits = &mut plan.scopes.get_mut("post_enrich").unwrap().exits;
                for exit in ["first", "second"] {
                    exits.insert(ExitName::new(exit).unwrap(), "dsd_agg.in".parse().unwrap());
                }
            },
            "its exits `first` and `second` both name `dsd_agg.in`",
        ),
        (
            "two inputs of one port",
            |plan| {
                let port = "metrics_enrich_host_tags.out".parse().unwrap();
                plan.scopes
                    .get_mut("post_enrich")
                    .unwrap()
                    .inputs
                    .insert("again".parse().unwrap(), port);
            },
            "both name `metrics_enrich_host_tags.out`",
        ),
        (
            "a region node fed from outside",
            |plan| {
                // The DogStatsD client telemetry tap now enters a second port of a node inside the scope.
                let edge = plan
                    .edges
                    .iter_mut()
                    .find(|edge| edge.to.to_string() == "dsd_post_agg_filter_fanout_0.in")
                    .unwrap();
                edge.to = "af_metrics_gateway_false.1".parse().unwrap();
            },
            "`af_metrics_gateway_false.1` is inside the scope, but is fed from `dsd_post_agg_filter_fanout.0`",
        ),
        (
            "an emit to an undeclared output",
            |plan| plan.outputs.retain(|output| output.id.as_str() != "mrf_metrics_encode"),
            "delivers to `mrf_metrics_encode`, which the plan does not declare",
        ),
        (
            "an edge into a missing node",
            |plan| {
                plan.nodes.retain(|node| node.id.to_string() != "metrics_policy_0");
            },
            "an edge enters `metrics_policy_0`, which is not a node of the plan",
        ),
        (
            "a node named like the scope input",
            |plan| rename(plan, "metrics_policy_0", "input"),
            "local ID that artifact nodes cannot have",
        ),
        (
            "two nodes with one local ID",
            |plan| rename(plan, "metrics_policy_0", "policy/metrics_policy"),
            "the local ID `metrics_policy`",
        ),
    ];
    for (what, mutate, fragment) in cases {
        let mut plan = default_program();
        mutate(&mut plan);
        let error = base(&plan, &scope).unwrap_err();
        assert!(
            matches!(error, ExportError::NotExportable { .. }),
            "{}: {}",
            what,
            error
        );
        assert!(error.to_string().contains(fragment), "{}: {}", what, error);
        assert!(
            export(&plan, &scope, &MemoryCatalog::new(), &BTreeMap::new()).is_err(),
            "{}",
            what
        );
    }

    let error = base(&default_program(), &"pre_aggregation".parse().unwrap()).unwrap_err();
    assert!(matches!(error, ExportError::UnknownScope { .. }), "{}", error);

    // An edge that leaves the pre-aggregation scope other than through an exit takes what it enters into the region,
    // which is then fed from outside.
    let mut plan = default_program();
    let edge = plan
        .edges
        .iter_mut()
        .find(|edge| edge.to.to_string() == "dsd_post_agg_filter_in.1")
        .unwrap();
    edge.to = "metrics_enrich_in.0".parse().unwrap();
    let error = base(&plan, &"dogstatsd_pre_aggregation".parse().unwrap()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is inside the scope, but is fed from `dsd_post_agg_filter_fanout.1`, outside it"),
        "{}",
        error
    );
}

/// Renames a node and every edge at its ports.
fn rename(plan: &mut Plan, from: &str, to: &str) {
    let from = from.parse::<NodeId>().unwrap();
    let to = to.parse::<NodeId>().unwrap();
    for node in plan.nodes.iter_mut().filter(|node| node.id == from) {
        node.id = to.clone();
    }
    for edge in &mut plan.edges {
        for port in [&mut edge.from, &mut edge.to] {
            if port.node == from {
                port.node = to.clone();
            }
        }
    }
}

/// The export of the default program's `dogstatsd_pre_aggregation` scope, written by hand from
/// `fixtures/default-program.json`, with its program hash computed as [`DEFAULT_PROGRAM_EXPORT_JSON`]'s was. Its edges
/// reach the scope's exits (`exit.<name>`).
const PRE_AGGREGATION_EXPORT_JSON: &str =
    include_str!("../../fixtures/export/default-program-dogstatsd-pre-aggregation.json");

fn pre_aggregation() -> ScopeName {
    "dogstatsd_pre_aggregation".parse().unwrap()
}

fn edges(artifact: &PolicyArtifact) -> Vec<String> {
    artifact
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect()
}

#[test]
fn the_pre_aggregation_scope_exports_up_to_its_exits_as_the_hand_written_artifact() {
    let bundle = export(
        &default_program(),
        &pre_aggregation(),
        &two_scope_catalog(),
        &BTreeMap::new(),
    )
    .unwrap();
    let expected = serde_json::from_str::<Value>(PRE_AGGREGATION_EXPORT_JSON).unwrap();
    assert_eq!(serde_json::to_value(&bundle.artifact).unwrap(), expected);
    // The document reads back as an artifact, its edges into the exits included, and is the export's.
    assert_eq!(PolicyArtifact::from_value(expected).unwrap(), bundle.artifact);
    // Its records leave through the exits, not through `emit`s: the base records no output.
    assert!(bundle.artifact.base.outputs.is_empty());
    assert_eq!(
        base(&default_program(), &pre_aggregation()).unwrap(),
        bundle.artifact.base
    );
    // The policy scope's base is what it was before the pre-aggregation scope existed.
    assert_eq!(
        base(&default_program(), &post_enrich()).unwrap(),
        expected_export().base
    );
}

#[test]
fn without_the_no_aggregation_pipeline_the_pre_aggregation_scope_ends_at_one_exit() {
    let plan = two_scopes(NO_AGGREGATION_PIPELINE_OFF_JSON);
    let artifact = export(&plan, &pre_aggregation(), &MemoryCatalog::new(), &BTreeMap::new())
        .unwrap()
        .artifact;
    let nodes = artifact
        .nodes
        .iter()
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        nodes,
        [
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_prefix_filter_blocklist",
            "dsd_prefix_filter_blocklist_false",
            "dsd_tag_filterlist",
            "dsd_agg",
        ]
    );
    assert_eq!(
        edges(&artifact),
        [
            "input.metrics -> dsd_enrich.in",
            "dsd_enrich.out -> dsd_prefix_filter.in",
            "dsd_prefix_filter.out -> dsd_prefix_filter_blocklist.in",
            "dsd_prefix_filter_blocklist.false -> dsd_prefix_filter_blocklist_false.in",
            "dsd_prefix_filter_blocklist.true -> dsd_tag_filterlist.in",
            "dsd_tag_filterlist.out -> dsd_agg.in",
            "dsd_agg.out -> exit.aggregated",
        ]
    );
}

#[test]
fn a_change_past_a_scopes_exits_changes_neither_its_export_nor_its_base() {
    // The post-aggregation filter is past the exits of the pre-aggregation scope.
    let mut changed = default_program();
    let filter = changed
        .nodes
        .iter_mut()
        .find(|node| node.id.to_string() == "dsd_post_agg_filter")
        .unwrap();
    filter.params = Params::new();
    let before = export(
        &default_program(),
        &pre_aggregation(),
        &MemoryCatalog::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let after = export(&changed, &pre_aggregation(), &MemoryCatalog::new(), &BTreeMap::new()).unwrap();
    assert_eq!(before.artifact, after.artifact);

    // An exit's port fed from outside the region too: that edge is not the region's.
    let mut fed = default_program();
    fed.edges.push(crate::ir::Edge {
        from: "otlp_in.metrics".parse().unwrap(),
        to: "dsd_post_agg_filter_in.0".parse().unwrap(),
    });
    let artifact = export(&fed, &pre_aggregation(), &MemoryCatalog::new(), &BTreeMap::new())
        .unwrap()
        .artifact;
    assert_eq!(artifact, before.artifact);
}

use serde_json::{json, Value};

use super::*;
use crate::testing::{
    default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON,
};
use crate::validate::Catalog;

/// The export of the default program, derived from `fixtures/default-program.json` with Python (every node but the
/// sources, with its bound parameters unresolved and without provenance, and every edge, with each source's port
/// written as `input.<binding>`). Its program hash, below, was computed independently of this crate, with Python: the
/// SHA-256 digest of `json.dumps(document, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`.
const DEFAULT_PROGRAM_EXPORT_JSON: &str = include_str!("../../fixtures/export/default-program.json");

/// The program hash of [`DEFAULT_PROGRAM_EXPORT_JSON`].
const DEFAULT_PROGRAM_EXPORT_HASH: &str = "sha256:9d2e20e85139e30dceeed6470c66ccc57e42c4c6655a2341ac1493fb02b36b7b";

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap()
}

fn expected_export() -> PolicyArtifact {
    PolicyArtifact::from_json(DEFAULT_PROGRAM_EXPORT_JSON).unwrap()
}

fn edges(artifact: &PolicyArtifact) -> Vec<String> {
    artifact
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect()
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
fn the_default_program_exports_as_the_derived_artifact() {
    let plan = default_program();
    let catalog = default_program_catalog();
    let labels = BTreeMap::from([("dd_metrics_encode".parse().unwrap(), "Primary".to_string())]);
    let bundle = export(&plan, &catalog, &labels).unwrap();
    assert_eq!(bundle.artifact, expected_export());
    assert_eq!(bundle.export_version, EXPORT_VERSION);
    assert_eq!(bundle.ir_versions, IrVersions::supported());
    assert_eq!(bundle.catalog, catalog);
    assert_eq!(bundle.labels, labels);
    assert_eq!(
        program_hash(&bundle.artifact).unwrap().to_string(),
        DEFAULT_PROGRAM_EXPORT_HASH
    );

    // The program is every node but the sources, aggregation included, and every edge; the sources' edges leave
    // `input.<binding>`, written in the order the plan lists them.
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
            "input.otlp -> metrics_in.1",
            "input.checks -> metrics_in.2",
            "input.liveness -> metrics_in.3",
            "input.trace_sampler -> metrics_in.4",
        ]
    );

    // The bundle is written as the protocol describes it, and reads back.
    let document = serde_json::to_value(&bundle).unwrap();
    let members = document.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        members,
        ["artifact", "catalog", "export_version", "form", "ir_versions", "labels"]
    );
    assert_eq!(document["export_version"], json!(2));
    assert_eq!(document["labels"], json!({"dd_metrics_encode": "Primary"}));
    assert_eq!(document["catalog"], serde_json::to_value(&catalog).unwrap());
    assert_eq!(serde_json::from_value::<ExportBundle>(document).unwrap(), bundle);

    // Labels are left out when there are none.
    let unlabeled = export(&plan, &catalog, &BTreeMap::new()).unwrap();
    assert!(serde_json::to_value(&unlabeled).unwrap().get("labels").is_none());
}

#[test]
fn bundles_are_read_ignoring_the_members_the_reader_does_not_know() {
    let catalog = catalog_with_values();
    let primary = "dd_metrics_encode".parse::<OutputId>().unwrap();
    let mirror = "mrf_metrics_encode".parse::<OutputId>().unwrap();
    let labels = BTreeMap::from([(primary.clone(), "Primary".to_string())]);
    let bundle = export(&default_program(), &catalog, &labels).unwrap();
    let written = serde_json::to_value(&bundle).unwrap();

    // Members added later, at every level of the bundle and its catalog, members earlier writers wrote (a scope, and
    // outputs' fingerprints), and labels written in the catalog's outputs, as the stub protocol places them.
    let mut document = written.clone();
    document["future_member"] = json!({"anything": 1});
    document["scope"] = json!("metrics");
    document["catalog"]["future_member"] = json!([]);
    let outputs = &mut document["catalog"]["outputs"];
    outputs["mrf_metrics_encode"]["label"] = json!("Multi-region failover");
    outputs["mrf_metrics_encode"]["future_member"] = json!(true);
    outputs["dd_metrics_encode"]["label"] = json!("Not this one: `labels` has a label for the output");
    outputs["dd_metrics_encode"]["fingerprint"] = json!(format!("sha256:{}", "2".repeat(64)));
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
    let refused: [(&str, Mutation); 4] = [
        ("an unknown artifact member", |document| {
            document["artifact"]["future_member"] = json!(1)
        }),
        ("an artifact of one scope", |document| {
            document["artifact"]["scope"] = json!("metrics")
        }),
        ("malformed recipients", |document| {
            document["catalog"]["outputs"]["dd_metrics_encode"]["recipients"] = json!(3)
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
fn a_bound_default_programs_export_carries_no_host_settings() {
    let plan = Plan::from_value(crate::testing::with_resources(DEFAULT_PROGRAM_JSON)).unwrap();
    let main = "dd_metrics_encode".parse::<OutputId>().unwrap();
    let catalog = crate::testing::default_program_resources_catalog();
    let labels = BTreeMap::from([(main, "Primary".to_string())]);
    let bundle = export(&plan, &catalog, &labels).unwrap();

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
    let catalog = default_program_catalog().with_recipient("primary".parse().unwrap(), recipient.clone());
    let plain = export(&plan, &catalog, &BTreeMap::new()).unwrap();
    assert!(plain.annotations.is_empty());
    assert!(serde_json::to_value(&plain).unwrap().get("annotations").is_none());

    let mut bundle = plain.clone();
    bundle.annotations.nodes.insert(
        "metrics_fanout".parse().unwrap(),
        "Policy: one branch per destination".to_string(),
    );
    bundle
        .annotations
        .bindings
        .insert("host.tags".parse().unwrap(), "The host's tags".to_string());
    let written = serde_json::to_value(&bundle).unwrap();
    assert_eq!(
        written["annotations"],
        json!({"nodes": {"metrics_fanout": "Policy: one branch per destination"},
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
    // The artifact, and so its program hash, is the same with or without notes.
    assert_eq!(read.artifact, plain.artifact);
}

/// Returns the fixtures' catalog with a value for every binding it declares, as a host's exported catalog has.
fn catalog_with_values() -> MemoryCatalog {
    let mut document = serde_json::to_value(default_program_catalog()).unwrap();
    for binding in document["bindings"].as_object_mut().unwrap().values_mut() {
        binding["value"] = json!(false);
    }
    serde_json::from_value(document).unwrap()
}

#[test]
fn the_program_hash_covers_the_programs_structure_and_literals_but_not_resolved_values() {
    let plan = default_program();
    let hash = |plan: &Plan| program_hash(&export(plan, &MemoryCatalog::new(), &BTreeMap::new()).unwrap().artifact);
    let expected = hash(&plan).unwrap();
    assert_eq!(expected.to_string(), DEFAULT_PROGRAM_EXPORT_HASH);

    let index = |plan: &Plan, id: &str| plan.nodes.iter().position(|node| node.id.to_string() == id).unwrap();
    let with = |change: fn(&mut Plan, usize)| {
        let mut changed = default_program();
        let gateway = index(&changed, "af_metrics_gateway");
        change(&mut changed, gateway);
        hash(&changed).unwrap()
    };

    // A new resolved value of a signal: the same program, whose export binds the signal.
    let resolved = with(|plan, gateway| {
        let predicate = json!({"all": [{"kind_in": ["counter", "rate", "gauge", "set"]},
            {"flag": {"config": "autoscaling_failover.enabled", "resolved": false}},
            {"name": {"eq": "requests.count"}}]});
        plan.nodes[gateway]
            .params
            .insert("predicate".parse().unwrap(), predicate);
    });
    assert_eq!(resolved, expected);

    // A literal parameter: another program.
    let literal = with(|plan, gateway| {
        plan.nodes[gateway]
            .params
            .insert("predicate".parse().unwrap(), json!({"const": true}));
    });
    assert_ne!(literal, expected);

    // The fixture without the no-aggregation pipeline: another program.
    let other = Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap();
    assert_ne!(other.hash().unwrap(), plan.hash().unwrap());
    assert_ne!(hash(&other).unwrap(), expected);
}

#[test]
fn the_program_hash_is_the_hash_of_the_exports_document() {
    // For every default program, the export's document hashes to its program hash, so a tool can tell an unmodified
    // export by its document alone, and the host's `default_program_hash` is what the export it serves hashes to.
    let plans = [
        default_program(),
        Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
        Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap(),
    ];
    for plan in plans {
        let artifact = export(&plan, &MemoryCatalog::new(), &BTreeMap::new()).unwrap().artifact;
        let document = serde_json::to_value(&artifact).unwrap();
        assert_eq!(PlanHash::of_value(&document).unwrap(), program_hash(&artifact).unwrap());
        assert_eq!(
            crate::ir::ReceivedArtifact::from_value(document).unwrap().hash,
            program_hash(&artifact).unwrap()
        );
    }
}

#[test]
fn the_default_program_with_opaque_gateways_exports_them_as_opaque_hosts() {
    // Until the gateways lower to routing operators, the default program holds them as opaque hosts, each with the
    // `emit` its output feeds.
    let plan = Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap();
    let artifact = export(&plan, &MemoryCatalog::new(), &BTreeMap::new()).unwrap().artifact;
    let index = artifact
        .nodes
        .iter()
        .position(|node| node.id.as_str() == "metrics_fanout")
        .unwrap();
    let nodes = artifact.nodes[index..]
        .iter()
        .map(|node| format!("{}={}", node.id, node.op))
        .collect::<Vec<_>>();
    assert_eq!(
        nodes,
        [
            "metrics_fanout=tee",
            "metrics_fanout_0=emit",
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
    assert_eq!(artifact.edges.len(), plan.edges.len());
    assert_ne!(
        program_hash(&artifact).unwrap(),
        program_hash(&expected_export()).unwrap()
    );
}

#[test]
fn programs_that_cannot_be_written_as_artifacts_are_not_exported() {
    type Mutation = fn(&mut Plan);
    // (what, mutation, fragment of the error)
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "a node fed from a node the plan does not have",
            |plan| {
                plan.nodes.retain(|node| node.id.to_string() != "metrics_fanout");
            },
            "`metrics_fanout_0.in` is fed from `metrics_fanout.0`, which is neither a source nor a node of the program",
        ),
        (
            "a node named like the sources",
            |plan| rename(plan, "metrics_fanout_0", "input"),
            "local ID that artifact nodes cannot have",
        ),
        (
            "two nodes with one ID",
            |plan| rename(plan, "metrics_fanout_0", "metrics_fanout"),
            "the local ID `metrics_fanout`",
        ),
    ];
    for (what, mutate, fragment) in cases {
        let mut plan = default_program();
        mutate(&mut plan);
        let error = export(&plan, &MemoryCatalog::new(), &BTreeMap::new()).unwrap_err();
        assert!(
            matches!(error, ExportError::NotExportable { .. }),
            "{}: {}",
            what,
            error
        );
        assert!(error.to_string().contains(fragment), "{}: {}", what, error);
    }
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

#[test]
fn the_program_a_composed_plan_runs_is_every_node_but_its_sources() {
    let mut plan = default_program();
    assert_eq!(program_nodes(&plan), None);
    let revision = ArtifactRevision {
        revision: 4,
        hash: PlanHash::of_bytes(b"artifact"),
    };
    plan.provenance.get_or_insert_with(Default::default).artifact = Some(revision.clone());
    let (artifact, nodes) = program_nodes(&plan).unwrap();
    assert_eq!(artifact, revision);
    assert_eq!(nodes.len(), plan.nodes.len() - 5);
    assert!(nodes
        .iter()
        .all(|node| plan.node(node).unwrap().op.as_str() != names::SOURCE));
}

//! Frames with delivery hosts, and placement of outputs by their host: fixtures with expected results written by hand
//! from the stage 7 design.
//!
//! The default program here is the one with its outputs bound to `dd_out`, `mrf_dd_out`, and `cluster_agent_out`, as
//! `fixtures/default-program-resources.json` binds them, and its frame has the three hosts' encode hosts.

use serde_json::{json, Value};

use super::*;
use crate::{
    ir::{HostId, NativeComponentId, NodeId, OutputId, Plan},
    testing::{default_program_hosts, default_program_resources_catalog, with_resources, DEFAULT_PROGRAM_JSON},
    validate::{MemoryCatalog, Validator},
};

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

fn host(id: &str) -> FrameTarget {
    FrameTarget::EncodeHost(HostId::new(id).unwrap())
}

fn destination(id: &str) -> FrameTarget {
    FrameTarget::Destination(OutputId::new(id).unwrap())
}

fn bound(edit: impl FnOnce(&mut Value)) -> Plan {
    let mut value = with_resources(DEFAULT_PROGRAM_JSON);
    edit(&mut value);
    Plan::from_value(value).unwrap()
}

fn node_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == id)
        .unwrap()
}

fn output_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["outputs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|output| output["id"] == id)
        .unwrap()
}

fn boot_frame() -> Frame {
    Frame::compute_with_hosts(
        &bound(|_| {}),
        &OperatorRegistry::native(),
        &namer,
        &default_program_hosts(),
    )
    .unwrap()
}

fn exits(frame: &Frame, slot: &str) -> Vec<(String, FrameTarget)> {
    frame
        .slots()
        .iter()
        .find(|found| found.name.as_str() == slot)
        .unwrap_or_else(|| panic!("no slot `{}`", slot))
        .exits
        .iter()
        .map(|exit| (exit.name.to_string(), exit.target.clone()))
        .collect()
}

fn named(exits: &[(&str, FrameTarget)]) -> Vec<(String, FrameTarget)> {
    exits
        .iter()
        .map(|(name, target)| (name.to_string(), target.clone()))
        .collect()
}

fn validated(plan: &Plan, catalog: &MemoryCatalog) -> ValidationReport {
    let report = Validator::new(&OperatorRegistry::native(), catalog).validate(plan);
    assert!(report.is_valid(), "{}", report);
    report
}

/// Returns the problems placing `plan` into the bound default program's frame finds, and the capabilities they need.
fn unplaceable(plan: &Plan, catalog: &MemoryCatalog) -> (Vec<String>, Vec<MissingCapability>) {
    let error = place(
        plan,
        &validated(plan, catalog),
        &OperatorRegistry::native(),
        &boot_frame(),
    )
    .unwrap_err();
    let PlacementError::Unplaceable { problems } = &error else {
        panic!("not unplaceable: {}", error)
    };
    (problems.iter().map(ToString::to_string).collect(), error.capabilities())
}

#[test]
fn every_fused_slot_with_an_output_downstream_has_one_exit_to_every_encode_host() {
    let frame = boot_frame();
    let encode_hosts = [
        ("dd_metrics_encode", host("dd_out")),
        ("mrf_metrics_encode", host("mrf_dd_out")),
        ("af_metrics_encode", host("cluster_agent_out")),
    ];
    // The enrichment slot emits to every bound output: the main output and both routing groups share `dd_out`'s
    // encode host, in boot edge order, and the failover outputs have theirs.
    assert_eq!(exits(&frame, "metrics_enrich"), named(&encode_hosts));
    // The slot before enrichment: its boot edges to the client telemetry tap (a fixed destination) and to the
    // enrichment slot, then an exit to every encode host, idle, so that a later generation can emit to any.
    let mut expected = vec![
        ("dsd_client_telemetry", destination("dsd_client_telemetry")),
        ("metrics_enrich", FrameTarget::Slot(SlotId::new(5))),
    ];
    expected.extend(encode_hosts.iter().cloned());
    assert_eq!(exits(&frame, "dsd_post_agg_filter_region"), named(&expected));
    // The first slot has the tap downstream too, and every encode host after it.
    let mut expected = vec![
        ("dsd_prefix_filter_region", FrameTarget::Slot(SlotId::new(1))),
        ("dsd_client_telemetry", destination("dsd_client_telemetry")),
    ];
    expected.extend(encode_hosts.iter().cloned());
    assert_eq!(exits(&frame, "dsd_pre_aggregation"), named(&expected));
    // The isolation slot has only its boot edge.
    assert_eq!(
        exits(&frame, "dsd_agg"),
        named(&[("dsd_post_agg_filter_region", FrameTarget::Slot(SlotId::new(4)))])
    );
    assert_eq!(
        frame
            .encode_host(&HostId::new("mrf_dd_out").unwrap())
            .map(|host| host.encoder.as_str()),
        Some("mrf_metrics_encode")
    );
    assert_eq!(frame.hosts().iter().count(), 3);
}

#[test]
fn an_idle_host_gets_its_exits_so_an_output_bound_to_it_later_places_without_a_restart() {
    // A host no boot output is bound to, as an idle failover host is: every fused slot with an output downstream still
    // has an exit to its encode host, after those of the outputs.
    let hosts = default_program_hosts().with_host(
        HostId::new("spare_out").unwrap(),
        NativeComponentId::new("spare_encode").unwrap(),
    );
    let registry = OperatorRegistry::native();
    let frame = Frame::compute_with_hosts(&bound(|_| {}), &registry, &namer, &hosts).unwrap();
    let enrich = exits(&frame, "metrics_enrich");
    assert_eq!(enrich.last(), Some(&("spare_encode".to_string(), host("spare_out"))));
    assert_eq!(enrich.len(), 4);
    assert!(exits(&frame, "dsd_agg").iter().all(|(name, _)| name != "spare_encode"));

    // The autoscaling failover output bound to the spare host later places, as a lease there.
    let spare = HostId::new("spare_out").unwrap();
    let moved = bound(|value| {
        let output = output_mut(value, "af_metrics_encode");
        output["binding"]["host"] = json!("spare_out");
        let selector = serde_json::from_value(output["binding"]["selector"].clone()).unwrap();
        let accepts = serde_json::from_value(output["accepts"].clone()).unwrap();
        output["fingerprint"] = json!(crate::resources::fingerprint(&spare, accepts, &selector));
        output["recipients"] = json!(["spare_out/primary/0"]);
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "spare_out/primary/0", "host": "spare_out", "endpoint": "https://spare.example.com",
            "credential": "native:api_key", "classes": "metrics_only", "provenance": "native"
        }));
    });
    let af = moved.output(&OutputId::new("af_metrics_encode").unwrap()).unwrap();
    let catalog = default_program_resources_catalog().with_output(
        af.id.clone(),
        crate::validate::CatalogOutput {
            accepts: af.accepts,
            fingerprint: af.fingerprint,
            recipients: af.recipients.clone(),
            legacy_fingerprint: None,
        },
    );
    let report = validated(&moved, &catalog);
    let placement = place(&moved, &report, &registry, &frame).unwrap();
    let enrich = frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == "metrics_enrich")
        .unwrap();
    let spare_exit = enrich.exit_to(&host("spare_out")).unwrap().index;
    assert!(!placement.is_idle(enrich.id, spare_exit));
}

#[test]
fn a_boot_plan_bound_to_a_host_the_frame_lacks_or_feeding_an_encode_host_directly_has_no_frame() {
    let plan = bound(|_| {});
    let registry = OperatorRegistry::native();
    let hosts = default_program_hosts();
    let without_failover = FrameHosts::new()
        .with_host(
            HostId::new("dd_out").unwrap(),
            NativeComponentId::new("dd_metrics_encode").unwrap(),
        )
        .with_host(
            HostId::new("cluster_agent_out").unwrap(),
            NativeComponentId::new("af_metrics_encode").unwrap(),
        );
    assert_eq!(
        Frame::compute_with_hosts(&plan, &registry, &namer, &without_failover).unwrap_err(),
        FrameError::UnknownHost {
            output: OutputId::new("mrf_metrics_encode").unwrap(),
            host: HostId::new("mrf_dd_out").unwrap(),
        }
    );
    // Without hosts at all, the first bound output is refused.
    assert!(matches!(
        Frame::compute(&plan, &registry, &namer),
        Err(FrameError::UnknownHost { .. })
    ));

    // OTLP metrics emitted straight to the main output: no region would write the lease the encode host reads.
    let direct = bound(|value| {
        value["nodes"].as_array_mut().unwrap().push(json!({
            "id": "otlp_direct", "op": "emit", "params": {"output": "dd_metrics_encode"}
        }));
        for edge in value["edges"].as_array_mut().unwrap() {
            if edge["from"] == "otlp_in.metrics" {
                edge["to"] = json!("otlp_direct.in");
            }
        }
        node_mut(value, "metrics_enrich_in")["params"]["inputs"] = json!(4);
        for edge in value["edges"].as_array_mut().unwrap() {
            for (old, new) in [
                ("metrics_enrich_in.2", "metrics_enrich_in.1"),
                ("metrics_enrich_in.3", "metrics_enrich_in.2"),
                ("metrics_enrich_in.4", "metrics_enrich_in.3"),
            ] {
                if edge["to"] == old {
                    edge["to"] = json!(new);
                }
            }
        }
    });
    let error = Frame::compute_with_hosts(&direct, &registry, &namer, &hosts).unwrap_err();
    assert_eq!(
        error,
        FrameError::DirectToEncodeHost {
            port: "otlp_in.metrics".parse().unwrap(),
            output: OutputId::new("dd_metrics_encode").unwrap(),
            host: HostId::new("dd_out").unwrap(),
        }
    );
    assert!(
        error
            .to_string()
            .contains("Put a region between the host and the output"),
        "{}",
        error
    );
}

#[test]
fn a_new_or_rebound_output_on_a_frame_host_places_as_a_lease_with_no_new_structure() {
    let frame = boot_frame();
    let catalog = default_program_resources_catalog();
    let registry = OperatorRegistry::native();

    // The main output rebound (a dual-ship recipient, another encoder setting) places as the boot plan does.
    let rebound = bound(|value| {
        let main = output_mut(value, "dd_metrics_encode");
        main["recipients"] = json!(["dd_out/additional_2/0", "dd_out/primary/0"]);
        main["binding"]["encoder"]["flush_timeout"] = json!("5s");
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "dd_out/additional_2/0", "host": "dd_out", "endpoint": "https://dual.example.com",
            "credential": "native:additional/0000111122223333/0", "classes": "all", "provenance": "native"
        }));
    });
    let catalog_rebound = catalog.clone().with_output(
        OutputId::new("dd_metrics_encode").unwrap(),
        crate::validate::CatalogOutput {
            accepts: crate::model::PortType::ANY,
            fingerprint: rebound.outputs[1].fingerprint,
            recipients: rebound.outputs[1].recipients.clone(),
            legacy_fingerprint: None,
        },
    );
    let report = validated(&rebound, &catalog_rebound);
    assert!(place(&rebound, &report, &registry, &frame).is_ok());

    // The policy's primary branch emitting to the declared output instead: it runs as a lease of `dd_out`'s encode
    // host, by the enrichment slot's exit to it, which writes the declared output's lease.
    let routed = bound(|value| {
        node_mut(value, "metrics_policy_0")["params"]["output"] = json!("resources_team_d");
    });
    let report = Validator::new(&registry, &catalog).validate(&routed);
    assert!(report.is_valid(), "{}", report);
    let compiled = compile(&routed, &report, &registry, &frame).unwrap();
    let enrich = frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == "metrics_enrich")
        .unwrap();
    let to_dd_out = enrich.exit_to(&host("dd_out")).unwrap().index;
    let leases = compiled.programs[enrich.id.get()]
        .entries()
        .iter()
        .flat_map(|entry| &entry.steps)
        .filter_map(|step| match step {
            Step::Exit { exit, entry, .. } if *exit == to_dd_out => entry.lease().map(ToString::to_string),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        leases.into_iter().collect::<Vec<_>>(),
        vec![
            "metrics_routing_encode_0".to_string(),
            "metrics_routing_encode_1".to_string(),
            "resources_team_d".to_string()
        ]
    );
}

#[test]
fn an_output_bound_to_a_delivery_host_the_frame_lacks_needs_a_restart() {
    let elsewhere = HostId::new("elsewhere_out").unwrap();
    let plan = bound(|value| {
        let main = output_mut(value, "dd_metrics_encode");
        main["binding"]["host"] = json!("elsewhere_out");
        let selector = serde_json::from_value(main["binding"]["selector"].clone()).unwrap();
        main["fingerprint"] = json!(crate::resources::fingerprint(
            &elsewhere,
            crate::model::PortType::ANY,
            &selector
        ));
        main["recipients"] = json!(["elsewhere_out/primary/0"]);
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "elsewhere_out/primary/0", "host": "elsewhere_out", "endpoint": "https://elsewhere.example.com",
            "credential": "native:api_key", "classes": "all", "provenance": "native"
        }));
    });
    let main = &plan.outputs[1];
    let catalog = default_program_resources_catalog().with_output(
        main.id.clone(),
        crate::validate::CatalogOutput {
            accepts: main.accepts,
            fingerprint: main.fingerprint,
            recipients: main.recipients.clone(),
            legacy_fingerprint: None,
        },
    );
    let (problems, capabilities) = unplaceable(&plan, &catalog);
    assert_eq!(capabilities, vec![MissingCapability::Restart]);
    assert_eq!(
        problems,
        vec![
            "output `dd_metrics_encode` delivers through delivery host `elsewhere_out`, which the topology built at \
              boot has no encode host for: adding a delivery host needs a restart, since the physical structure is \
              built from native configuration at boot"
                .to_string()
        ]
    );
}

#[test]
fn structure_only_an_artifact_asks_for_is_unsupported_until_stage_10() {
    // An artifact aggregating after enrichment: the topology has no isolation slot there, and a restart would not add
    // one, since the frame is built from the default program.
    let hash = "sha256:00000000000000000000000000000000000000000000000000000000000000a7";
    let plan = bound(|value| {
        value["provenance"] = json!({"artifacts": [{"revision": 7, "hash": hash}]});
        let mut aggregate = node_mut(value, "dsd_agg").clone();
        aggregate["id"] = json!("policy/agg");
        aggregate["provenance"] = json!({"artifact": {"revision": 7, "hash": hash}});
        value["nodes"].as_array_mut().unwrap().push(aggregate);
        for edge in value["edges"].as_array_mut().unwrap() {
            if edge["from"] == "metrics_enrich_host_tags.out" {
                edge["from"] = json!("policy/agg.out");
            }
        }
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "metrics_enrich_host_tags.out", "to": "policy/agg.in"}));
    });
    let (problems, capabilities) = unplaceable(&plan, &default_program_resources_catalog());
    assert_eq!(capabilities, vec![MissingCapability::FrameChanges]);
    assert_eq!(
        problems,
        vec![
            "node `policy/agg`: it is a state owner, which runs only in an isolation slot that the region its input \
              comes from has an exit to, and the topology built at boot has no such isolation slot for \
              `metrics_enrich`: a new exit from a region to an isolation slot requires stage 10 frame changes: the \
              physical structure is built from the default program at boot, so a restart does not add it"
                .to_string()
        ]
    );
    assert_eq!(MissingCapability::FrameChanges.to_string(), "stage 10 frame changes");
    assert_eq!(MissingCapability::Restart.to_string(), "a restart");
    assert_eq!(MissingCapability::Restart.stage(), None);
}

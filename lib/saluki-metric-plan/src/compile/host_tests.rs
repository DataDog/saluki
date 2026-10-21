//! Frames with delivery hosts, and placement of outputs by their host: fixtures with expected results written by hand
//! from the stage 7 design.
//!
//! The default program here is the one with its outputs bound to `dd_out`, `mrf_dd_out`, and `cluster_agent_out`, as
//! `fixtures/default-program-resources.json` binds them, and its frame has the three hosts' encode hosts.

use saluki_core::execution::AdmissionPointId;
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
        &crate::compile::DerivedPoints::for_plan(&boot_frame(), plan, &OperatorRegistry::native()),
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
    // The enrichment slot, which runs the post-aggregation filter too, emits to the client telemetry tap (a fixed
    // destination) and to every bound output: the main output and both routing groups share `dd_out`'s encode host, in
    // boot edge order, and the failover outputs have theirs.
    let mut expected = vec![("dsd_client_telemetry", destination("dsd_client_telemetry"))];
    expected.extend(encode_hosts.iter().cloned());
    assert_eq!(exits(&frame, "metrics_enrich"), named(&expected));
    // The pre-aggregation slot: its boot edges to aggregation's slot and to the enrichment slot, then the tap
    // downstream of it and an exit to every encode host, idle, so that a later generation can emit to any.
    let mut expected = vec![
        ("dsd_agg", FrameTarget::Slot(SlotId::new(1))),
        ("metrics_enrich", FrameTarget::Slot(SlotId::new(2))),
        ("dsd_client_telemetry", destination("dsd_client_telemetry")),
    ];
    expected.extend(encode_hosts.iter().cloned());
    assert_eq!(exits(&frame, "dsd_pre_aggregation"), named(&expected));
    // Aggregation's slot has the exits of any slot: its boot edge, then the tap downstream of it, and every encode
    // host, so that stateless nodes after aggregation, or a state owner after it, can emit from its regions.
    let mut expected = vec![
        ("metrics_enrich", FrameTarget::Slot(SlotId::new(2))),
        ("dsd_client_telemetry", destination("dsd_client_telemetry")),
    ];
    expected.extend(encode_hosts.iter().cloned());
    assert_eq!(exits(&frame, "dsd_agg"), named(&expected));
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
    assert_eq!(enrich.len(), 5);
    assert_eq!(
        exits(&frame, "dsd_agg").last(),
        Some(&("spare_encode".to_string(), host("spare_out")))
    );

    // The autoscaling failover output bound to the spare host later places, as a lease there.
    let moved = bound(|value| {
        let output = output_mut(value, "af_metrics_encode");
        output["binding"]["host"] = json!("spare_out");
        output["recipients"] = json!(["spare_out/primary/0"]);
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "spare_out/primary/0", "host": "spare_out", "endpoint": "https://spare.example.com",
            "credential": "native:api_key", "classes": "metrics_only"
        }));
    });
    let af = moved.output(&OutputId::new("af_metrics_encode").unwrap()).unwrap();
    let catalog = default_program_resources_catalog().with_output(
        af.id.clone(),
        crate::validate::CatalogOutput {
            accepts: af.accepts,
            recipients: af.recipients.clone(),
        },
    );
    let report = validated(&moved, &catalog);
    let placement = place(
        &moved,
        &report,
        &registry,
        &frame,
        &crate::compile::DerivedPoints::for_plan(&frame, &moved, &registry),
    )
    .unwrap();
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
        node_mut(value, "metrics_in")["params"]["inputs"] = json!(4);
        for edge in value["edges"].as_array_mut().unwrap() {
            for (old, new) in [
                ("metrics_in.2", "metrics_in.1"),
                ("metrics_in.3", "metrics_in.2"),
                ("metrics_in.4", "metrics_in.3"),
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
            "credential": "native:additional/0000111122223333/0", "classes": "all"
        }));
    });
    let catalog_rebound = catalog.clone().with_output(
        OutputId::new("dd_metrics_encode").unwrap(),
        crate::validate::CatalogOutput {
            accepts: crate::model::PortType::ANY,
            recipients: rebound.outputs[1].recipients.clone(),
        },
    );
    let report = validated(&rebound, &catalog_rebound);
    assert!(place(
        &rebound,
        &report,
        &registry,
        &frame,
        &crate::compile::DerivedPoints::for_plan(&frame, &rebound, &registry)
    )
    .is_ok());

    // The policy's primary branch emitting to an implied output for an endpoint of its own instead: it runs as a lease
    // of `dd_out`'s encode host, by the enrichment slot's exit to it, which writes the implied output's lease.
    let recipient = "dd_out/additional_3/0";
    let selector = crate::ir::Selector::Recipients(vec![recipient.parse().unwrap()]);
    let routed = bound(|value| {
        node_mut(value, "metrics_fanout_0")["params"]["output"] = json!("implied_to_own");
        value["outputs"].as_array_mut().unwrap().push(json!({
            "id": "implied_to_own", "accepts": {"kinds": "all", "timestamps": "mixed"},
            "recipients": [recipient],
            "binding": {"host": "dd_out", "selector": selector, "telemetry": "implied_to_own"},
        }));
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": recipient, "host": "dd_out", "endpoint": "https://own.example.com",
            "credential": "native:additional/4444555566667777/0", "classes": "all"
        }));
    });
    let catalog = catalog.with_output(
        OutputId::new("implied_to_own").unwrap(),
        crate::validate::CatalogOutput {
            accepts: crate::model::PortType::ANY,
            recipients: vec![recipient.parse().unwrap()],
        },
    );
    let report = Validator::new(&registry, &catalog).validate(&routed);
    assert!(
        report
            .findings()
            .iter()
            .all(|finding| finding.severity != crate::validate::Severity::Error),
        "{}",
        report
    );
    let compiled = compile(
        &routed,
        &report,
        &registry,
        &frame,
        &crate::compile::DerivedPoints::for_plan(&frame, &routed, &registry),
    )
    .unwrap();
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
            Step::Exit { exit, entry, .. } if *exit == StepExit::Slot(to_dd_out) => {
                entry.lease().map(ToString::to_string)
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        leases.into_iter().collect::<Vec<_>>(),
        vec![
            "implied_to_own".to_string(),
            "metrics_routing_encode_0".to_string(),
            "metrics_routing_encode_1".to_string()
        ]
    );
}

#[test]
fn an_output_bound_to_a_delivery_host_the_frame_lacks_needs_a_restart() {
    let plan = bound(|value| {
        let main = output_mut(value, "dd_metrics_encode");
        main["binding"]["host"] = json!("elsewhere_out");
        main["recipients"] = json!(["elsewhere_out/primary/0"]);
        value["resources"]["recipients"].as_array_mut().unwrap().push(json!({
            "id": "elsewhere_out/primary/0", "host": "elsewhere_out", "endpoint": "https://elsewhere.example.com",
            "credential": "native:api_key", "classes": "all"
        }));
    });
    let main = &plan.outputs[1];
    let catalog = default_program_resources_catalog().with_output(
        main.id.clone(),
        crate::validate::CatalogOutput {
            accepts: main.accepts,
            recipients: main.recipients.clone(),
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
fn an_artifact_aggregating_after_enrichment_runs_it_in_the_main_region_of_enrichments_slot() {
    // An artifact aggregating after enrichment: the topology has no slot of aggregation's there, so it runs in
    // enrichment's slot, in the region of its input, the main region, which runs no other state owner and one lane,
    // with the nodes after it, over every source's point and aggregation's derived point.
    let hash = "sha256:00000000000000000000000000000000000000000000000000000000000000a7";
    let plan = bound(|value| {
        value["provenance"] = json!({"artifact": {"revision": 7, "hash": hash}});
        let mut aggregate = node_mut(value, "dsd_agg").clone();
        aggregate["id"] = json!("second_agg");
        value["nodes"].as_array_mut().unwrap().push(aggregate);
        for edge in value["edges"].as_array_mut().unwrap() {
            if edge["from"] == "metrics.out" {
                edge["from"] = json!("second_agg.out");
            }
        }
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "metrics.out", "to": "second_agg.in"}));
    });
    let catalog = default_program_resources_catalog();
    let frame = boot_frame();
    let registry = OperatorRegistry::native();
    let derived = DerivedPoints::for_plan(&frame, &plan, &registry);
    let owner = "second_agg".parse::<NodeId>().unwrap();
    let dsd_agg = "dsd_agg".parse::<NodeId>().unwrap();
    // Its derived point is numbered after aggregation's output point.
    let base = frame.derived_point_base();
    assert_eq!(derived.get(&owner), Some(&[AdmissionPointId::new(base)][..]));
    let compiled = compile(&plan, &validated(&plan, &catalog), &registry, &frame, &derived).unwrap();
    let enrich = frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == "metrics_enrich")
        .unwrap();
    assert_eq!(compiled.placement.region_of(&owner), Some((enrich.id, RegionKey::Main)));
    let [main] = compiled.placement.slot(enrich.id).unwrap().regions.as_slice() else {
        panic!("one region");
    };
    assert_eq!(main.state_owner, Some(owner.clone()));
    // The routing after the aggregation runs in its region, and writes the slot's exits.
    assert!(main.nodes.contains(&"metrics_fanout".parse().unwrap()));
    assert!(main.internal.is_empty() && main.progress.is_empty());
    let upstream = &main.upstream;
    let real = frame.admission_point_ids().collect::<Vec<_>>();
    assert_eq!(
        upstream
            .iter()
            .filter(|point| !point.derived)
            .map(|point| point.id)
            .collect::<Vec<_>>(),
        real
    );
    let dsd_agg_point = frame.boot_output_points(&dsd_agg).unwrap()[0];
    assert_eq!(
        upstream
            .iter()
            .filter(|point| point.derived)
            .cloned()
            .collect::<Vec<_>>(),
        vec![UpstreamPoint {
            id: dsd_agg_point,
            clock: ClockId::Derived(dsd_agg.clone()),
            derived: true
        }]
    );
    assert_eq!(
        compiled.placement.admission_upstream(&owner),
        real.iter().copied().collect()
    );
    assert!(compiled.sub_programs.is_empty());
    let program = &compiled.programs[enrich.id.get()];
    assert_eq!(program.state_upstream(), upstream.as_slice());
    assert_eq!(program.output_points(), &[AdmissionPointId::new(base)]);
    assert_eq!(MissingCapability::FrameChanges.to_string(), "stage 10 frame changes");
    assert_eq!(MissingCapability::Restart.to_string(), "a restart");
    assert_eq!(MissingCapability::Restart.stage(), None);
}

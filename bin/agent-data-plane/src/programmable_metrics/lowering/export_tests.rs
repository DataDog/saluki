//! Exports of lowered default programs: every configuration of the matrix exports its policy scope, the export prints
//! and parses back as the same document, and composing it back gives the default program again.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    compose::{compose, receive_artifact, Approval},
    export::export,
    install::diff_with_correspondence,
    ir::{OutputId, ReceivedArtifact, ScopeName},
    operators::{names, OperatorRegistry},
    validate::Catalog as _,
};
use saluki_metric_plan_syntax::{parse, print, PrintOptions};
use serde_json::Value;

use super::{matrix::MATRIX, Lowering};
use crate::programmable_metrics::{
    catalog::POST_ENRICH_SCOPE,
    test_support::{configuration, facts},
};

#[tokio::test]
async fn every_lowered_default_program_exports_and_composes_back_unchanged() {
    let registry = OperatorRegistry::native();
    let scope = ScopeName::new(POST_ENRICH_SCOPE).unwrap();
    for (name, yaml) in MATRIX {
        let config = configuration(yaml).await;
        let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
            .expect("the configuration lowers")
            .expect("the configuration has a metrics pipeline")
            .lower(&config)
            .expect("the configuration lowers");
        let plan = &lowered.plan;
        let bundle = export(plan, &scope, &lowered.catalog.to_document(), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert!(!bundle.artifact.nodes.is_empty(), "{}", name);

        // The base program hash, which ADP reports as `default_program_hash`, is the digest of the exported document
        // without it, and the base records the catalog's fingerprint of every output the export emits to.
        let mut document = serde_json::to_value(&bundle.artifact).unwrap();
        document["base"].as_object_mut().unwrap().remove("program_hash");
        assert_eq!(
            PlanHash::of_value(&document).unwrap(),
            bundle.artifact.base.program_hash,
            "{}",
            name
        );
        let emitted = bundle
            .artifact
            .nodes
            .iter()
            .filter(|node| node.op.as_str() == names::EMIT)
            .map(|node| {
                node.params
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect::<BTreeSet<OutputId>>();
        assert_eq!(
            bundle.artifact.base.outputs.keys().cloned().collect::<BTreeSet<_>>(),
            emitted,
            "{}",
            name
        );
        for (output, fingerprint) in &bundle.artifact.base.outputs {
            assert_eq!(
                lowered.catalog.output(output).map(|declared| &declared.fingerprint),
                Some(fingerprint),
                "{}: {}",
                name,
                output
            );
        }

        // Lowering writes plans in the syntax's canonical order, so the export prints and parses back as itself.
        let printed = print(&bundle.artifact, &PrintOptions::default());
        let parsed = parse(&printed).unwrap_or_else(|diagnostics| panic!("{}:\n{}\n{}", name, diagnostics, printed));
        assert_eq!(parsed.artifact, bundle.artifact, "{}:\n{}", name, printed);

        // Composing the export back, with ADP's catalog, gives the default program, with the same hash, and its diff
        // against the default program is empty.
        let document = serde_json::to_value(&parsed.artifact).unwrap();
        let received = receive_artifact(document).unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert_eq!(
            received.hash,
            ReceivedArtifact::from_value(serde_json::to_value(&bundle.artifact).unwrap())
                .unwrap()
                .hash,
            "{}",
            name
        );
        let approval = Approval {
            revision: 1,
            approved_against: None,
            active_plan_hash: None,
            in_effect: false,
            upgraded: Default::default(),
        };
        let composed = compose(plan, &received, &approval, &lowered.catalog, &registry)
            .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert!(!composed.stale_base && !composed.stale_review, "{}", name);
        assert_eq!(composed.plan.nodes, plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, plan.edges, "{}", name);
        assert_eq!(
            composed.plan.hash(ResolvedValues::Include).unwrap(),
            plan.hash(ResolvedValues::Include).unwrap(),
            "{}",
            name
        );
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        assert!(diff.diff.is_empty(), "{}: {:?}", name, diff.diff);
        assert_eq!(diff.artifact_nodes.len(), bundle.artifact.nodes.len(), "{}", name);
    }
}

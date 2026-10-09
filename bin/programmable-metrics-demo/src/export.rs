//! The export bundle: the default program's policy scope as an artifact, with the catalog it is checked against.
//!
//! ADP serves the bundle at `GET /programmable-metrics/export?scope=post_enrich`, as a JSON object with these members:
//!
//! - `export_version`: `1`;
//! - `scope`: the scope the export covers, `post_enrich`;
//! - `artifact`: the default program's scope as a policy artifact, with its bindings unresolved and its base filled in;
//! - `ir_versions`: `{"current": <version>, "accepted": [<lowest>, <highest>]}`;
//! - `catalog`: the plan crate's `MemoryCatalog` document;
//! - `labels` (optional): display labels, by output ID.
//!
//! Readers ignore unknown fields, so the reader here keeps only the catalog members the plan crate knows (the plan
//! crate's catalog document rejects unknown members) and collects display labels from either `labels` or a `label`
//! member of a catalog output.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    ir::{BindingName, OutputId, ReceivedArtifact},
    validate::{Catalog as _, Liveness, MemoryCatalog},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::settings::Recipient;

/// The export bundle versions this demo reads.
pub const EXPORT_VERSION: u64 = 1;

/// The IR versions an ADP accepts, as its export bundle states them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct IrVersions {
    /// The IR version ADP writes.
    pub current: u32,

    /// The lowest and highest IR versions ADP accepts.
    pub accepted: (u32, u32),
}

/// An export bundle, read.
#[derive(Clone, Debug)]
pub struct ExportBundle {
    /// The scope the export covers.
    pub scope: String,

    /// The default program's scope as an artifact, with the hash of its document.
    pub artifact: ReceivedArtifact,

    /// The IR versions ADP accepts, if the bundle states them.
    pub ir_versions: Option<IrVersions>,

    /// The catalog of outputs, bindings, and scope inputs.
    pub catalog: MemoryCatalog,

    /// Display labels of outputs.
    pub labels: BTreeMap<OutputId, String>,

    /// The output IDs the catalog declares, sorted.
    pub output_ids: Vec<String>,

    /// The binding names the catalog declares, sorted.
    pub binding_names: Vec<String>,
}

impl ExportBundle {
    /// Returns the base program hash the export carries: what an artifact derived from it records as its base.
    pub fn program_hash(&self) -> &PlanHash {
        &self.artifact.artifact.base.program_hash
    }

    /// Reads an export bundle from its JSON document.
    ///
    /// # Errors
    ///
    /// Returns a message saying what is wrong if the bundle has another export version, or its artifact or catalog
    /// cannot be read.
    pub fn from_value(mut value: Value) -> Result<Self, String> {
        let object = value
            .as_object_mut()
            .ok_or_else(|| "The export bundle is not a JSON object.".to_string())?;
        match object.get("export_version").and_then(Value::as_u64) {
            Some(EXPORT_VERSION) => {}
            other => {
                return Err(format!(
                    "The export bundle has export_version {}, but this demo reads version {}.",
                    other.map_or_else(|| "(missing)".to_string(), |version| version.to_string()),
                    EXPORT_VERSION
                ))
            }
        }
        let scope = object
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| "The export bundle has no `scope`.".to_string())?
            .to_string();
        let artifact = object
            .remove("artifact")
            .ok_or_else(|| "The export bundle has no `artifact`.".to_string())?;
        let artifact = ReceivedArtifact::from_value(artifact)
            .map_err(|error| format!("The export bundle's artifact cannot be read: {}", error))?;
        let ir_versions = match object.remove("ir_versions") {
            Some(versions) => Some(
                serde_json::from_value(versions)
                    .map_err(|error| format!("The export bundle's `ir_versions` cannot be read: {}", error))?,
            ),
            None => None,
        };

        let mut labels = BTreeMap::new();
        if let Some(Value::Object(members)) = object.get("labels") {
            for (id, label) in members {
                if let (Ok(id), Some(label)) = (id.parse::<OutputId>(), label.as_str()) {
                    labels.insert(id, label.to_string());
                }
            }
        }
        let catalog = object
            .remove("catalog")
            .ok_or_else(|| "The export bundle has no `catalog`.".to_string())?;
        let catalog = known_catalog_members(catalog, &mut labels);
        let names = |member: &str| -> Vec<String> {
            catalog
                .get(member)
                .and_then(Value::as_object)
                .map(|members| members.keys().cloned().collect())
                .unwrap_or_default()
        };
        let (output_ids, binding_names) = (names("outputs"), names("bindings"));
        let catalog: MemoryCatalog = serde_json::from_value(catalog)
            .map_err(|error| format!("The export bundle's catalog cannot be read: {}", error))?;

        Ok(Self {
            scope,
            artifact,
            ir_versions,
            catalog,
            labels,
            output_ids,
            binding_names,
        })
    }

    /// Returns what the editor shows next to a program: each output with its label, recipients, and the intake it
    /// most likely reaches, and each configuration binding with its current value.
    pub fn legend(&self, intakes: &[Recipient]) -> Legend {
        let outputs = self
            .output_ids
            .iter()
            .filter_map(|id| {
                let output_id = id.parse::<OutputId>().ok()?;
                let declared = self.catalog.output(&output_id)?;
                let label = self.labels.get(&output_id).cloned();
                let recipients = declared.recipients.iter().map(ToString::to_string).collect::<Vec<_>>();
                let intake = intake_of(label.as_deref(), &recipients, intakes);
                Some(LegendOutput {
                    id: id.clone(),
                    label,
                    recipients,
                    intake,
                })
            })
            .collect();
        let bindings = self
            .binding_names
            .iter()
            .filter_map(|name| {
                let binding = self.catalog.binding(&name.parse::<BindingName>().ok()?)?;
                Some(LegendBinding {
                    name: name.clone(),
                    value: binding.value.clone(),
                    live: binding.liveness == Liveness::Live,
                })
            })
            .collect();
        Legend { outputs, bindings }
    }
}

/// What the editor shows next to a program: where its outputs go, and what its configuration bindings hold.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Legend {
    /// The outputs the catalog declares, by ID.
    pub outputs: Vec<LegendOutput>,

    /// The configuration bindings the catalog declares, by name.
    pub bindings: Vec<LegendBinding>,
}

/// One output, as the legend shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LegendOutput {
    /// The output ID.
    pub id: String,

    /// The output's display label, if the export gives one.
    pub label: Option<String>,

    /// The output's physical recipients.
    pub recipients: Vec<String>,

    /// The name of the intake the output most likely reaches, if it can be told.
    pub intake: Option<&'static str>,
}

/// One configuration binding, as the legend shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LegendBinding {
    /// The binding name.
    pub name: String,

    /// The binding's current value, if the export carries it.
    pub value: Option<Value>,

    /// Whether the value can change while ADP runs (otherwise it is read at startup).
    pub live: bool,
}

/// Returns the intake an output most likely reaches, from what the export says about it, or `None`.
///
/// The export does not name endpoints in a fixed form, so this tries, in order: a label or recipient that contains the
/// intake's port (`:17102`), a label equal to the intake's label or name (`Team B`), and a recipient whose identity
/// names the multi-region failover forwarder or the primary endpoint.
fn intake_of(label: Option<&str>, recipients: &[String], intakes: &[Recipient]) -> Option<&'static str> {
    let texts = label
        .into_iter()
        .chain(recipients.iter().map(String::as_str))
        .collect::<Vec<_>>();
    if let Some(intake) = intakes.iter().find(|intake| {
        let port = format!(":{}", intake.port);
        texts.iter().any(|text| text.contains(&port))
    }) {
        return Some(intake.name);
    }
    if let Some(label) = label {
        if let Some(intake) = intakes
            .iter()
            .find(|intake| label.eq_ignore_ascii_case(intake.label) || label.eq_ignore_ascii_case(intake.name))
        {
            return Some(intake.name);
        }
    }
    let named = |name: &str| {
        intakes
            .iter()
            .find(|intake| intake.name == name)
            .map(|intake| intake.name)
    };
    if recipients.iter().any(|recipient| recipient.starts_with("mrf")) {
        return named("mrf");
    }
    if recipients.iter().any(|recipient| recipient.contains("/primary")) {
        return named("primary");
    }
    None
}

/// Keeps the members of a catalog document the plan crate reads, collecting output labels on the way.
fn known_catalog_members(catalog: Value, labels: &mut BTreeMap<OutputId, String>) -> Value {
    let Value::Object(catalog) = catalog else {
        return catalog;
    };
    let mut kept = Map::new();
    for (member, value) in catalog {
        match (member.as_str(), value) {
            ("outputs", Value::Object(outputs)) => {
                let outputs = outputs
                    .into_iter()
                    .map(|(id, output)| {
                        if let (Ok(output_id), Some(label)) =
                            (id.parse::<OutputId>(), output.get("label").and_then(Value::as_str))
                        {
                            labels.entry(output_id).or_insert_with(|| label.to_string());
                        }
                        let output = keep_members(output, &["accepts", "fingerprint", "recipients"]);
                        (id, output)
                    })
                    .collect();
                kept.insert(member, Value::Object(outputs));
            }
            ("bindings", Value::Object(bindings)) => {
                let bindings = bindings
                    .into_iter()
                    .map(|(name, binding)| (name, keep_members(binding, &["type", "liveness", "value"])))
                    .collect();
                kept.insert(member, Value::Object(bindings));
            }
            ("scopes", value) => {
                kept.insert(member, value);
            }
            _ => {}
        }
    }
    Value::Object(kept)
}

fn keep_members(value: Value, members: &[&str]) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(member, _)| members.contains(&member.as_str()))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use saluki_metric_plan::validate::Catalog as _;

    use super::*;

    /// A hand-written export bundle for the demo's configuration: the primary endpoint, two additional endpoints
    /// routed by prefix, and the multi-region failover gateway with mirroring off.
    pub(crate) const DEMO_EXPORT: &str = include_str!("../fixtures/export-post-enrich.json");

    pub(crate) fn demo_export() -> ExportBundle {
        ExportBundle::from_value(serde_json::from_str(DEMO_EXPORT).unwrap()).unwrap()
    }

    #[test]
    fn the_demo_export_reads_with_its_catalog_and_labels() {
        let export = demo_export();
        assert_eq!(export.scope, "post_enrich");
        assert_eq!(export.artifact.artifact.scope.as_str(), "post_enrich");
        assert_eq!(
            export.ir_versions,
            Some(IrVersions {
                current: 1,
                accepted: (1, 1)
            })
        );
        let output = |id: &str| id.parse::<OutputId>().unwrap();
        assert_eq!(
            export
                .catalog
                .output(&output("metrics_routing_encode_0"))
                .unwrap()
                .recipients,
            vec!["dd_out/additional_0/0".parse().unwrap()]
        );
        // The fixture writes one label in `labels` and one as a catalog output's `label` member.
        assert_eq!(
            export.labels.get(&output("dd_metrics_encode")).map(String::as_str),
            Some("Primary")
        );
        assert_eq!(
            export.labels.get(&output("mrf_metrics_encode")).map(String::as_str),
            Some("Multi-region failover")
        );
        assert_eq!(
            export.output_ids,
            [
                "dd_metrics_encode",
                "metrics_routing_encode_0",
                "metrics_routing_encode_1",
                "mrf_metrics_encode"
            ]
        );
        assert_eq!(export.binding_names.len(), 7);
        assert_eq!(
            export.program_hash().to_string(),
            "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"
        );
    }

    #[test]
    fn the_legend_says_where_each_output_goes_and_what_each_binding_holds() {
        let intakes = crate::settings::DemoPorts::default().recipients();
        let legend = demo_export().legend(&intakes);
        let outputs = legend
            .outputs
            .iter()
            .map(|output| (output.id.as_str(), output.label.as_deref(), output.intake))
            .collect::<Vec<_>>();
        assert_eq!(
            outputs,
            [
                ("dd_metrics_encode", Some("Primary"), Some("primary")),
                ("metrics_routing_encode_0", Some("Team B"), Some("team-b")),
                ("metrics_routing_encode_1", Some("Team C"), Some("team-c")),
                ("mrf_metrics_encode", Some("Multi-region failover"), Some("mrf")),
            ]
        );
        assert_eq!(legend.outputs[1].recipients, ["dd_out/additional_0/0"]);
        let prefixes = legend
            .bindings
            .iter()
            .find(|binding| binding.name == "endpoint_routing.group.b0b0b0b0.prefixes")
            .unwrap();
        assert_eq!(prefixes.value, Some(serde_json::json!(["demo.checkout."])));
        assert!(!prefixes.live);
        assert_eq!(legend.bindings.len(), 7);

        // An endpoint named by its URL in a label or a recipient is matched by port, before anything else.
        let by_url = super::intake_of(
            Some("http://127.0.0.1:17103"),
            &["dd_out/primary/0".to_string()],
            &intakes,
        );
        assert_eq!(by_url, Some("team-c"));
        assert_eq!(
            super::intake_of(Some("Somewhere"), &["x/y".to_string()], &intakes),
            None
        );
    }

    #[test]
    fn bundles_of_other_versions_or_without_parts_are_refused_with_the_reason() {
        let bundle = || serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        type Mutation = fn(&mut Value);
        let cases: [(&str, Mutation); 4] = [
            ("export_version 2", |v| v["export_version"] = 2.into()),
            ("has no `artifact`", |v| {
                v.as_object_mut().unwrap().remove("artifact");
            }),
            ("has no `catalog`", |v| {
                v.as_object_mut().unwrap().remove("catalog");
            }),
            ("artifact cannot be read", |v| v["artifact"]["ir_version"] = 99.into()),
        ];
        for (reason, mutate) in cases {
            let mut value = bundle();
            mutate(&mut value);
            let error = ExportBundle::from_value(value).unwrap_err();
            assert!(error.contains(reason), "{}: {}", reason, error);
        }
    }
}

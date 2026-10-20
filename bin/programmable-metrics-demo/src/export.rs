//! The export bundle: the default program, the whole metrics path after its sources, as an artifact, with the catalog
//! it is checked against.
//!
//! ADP serves the bundle at `GET /programmable-metrics/export`, as a JSON object with these members:
//!
//! - `export_version`: `2`;
//! - `artifact`: the default program as a policy artifact, with its bindings unresolved;
//! - `ir_versions`: `{"current": <version>, "accepted": [<lowest>, <highest>]}`;
//! - `catalog`: the plan crate's `MemoryCatalog` document, with the recipient directory under `recipients`;
//! - `labels` (optional): display labels, by output ID;
//! - `annotations` (optional): notes for tools, by node and by binding, which the editor prints as comments.
//!
//! Readers ignore unknown fields, so the reader here keeps only the catalog members the plan crate knows (the plan
//! crate's catalog document rejects unknown members) and collects display labels from either `labels` or a `label`
//! member of a catalog output.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    export::{program_hash, Annotations, ExportForm},
    ir::{BindingName, OutputId, ReceivedArtifact, RecipientName},
    validate::{Catalog as _, CatalogRecipient, Liveness, MemoryCatalog, TelemetryContract},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::settings::Recipient;

/// The export bundle versions this demo reads.
pub const EXPORT_VERSION: u64 = 2;

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
    /// The default program as an artifact, with the hash of its document.
    pub artifact: ReceivedArtifact,

    /// The program hash of the artifact ([`program_hash`]): what an unmodified copy hashes to when ADP receives it.
    pub program_hash: PlanHash,

    /// How the artifact writes the default program: `bound`, if the bundle does not say.
    pub form: ExportForm,

    /// The IR versions ADP accepts, if the bundle states them.
    pub ir_versions: Option<IrVersions>,

    /// The catalog of outputs, bindings, and recipients.
    pub catalog: MemoryCatalog,

    /// Display labels of outputs.
    pub labels: BTreeMap<OutputId, String>,

    /// What ADP says each node of its default program and each binding is, which the editor prints as comments.
    pub annotations: Annotations,

    /// The output IDs the catalog declares, sorted.
    pub output_ids: Vec<String>,

    /// The binding names the catalog declares, sorted.
    pub binding_names: Vec<String>,
}

impl ExportBundle {
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
        let artifact = object
            .remove("artifact")
            .ok_or_else(|| "The export bundle has no `artifact`.".to_string())?;
        let artifact = ReceivedArtifact::from_value(artifact)
            .map_err(|error| format!("The export bundle's artifact cannot be read: {}", error))?;
        let program_hash = program_hash(&artifact.artifact)
            .map_err(|error| format!("The export bundle's artifact cannot be hashed: {}", error))?;
        let form = match object.remove("form") {
            Some(form) => serde_json::from_value(form)
                .map_err(|error| format!("The export bundle's `form` cannot be read: {}", error))?,
            None => ExportForm::default(),
        };
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
        // Notes that do not read are left out: they are only comments.
        let annotations = object
            .remove("annotations")
            .and_then(|annotations| serde_json::from_value(annotations).ok())
            .unwrap_or_default();
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
            artifact,
            program_hash,
            form,
            ir_versions,
            catalog,
            labels,
            annotations,
            output_ids,
            binding_names,
        })
    }

    /// Returns what the exported program writes after `emit` to deliver to `output`, if one of its `emit`s addresses it
    /// by the names of its recipients: `to [<name>, ...]`, each name bare if it is an identifier and quoted otherwise,
    /// as the editor prints it.
    fn emit_target(&self, output: &OutputId) -> Option<String> {
        self.artifact.artifact.nodes.iter().find_map(|node| {
            let names = node.params.get("to")?.as_array()?;
            let mut recipients = names
                .iter()
                .filter_map(|name| self.catalog.recipient(&name.as_str()?.parse().ok()?))
                .flat_map(|entry| entry.recipients.iter().cloned())
                .collect::<Vec<_>>();
            recipients.sort();
            recipients.dedup();
            if self.catalog.output_for(&recipients).as_ref() != Some(output) {
                return None;
            }
            let names = names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| {
                    if saluki_metric_plan::ir::is_identifier(name) {
                        name.to_string()
                    } else {
                        // Quoted as the syntax writes a string, with its quotes and backslashes escaped.
                        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{}\"", name))
                    }
                })
                .collect::<Vec<_>>();
            Some(format!("to [{}]", names.join(", ")))
        })
    }

    /// Returns what the editor shows next to a program: each output with its label, recipients, and the intake it
    /// most likely reaches, the outputs ADP added for a program's recipient sets beside the native ones, and each configuration
    /// binding with its current value.
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
                let emit = self.emit_target(&output_id).unwrap_or_else(|| id.clone());
                Some(LegendOutput {
                    id: id.clone(),
                    label,
                    recipients,
                    intake,
                    emit,
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
    pub intake: Option<String>,

    /// What the program's source writes after `emit` to deliver to the output: `to [<recipient>, ...]`, the names of
    /// its recipients, where the export addresses it so, or else its ID.
    pub emit: String,
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
fn intake_of(label: Option<&str>, recipients: &[String], intakes: &[Recipient]) -> Option<String> {
    let texts = label
        .into_iter()
        .chain(recipients.iter().map(String::as_str))
        .collect::<Vec<_>>();
    if let Some(intake) = intakes.iter().find(|intake| {
        let port = format!(":{}", intake.port);
        texts.iter().any(|text| text.contains(&port))
    }) {
        return Some(intake.name.clone());
    }
    if let Some(label) = label {
        if let Some(intake) = intakes
            .iter()
            .find(|intake| label.eq_ignore_ascii_case(&intake.label) || label.eq_ignore_ascii_case(&intake.name))
        {
            return Some(intake.name.clone());
        }
    }
    let named = |name: &str| {
        intakes
            .iter()
            .find(|intake| intake.name == name)
            .map(|intake| intake.name.clone())
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
                        let output = keep_members(output, &["accepts", "recipients"]);
                        (id, output)
                    })
                    .collect();
                kept.insert(member, Value::Object(outputs));
            }
            ("bindings", Value::Object(bindings)) => {
                let bindings = bindings
                    .into_iter()
                    .map(|(name, binding)| (name, keep_members(binding, &["type", "liveness", "class", "value"])))
                    .collect();
                kept.insert(member, Value::Object(bindings));
            }
            // The recipient directory, which the editor only lists: an entry that does not read is left out, so a
            // directory from a newer ADP loses only comments.
            ("recipients", Value::Object(entries)) => {
                let entries = entries
                    .into_iter()
                    .filter(|(name, _)| name.parse::<RecipientName>().is_ok())
                    .map(|(name, entry)| (name, keep_members(entry, &["recipients", "host", "endpoint"])))
                    .filter(|(_, entry)| serde_json::from_value::<CatalogRecipient>(entry.clone()).is_ok())
                    .collect();
                kept.insert(member, Value::Object(entries));
            }
            // Whether ADP runs at most one aggregation per program, which validation then checks, and whether a
            // published program may bind only facts and signals, which composition then checks.
            ("single_aggregation", value) | ("detached_programs", value) => {
                kept.insert(member, value);
            }
            // The nodes ADP reads series from by name, which validation warns about a program lacking: a contract a
            // newer ADP writes that does not read is left out, losing only the warnings.
            ("telemetry_contract", value) if serde_json::from_value::<TelemetryContract>(value.clone()).is_ok() => {
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

    /// A hand-written export bundle for the demo's configuration, trimmed to the policy region, fed straight from the
    /// `dogstatsd` source: the primary endpoint, two additional endpoints routed by prefix, and the multi-region
    /// failover gateway with mirroring off.
    pub(crate) const DEMO_EXPORT: &str = include_str!("../fixtures/export-metrics.json");

    pub(crate) fn demo_export() -> ExportBundle {
        ExportBundle::from_value(serde_json::from_str(DEMO_EXPORT).unwrap()).unwrap()
    }

    #[test]
    fn the_catalogs_telemetry_contract_is_read_when_it_reads() {
        let mut document = serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        let entry = serde_json::json!({"series": "aggregator.flush", "node": "dsd_agg", "operator": "aggregate",
                                       "reports": "aggregate_flushed_total"});
        document["catalog"]["telemetry_contract"] = serde_json::json!({"entries": [entry]});
        let export = ExportBundle::from_value(document.clone()).unwrap();
        assert_eq!(export.catalog.telemetry_contract().unwrap().entries.len(), 1);
        document["catalog"]["telemetry_contract"]["entries"][0]["future_member"] = serde_json::json!(1);
        let export = ExportBundle::from_value(document).unwrap();
        assert!(export.catalog.telemetry_contract().is_none());
    }

    #[test]
    fn the_demo_export_reads_with_its_catalog_and_labels() {
        let export = demo_export();
        assert!(export.catalog.single_aggregation());
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
            vec!["dd_out/additional/0/http://127.0.0.1:17102".parse().unwrap()]
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
        assert_eq!(export.binding_names.len(), 2);
        // The program hash is that of the artifact, which ADP's status reports as its default program's.
        assert_eq!(export.program_hash, program_hash(&export.artifact.artifact).unwrap());
    }

    #[test]
    fn the_legend_says_where_each_output_goes_and_what_each_binding_holds() {
        let intakes = crate::settings::DemoPorts::default().recipients();
        let legend = demo_export().legend(&intakes);
        let outputs = legend
            .outputs
            .iter()
            .map(|output| (output.id.as_str(), output.label.as_deref(), output.intake.as_deref()))
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
        assert_eq!(
            legend.outputs[1].recipients,
            ["dd_out/additional/0/http://127.0.0.1:17102"]
        );
        // Only facts and signals are bound: the routing groups' lists are literals of the program.
        let mirroring = legend
            .bindings
            .iter()
            .find(|binding| binding.name == "multi_region_failover.metric_mirroring.enabled")
            .unwrap();
        assert_eq!(mirroring.value, Some(serde_json::json!(false)));
        assert!(mirroring.live);
        assert_eq!(legend.bindings.len(), 2);

        // An endpoint named by its URL in a label or a recipient is matched by port, before anything else.
        let by_url = super::intake_of(
            Some("http://127.0.0.1:17103"),
            &["dd_out/primary/0".to_string()],
            &intakes,
        );
        assert_eq!(by_url.as_deref(), Some("team-c"));
        assert_eq!(
            super::intake_of(Some("Somewhere"), &["x/y".to_string()], &intakes),
            None
        );
    }

    #[test]
    fn the_legend_lists_implied_outputs_beside_native_ones_with_the_intake_they_reach() {
        let mut bundle = serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        bundle["catalog"]["outputs"]["implied_to_d_true_0a1b2c3d"] = serde_json::json!({
            "accepts": {"kinds": "all", "timestamps": "mixed"},
            "recipients": ["dd_out/additional/0/http://127.0.0.1:17105"]
        });
        bundle["labels"]["implied_to_d_true_0a1b2c3d"] = "Implied output of emit to_d_true".into();
        let export = ExportBundle::from_value(bundle).unwrap();
        let mut intakes = crate::settings::DemoPorts::default().recipients();
        intakes.push(Recipient::added('D', 17105));
        let legend = export.legend(&intakes);
        let outputs = legend
            .outputs
            .iter()
            .map(|output| (output.id.as_str(), output.intake.as_deref()))
            .collect::<Vec<_>>();
        assert!(
            outputs.contains(&("dd_metrics_encode", Some("primary"))),
            "{:?}",
            outputs
        );
        assert!(
            outputs.contains(&("implied_to_d_true_0a1b2c3d", Some("dest-d"))),
            "{:?}",
            outputs
        );
    }

    #[test]
    fn a_directory_or_notes_the_demo_cannot_read_lose_only_their_comments() {
        let mut bundle = serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        let directory = &mut bundle["catalog"]["recipients"];
        // A member the demo does not know is left out, and an entry that does not read is left out alone.
        directory["primary"]["label"] = "Primary".into();
        directory["mrf"]["source"] = "fleet".into();
        directory["http://127.0.0.1:17103"]["recipients"] = "not a list".into();
        bundle["annotations"] = serde_json::json!({"nodes": "not a map"});
        let export = ExportBundle::from_value(bundle).unwrap();
        let names = export
            .catalog
            .recipients()
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["http://127.0.0.1:17102", "mrf", "primary"]);
        assert!(export.annotations.is_empty());
        assert_eq!(export.output_ids.len(), 4);
    }

    #[test]
    fn bundles_of_other_versions_or_without_parts_are_refused_with_the_reason() {
        let bundle = || serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        type Mutation = fn(&mut Value);
        let cases: [(&str, Mutation); 4] = [
            ("export_version 1", |v| v["export_version"] = 1.into()),
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

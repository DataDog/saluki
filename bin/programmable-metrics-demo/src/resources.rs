//! Resources artifacts in the demo: the team D template the Resources tab starts from, and what the demo reads from a
//! draft before it is published.
//!
//! A resources artifact declares recipients (endpoints of a delivery host, each with a credential the operator made
//! available to resources artifacts) and outputs that deliver to them, which policies then emit to as
//! `resources_<name>`. ADP validates the document when it arrives; the demo checks a draft with the same function
//! ([`saluki_metric_plan::resources::validate`]), against what it knows of ADP's hosts ([`DeclaredHosts`]), and
//! computes the fingerprint of each output it declares ([`declared_outputs`]). A policy that emits to a declared output
//! records that fingerprint in its base: the editor takes it from ADP's export once ADP declares the output, and from
//! the draft until then, so that a policy can be published before the resources it waits for (it is `pending`
//! meanwhile).

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    ir::{BindingName, HostId, InputName, OutputId, ScopeName, Selector},
    model::PortType,
    resources::{
        credential_name, declared_recipient_id, fingerprint, validate, ResourcesArtifact, CREDENTIALS_NAMESPACE,
        RESOURCES_VERSION,
    },
    validate::{Catalog, CatalogBinding, CatalogOutput, HostSchema},
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::settings::{Recipient, TEAM_D_CREDENTIAL};

/// The delivery host that accepts recipients a resources artifact declares: `dd_out`, the only one in this version.
pub const DECLARED_RECIPIENT_HOST: &str = "dd_out";

/// The output the team D template declares.
pub const TEAM_D_OUTPUT: &str = "resources_team_d";

/// Returns the team D template: one recipient, team D's intake, with the credential the launcher declares for
/// resources artifacts, and one output delivering to it, [`TEAM_D_OUTPUT`], labeled as the intake is.
///
/// Its delivery policy shrinks the queue to 4 MiB, never spills to disk, and attempts queued work until the drain
/// deadline when the recipient retires, so that a paused intake shows a drain that reaches its deadline.
pub fn team_d_template(team_d: &Recipient) -> Value {
    json!({
        "resources_version": RESOURCES_VERSION,
        "recipients": [{
            "name": "team_d",
            "host": DECLARED_RECIPIENT_HOST,
            "endpoint": team_d.url(),
            "credential": format!("{}{}", CREDENTIALS_NAMESPACE, TEAM_D_CREDENTIAL),
            "protocol": { "v3_series": false, "v3_sketches": false },
            "delivery": { "queue_bytes": 4_194_304, "spill": "never", "on_retire": "attempt" },
        }],
        "outputs": [{ "name": "team_d", "recipients": ["team_d"], "accepts": "any", "label": team_d.label }],
    })
}

/// Returns a document as the Resources tab's editor shows it: indented JSON, ending with a newline.
pub fn document_text(document: &Value) -> String {
    let mut text = serde_json::to_string_pretty(document).unwrap_or_else(|_| document.to_string());
    text.push('\n');
    text
}

/// What ADP declares about its delivery hosts, as far as resources artifacts need it: `dd_out` accepts declared
/// recipients. It declares no output, binding, or scope, which a resources artifact never refers to.
pub struct DeclaredHosts;

impl Catalog for DeclaredHosts {
    fn output(&self, _id: &OutputId) -> Option<&CatalogOutput> {
        None
    }

    fn binding(&self, _name: &BindingName) -> Option<&CatalogBinding> {
        None
    }

    fn scope_input(&self, _scope: &ScopeName, _input: &InputName) -> Option<PortType> {
        None
    }

    fn host(&self, host: &HostId) -> Option<HostSchema> {
        // The parameter tables are ADP's; a resources artifact sets none of them.
        (host.as_str() == DECLARED_RECIPIENT_HOST).then_some(HostSchema {
            encoder: &[],
            settings: &[],
            protocol: &[],
            declared_recipients: true,
        })
    }
}

/// One output a resources artifact declares, as the Resources tab and the editor show it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeclaredOutputView {
    /// The output's ID, `resources_<name>`.
    pub id: String,

    /// Its display label, if the document gives one.
    pub label: Option<String>,

    /// The identities of the recipients it delivers to, sorted.
    pub recipients: Vec<String>,

    /// Its fingerprint, which a policy that emits to it records in its base.
    pub fingerprint: String,
}

/// The outcome of checking a resources draft.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ResourcesCheck {
    /// Whether the draft can be published: it is a JSON document with a canonical form, which gives it a hash.
    pub ok: bool,

    /// The hash a publish approves: the plan crate's hash of the document, which ADP recomputes on receipt.
    pub artifact_hash: Option<String>,

    /// Why the draft cannot be published, if it cannot.
    pub error: Option<String>,

    /// Whether the document validates, as ADP validates it on receipt.
    pub valid: bool,

    /// Why validation refuses the document, if it does: ADP then rejects it. It can still be published, to show the
    /// rejection.
    pub validation_error: Option<String>,

    /// The outputs the document declares, with their fingerprints, once it validates.
    pub outputs: Vec<DeclaredOutputView>,
}

/// Checks a resources draft: reads it as JSON, hashes it, validates it as ADP does, and computes the fingerprints of
/// the outputs it declares.
///
/// Returns the outcome and, if the draft can be published, the document with its hash.
pub fn check(text: &str) -> (ResourcesCheck, Option<(Value, PlanHash)>) {
    let document = match serde_json::from_str::<Value>(text) {
        Ok(document) => document,
        Err(error) => {
            let check = ResourcesCheck {
                error: Some(format!("The resources draft is not JSON: {}", error)),
                ..ResourcesCheck::default()
            };
            return (check, None);
        }
    };
    let hash = match PlanHash::of_value(&document) {
        Ok(hash) => hash,
        Err(error) => {
            let check = ResourcesCheck {
                error: Some(format!(
                    "The resources draft has no canonical form, so no hash: {}",
                    error
                )),
                ..ResourcesCheck::default()
            };
            return (check, None);
        }
    };
    let mut check = ResourcesCheck {
        ok: true,
        artifact_hash: Some(hash.to_string()),
        ..ResourcesCheck::default()
    };
    match validate(document.clone(), &DeclaredHosts)
        .map_err(|error| error.to_string())
        .and_then(|artifact| declared_outputs(&artifact))
    {
        Ok(outputs) => {
            check.valid = true;
            check.outputs = outputs;
        }
        Err(error) => check.validation_error = Some(error),
    }
    (check, Some((document, hash)))
}

/// The fingerprints of the outputs a resources draft declares, for the editor.
#[derive(Clone, Debug, Default)]
pub struct DraftFingerprints {
    /// The fingerprint of each output the draft declares.
    pub fingerprints: BTreeMap<OutputId, PlanHash>,

    /// Why the draft gives none, when it was given and cannot be read or does not validate.
    pub error: Option<String>,
}

impl DraftFingerprints {
    /// Reads the fingerprints of the outputs a resources draft declares. A draft that cannot be read or does not
    /// validate gives none, and says why.
    pub fn of_draft(text: &str) -> Self {
        let (check, _) = check(text);
        match (check.error, check.validation_error) {
            (Some(error), _) | (None, Some(error)) => Self {
                fingerprints: BTreeMap::new(),
                error: Some(error),
            },
            (None, None) => Self {
                fingerprints: check
                    .outputs
                    .iter()
                    .filter_map(|output| Some((output.id.parse().ok()?, output.fingerprint.parse().ok()?)))
                    .collect(),
                error: None,
            },
        }
    }
}

/// Returns the outputs a validated resources artifact declares, each with its recipients' identities and its
/// fingerprint (version 2: its host, what it accepts, and its sorted recipient identities).
///
/// # Errors
///
/// Returns the first output whose recipients cannot be resolved: a recipient the artifact does not declare, a
/// credential outside the namespace resources artifacts may use, or recipients on more than one host. Validation
/// refuses each of these, so for a validated artifact this returns every output.
pub fn declared_outputs(artifact: &ResourcesArtifact) -> Result<Vec<DeclaredOutputView>, String> {
    artifact
        .outputs
        .iter()
        .map(|output| {
            let id = output
                .id()
                .map_err(|error| format!("output `{}`: {}", output.name, error))?;
            let mut host = None;
            let mut recipients = Vec::new();
            for name in &output.recipients {
                let recipient = artifact
                    .recipients
                    .iter()
                    .find(|recipient| &recipient.name == name)
                    .ok_or_else(|| format!("output `{}` names recipient `{}`, which is not declared", id, name))?;
                let credential = credential_name(&recipient.credential).ok_or_else(|| {
                    format!(
                        "recipient `{}` uses credential `{}`, which is not a `{}<name>` reference",
                        name, recipient.credential, CREDENTIALS_NAMESPACE
                    )
                })?;
                match &host {
                    Some(host) if host != &recipient.host => {
                        return Err(format!("output `{}` names recipients on more than one host", id))
                    }
                    _ => host = Some(recipient.host.clone()),
                }
                recipients.push(declared_recipient_id(&recipient.host, credential, &recipient.endpoint));
            }
            let host = host.ok_or_else(|| format!("output `{}` names no recipient", id))?;
            recipients.sort();
            recipients.dedup();
            let selector = Selector::Declared(recipients.clone());
            Ok(DeclaredOutputView {
                id: id.to_string(),
                label: output.label.clone(),
                recipients: recipients.iter().map(ToString::to_string).collect(),
                fingerprint: fingerprint(&host, output.accepts.port_type(), &selector).to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DemoPorts;

    fn team_d() -> Recipient {
        DemoPorts::default()
            .recipients()
            .into_iter()
            .find(|recipient| recipient.name == "team-d")
            .unwrap()
    }

    fn template() -> ResourcesArtifact {
        serde_json::from_value(team_d_template(&team_d())).unwrap()
    }

    #[test]
    fn the_team_d_template_declares_the_intake_with_the_launchers_credential() {
        let artifact = template();
        assert_eq!(artifact.resources_version, RESOURCES_VERSION);
        let recipient = &artifact.recipients[0];
        assert_eq!(
            (
                recipient.host.as_str(),
                recipient.endpoint.as_str(),
                recipient.credential.as_str()
            ),
            ("dd_out", "http://127.0.0.1:17105", "credentials/team_d")
        );
        let delivery = recipient.delivery.unwrap();
        assert_eq!(delivery.queue_bytes, Some(4_194_304));
        let output = &artifact.outputs[0];
        assert_eq!(output.id().unwrap().as_str(), TEAM_D_OUTPUT);
        assert_eq!(output.label.as_deref(), Some("Team D"));
        // The editor shows the document as indented JSON that reads back the same.
        let text = document_text(&team_d_template(&team_d()));
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            team_d_template(&team_d())
        );
    }

    /// The fingerprints are the digests of the canonical documents written out by hand here (members sorted, no
    /// whitespace), computed with `shasum -a 256`, independently of the code under test.
    #[test]
    fn a_declared_outputs_fingerprint_covers_its_host_and_its_sorted_recipients() {
        let outputs = declared_outputs(&template()).unwrap();
        // {"accepts":{"kinds":"all","timestamps":"mixed"},"host":"dd_out",
        //  "selector":{"declared":["dd_out/declared/team_d/http://127.0.0.1:17105"]},"v":2}
        assert_eq!(
            outputs,
            [DeclaredOutputView {
                id: "resources_team_d".to_string(),
                label: Some("Team D".to_string()),
                recipients: vec!["dd_out/declared/team_d/http://127.0.0.1:17105".to_string()],
                fingerprint: "sha256:361f843bc9a6b25663cc6da865dd3af0daf04bb8890e3717ae98d7d30bb1c838".to_string(),
            }]
        );

        // A second output delivering to team E and team D, named in that order: the selector sorts them.
        let mut document = team_d_template(&team_d());
        document["recipients"].as_array_mut().unwrap().push(json!({
            "name": "team_e", "host": "dd_out", "endpoint": "https://e.example.com", "credential": "credentials/team_e"
        }));
        document["outputs"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "both", "recipients": ["team_e", "team_d", "team_e"] }));
        let artifact = serde_json::from_value::<ResourcesArtifact>(document).unwrap();
        let both = &declared_outputs(&artifact).unwrap()[1];
        assert_eq!(both.id, "resources_both");
        assert_eq!(
            both.recipients,
            [
                "dd_out/declared/team_d/http://127.0.0.1:17105",
                "dd_out/declared/team_e/https://e.example.com"
            ]
        );
        // {"accepts":{...},"host":"dd_out","selector":{"declared":["dd_out/declared/team_d/http://127.0.0.1:17105",
        //  "dd_out/declared/team_e/https://e.example.com"]},"v":2}
        assert_eq!(
            both.fingerprint,
            "sha256:b89cad5700638679428a88f84402a07bf1af503cd043e823f14f7e1527ec4c1f"
        );
    }

    #[test]
    fn a_long_endpoint_is_written_as_its_digest_in_the_recipients_identity() {
        let host = HostId::new("dd_out").unwrap();
        // 423 bytes, over the 400 an identity spells out. `printf '%s' <endpoint> | shasum -a 256`, first 16 digits.
        let endpoint = format!("http://127.0.0.1:17105/{}", "a".repeat(400));
        assert_eq!(
            declared_recipient_id(&host, "team_d", &endpoint).as_str(),
            "dd_out/declared/team_d/sha256:43af4262ad897f82"
        );
        assert_eq!(
            declared_recipient_id(&host, "team_d", "http://127.0.0.1:17105").as_str(),
            "dd_out/declared/team_d/http://127.0.0.1:17105"
        );
    }

    #[test]
    fn outputs_whose_recipients_cannot_be_resolved_are_refused_with_the_reason() {
        type Mutation = fn(&mut Value);
        let cases: [(&str, Mutation); 3] = [
            ("names recipient `team_x`, which is not declared", |document| {
                document["outputs"][0]["recipients"] = json!(["team_x"]);
            }),
            ("not a `credentials/<name>` reference", |document| {
                document["recipients"][0]["credential"] = json!("api_key");
            }),
            ("more than one host", |document| {
                document["recipients"].as_array_mut().unwrap().push(json!({
                    "name": "failover", "host": "mrf_dd_out", "endpoint": "http://127.0.0.1:17104",
                    "credential": "credentials/team_d"
                }));
                document["outputs"][0]["recipients"] = json!(["team_d", "failover"]);
            }),
        ];
        for (reason, mutate) in cases {
            let mut document = team_d_template(&team_d());
            mutate(&mut document);
            let artifact = serde_json::from_value::<ResourcesArtifact>(document).unwrap();
            let error = declared_outputs(&artifact).unwrap_err();
            assert!(error.contains(reason), "{}: {}", reason, error);
        }
    }

    #[test]
    fn a_draft_is_publishable_when_it_has_a_hash_and_validation_decides_what_adp_will_say() {
        let (refused, document) = check("{ not json");
        assert!(!refused.ok && document.is_none());
        assert!(refused.error.unwrap().contains("not JSON"));

        // A document ADP refuses can still be published, to show the refusal: it has a hash.
        let wrong_version = json!({ "resources_version": 99 });
        let (checked, document) = check(&wrong_version.to_string());
        assert!(checked.ok);
        let (document, hash) = document.unwrap();
        assert_eq!(document, wrong_version);
        assert_eq!(hash, PlanHash::of_value(&wrong_version).unwrap());
        assert_eq!(checked.artifact_hash, Some(hash.to_string()));
        assert!(!checked.valid && checked.outputs.is_empty());
        assert!(checked.validation_error.is_some());

        let fingerprints = DraftFingerprints::of_draft(&wrong_version.to_string());
        assert!(fingerprints.fingerprints.is_empty() && fingerprints.error.is_some());
        assert!(DraftFingerprints::of_draft("[").error.unwrap().contains("not JSON"));
    }

    #[test]
    fn only_the_primary_delivery_host_accepts_declared_recipients() {
        let schema = DeclaredHosts.host(&HostId::new("dd_out").unwrap()).unwrap();
        assert!(schema.declared_recipients);
        assert!(DeclaredHosts.host(&HostId::new("mrf_dd_out").unwrap()).is_none());
    }
}

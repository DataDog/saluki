//! The resources product: reading its snapshots into what the installer core takes, and what the core holds of it.
//!
//! The core stays free of I/O and of host knowledge, so ADP reads each snapshot of the `resources` product before the
//! core sees it ([`read_snapshot`]): its shape (one config), each config's transport hash and envelope, the artifact
//! hash, and the document itself, validated against ADP's catalog. The core then applies the product's revision rules,
//! and keeps the documents it accepted ([`HeldResources`]): the credentials every one of them references are what it is
//! told is missing before each lowering, and the one lowering takes records its revision and hash as the provenance of
//! the resources it declares.

use saluki_metric_plan::{
    canonical::PlanHash,
    channel::{ArtifactConfig, ArtifactSnapshot, Envelope},
    install::ResourcesEvent,
    ir::{ArtifactRevision, RecipientId},
    resources::{ResourcesArtifact, ResourcesError},
    validate::Catalog,
};
use serde_json::Value;

/// Validates a resources document, as received, against a catalog: [`saluki_metric_plan::resources::validate`], or a
/// test double.
pub(super) type Validate = fn(Value, &dyn Catalog) -> Result<ResourcesArtifact, ResourcesError>;

/// Reads a snapshot of the resources product into the events the installer core takes.
///
/// An empty snapshot revokes every declared resource. A snapshot with more than one config refuses them all: the
/// product holds one. The one config is refused if its transport hash, envelope, or artifact hash does not check out,
/// and otherwise delivered with its document as `validate` returns it against `catalog`, or why the document is not
/// valid.
pub(super) fn read_snapshot(
    snapshot: &ArtifactSnapshot, catalog: &dyn Catalog, validate: Validate,
) -> Vec<ResourcesEvent> {
    if snapshot.revokes() {
        return vec![ResourcesEvent::Revoked];
    }
    if let [config] = snapshot.configs.as_slice() {
        return vec![read_config(config, catalog, validate)];
    }
    let count = snapshot.configs.len();
    snapshot
        .configs
        .iter()
        .map(|config| ResourcesEvent::Refused {
            config: config.id.clone(),
            version: config.version,
            reason: format!(
                "the resources product holds {} configs, and holds at most one: every one is refused until it holds \
                 one",
                count
            ),
        })
        .collect()
}

/// Reads the one config of a resources snapshot.
fn read_config(config: &ArtifactConfig, catalog: &dyn Catalog, validate: Validate) -> ResourcesEvent {
    let refused = |reason: String| ResourcesEvent::Refused {
        config: config.id.clone(),
        version: config.version,
        reason,
    };
    if let Err(mismatch) = config.verify_transport_hash() {
        return refused(mismatch.to_string());
    }
    let envelope = match Envelope::parse(&config.payload) {
        Ok(envelope) => envelope,
        Err(error) => return refused(error.to_string()),
    };
    // The artifact hash covers the document as received, in its canonical form, as a policy artifact's does.
    match PlanHash::of_value(&envelope.artifact) {
        Ok(hash) if hash == envelope.artifact_hash => {}
        Ok(hash) => {
            return refused(format!(
                "artifact hash mismatch: the envelope declares {}, but the artifact as received hashes to {}",
                envelope.artifact_hash, hash
            ))
        }
        Err(error) => return refused(format!("the artifact has no canonical form: {}", error)),
    }
    ResourcesEvent::Delivered {
        config: config.id.clone(),
        version: config.version,
        revision: envelope.revision,
        artifact_hash: envelope.artifact_hash,
        document: validate(envelope.artifact, catalog).map_err(|error| error.to_string()),
    }
}

/// The resources artifacts the installer core holds, with the revision and hash each was delivered with.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HeldResources {
    /// Every artifact the core holds, newest first: the newest accepted revision, and, while that one is rejected or
    /// pending, the revision in use when it was accepted.
    pub(super) held: Vec<(ResourcesArtifact, ArtifactRevision)>,

    /// The artifact lowering declares resources from, one of those held.
    pub(super) accepted: Option<(ResourcesArtifact, ArtifactRevision)>,
}

impl HeldResources {
    /// Returns the name a held artifact declares the recipient `id` under: the one lowering declares resources from,
    /// else the newest that declares it.
    ///
    /// The name names the recipient's task and telemetry; a recipient declared again under another name keeps the
    /// identity, and the queue, it had.
    pub(super) fn declared_name(&self, id: &RecipientId) -> Option<String> {
        let accepted = self.accepted.iter().map(|(artifact, _)| artifact);
        let held = self.held.iter().map(|(artifact, _)| artifact);
        accepted.chain(held).find_map(|artifact| {
            artifact
                .recipients
                .iter()
                .find_map(|recipient| (recipient.id().ok()? == *id).then(|| recipient.name.clone()))
        })
    }
}

#[cfg(test)]
mod tests {
    use saluki_metric_plan::{
        channel::{ArtifactConfigId, ArtifactProduct},
        ir::{CredentialRef, HostId},
        resources::{self, DeclaredAccepts, DeclaredOutput, DeclaredProtocol, DeclaredRecipient, RESOURCES_VERSION},
        validate::MemoryCatalog,
    };
    use serde_json::json;

    use super::*;

    fn config(id: &str, payload: &str) -> ArtifactConfig {
        ArtifactConfig {
            id: ArtifactConfigId::new(id),
            version: 3,
            sha256: ArtifactConfig::transport_hash(payload),
            payload: payload.to_string(),
        }
    }

    fn envelope(revision: u64, artifact: &Value) -> String {
        let hash = PlanHash::of_value(artifact).unwrap();
        json!({"revision": revision, "artifact_hash": hash, "artifact": artifact}).to_string()
    }

    /// A resources document as an operator writes it: its endpoint is not in canonical form.
    fn document() -> Value {
        json!({
            "resources_version": 1,
            "recipients": [{
                "name": "team_d",
                "host": "dd_out",
                "endpoint": "HTTP://127.0.0.1:17105/",
                "credential": "credentials/team_d"
            }],
            "outputs": [{"name": "team_d", "recipients": ["team_d"]}]
        })
    }

    /// The document validation makes of [`document`]: its endpoint in canonical form.
    fn validated() -> ResourcesArtifact {
        ResourcesArtifact {
            resources_version: RESOURCES_VERSION,
            recipients: vec![DeclaredRecipient {
                name: "team_d".to_string(),
                host: HostId::new("dd_out").unwrap(),
                endpoint: "http://127.0.0.1:17105".to_string(),
                credential: CredentialRef::new("credentials/team_d").unwrap(),
                protocol: DeclaredProtocol::default(),
                delivery: None,
            }],
            outputs: vec![DeclaredOutput {
                name: "team_d".to_string(),
                recipients: vec!["team_d".to_string()],
                accepts: DeclaredAccepts::Any,
                label: None,
            }],
        }
    }

    /// Validation that accepts [`document`] alone, as [`validated`].
    fn accepts_the_document(document: Value, _: &dyn Catalog) -> Result<ResourcesArtifact, ResourcesError> {
        if document == self::document() {
            Ok(validated())
        } else {
            Err(ResourcesError::Invalid {
                message: format!("not the test's document: {}", document),
            })
        }
    }

    /// Validation that refuses every document.
    fn refuses_every_document(_: Value, _: &dyn Catalog) -> Result<ResourcesArtifact, ResourcesError> {
        Err(ResourcesError::Invalid {
            message: "recipient `team_d` names host `mrf_dd_out`, which takes no declared recipients".to_string(),
        })
    }

    fn snapshot(configs: Vec<ArtifactConfig>) -> ArtifactSnapshot {
        ArtifactSnapshot::new(ArtifactProduct::Resources, configs)
    }

    fn refusal(event: &ResourcesEvent) -> &str {
        match event {
            ResourcesEvent::Refused { reason, .. } => reason,
            other => panic!("a refusal, not {:?}", other),
        }
    }

    #[test]
    fn an_empty_snapshot_revokes_and_several_configs_are_all_refused() {
        let catalog = MemoryCatalog::new();
        assert_eq!(
            read_snapshot(&snapshot(Vec::new()), &catalog, resources::validate),
            [ResourcesEvent::Revoked]
        );
        let payload = envelope(1, &document());
        let events = read_snapshot(
            &snapshot(vec![config("a", &payload), config("b", &payload)]),
            &catalog,
            accepts_the_document,
        );
        assert_eq!(events.len(), 2);
        for event in &events {
            assert_eq!(
                refusal(event),
                "the resources product holds 2 configs, and holds at most one: every one is refused until it holds one"
            );
        }
    }

    #[test]
    fn a_config_is_refused_for_its_transport_hash_envelope_or_artifact_hash() {
        let catalog = MemoryCatalog::new();
        let mut tampered = config("a", &envelope(1, &document()));
        tampered.payload.push(' ');
        let events = read_snapshot(&snapshot(vec![tampered]), &catalog, accepts_the_document);
        assert!(refusal(&events[0]).starts_with("transport hash mismatch"));

        let events = read_snapshot(
            &snapshot(vec![config("a", "{\"revision\": 1}")]),
            &catalog,
            accepts_the_document,
        );
        assert!(refusal(&events[0]).starts_with("The payload is not an artifact envelope"));

        let mismatched = json!({
            "revision": 1,
            "artifact_hash": PlanHash::of_value(&json!({})).unwrap(),
            "artifact": document()
        })
        .to_string();
        let events = read_snapshot(
            &snapshot(vec![config("a", &mismatched)]),
            &catalog,
            accepts_the_document,
        );
        assert!(refusal(&events[0]).starts_with("artifact hash mismatch: the envelope declares"));
    }

    #[test]
    fn the_one_config_is_delivered_with_its_revision_hash_and_the_document_validation_returns() {
        let catalog = MemoryCatalog::new();
        let payload = envelope(7, &document());
        let events = read_snapshot(
            &snapshot(vec![config("resources", &payload)]),
            &catalog,
            accepts_the_document,
        );
        assert_eq!(
            events,
            [ResourcesEvent::Delivered {
                config: ArtifactConfigId::new("resources"),
                version: 3,
                revision: 7,
                // The hash of the document as received, not of what validation made of it.
                artifact_hash: PlanHash::of_value(&document()).unwrap(),
                document: Ok(validated()),
            }]
        );

        // A document validation refuses is delivered with the reason, which the core rejects it for.
        let events = read_snapshot(
            &snapshot(vec![config("resources", &payload)]),
            &catalog,
            refuses_every_document,
        );
        let [ResourcesEvent::Delivered {
            revision: 7,
            document: Err(reason),
            ..
        }] = events.as_slice()
        else {
            panic!("one invalid delivery, not {:?}", events);
        };
        assert_eq!(
            reason,
            "The resources artifact is invalid: recipient `team_d` names host `mrf_dd_out`, which takes no declared \
             recipients"
        );
    }

    fn named(name: &str) -> ResourcesArtifact {
        let mut artifact = validated();
        artifact.recipients[0].name = name.to_string();
        artifact
    }

    fn revision(revision: u64) -> ArtifactRevision {
        ArtifactRevision {
            revision,
            hash: PlanHash::of_bytes(&revision.to_be_bytes()),
        }
    }

    #[test]
    fn a_declared_recipient_is_named_as_the_accepted_artifact_names_it_else_as_the_newest_held_one_does() {
        let id = RecipientId::new("dd_out/declared/team_d/http://127.0.0.1:17105").unwrap();
        // Revision 2 renames the recipient and is pending; revision 1 is still the one lowering takes.
        let mut held = HeldResources {
            held: vec![
                (named("team_d_renamed"), revision(2)),
                (named("team_d_intake"), revision(1)),
            ],
            accepted: Some((named("team_d_intake"), revision(1))),
        };
        assert_eq!(held.declared_name(&id).as_deref(), Some("team_d_intake"));
        held.accepted = None;
        assert_eq!(held.declared_name(&id).as_deref(), Some("team_d_renamed"));
        let other = RecipientId::new("dd_out/declared/team_e/http://127.0.0.1:17105").unwrap();
        assert_eq!(held.declared_name(&other), None);
        assert_eq!(HeldResources::default().declared_name(&id), None);
    }
}

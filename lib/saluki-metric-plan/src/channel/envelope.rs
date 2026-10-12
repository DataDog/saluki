//! The envelope inside a config's payload.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::{ResultExt as _, Snafu};

use crate::{canonical::PlanHash, ir::parse_document};

/// What an approval was made against.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovedAgainst {
    /// The active plan hash the host reported when the artifact was reviewed.
    ///
    /// When it differs from the active plan hash at the time the artifact is prepared, the artifact is still installed,
    /// and its review is reported as stale.
    pub active_plan_hash: PlanHash,
}

/// The envelope of one config: an approved artifact with its revision and the hash approval bound to.
///
/// Serialized as `{"revision": <n>, "artifact_hash": "sha256:<hex>", "approved_against": {"active_plan_hash":
/// "sha256:<hex>"}, "artifact": {...}}`, where `approved_against` is optional. Readers ignore unknown members. A
/// config of the resources product has the same envelope, without `approved_against`, around a resources artifact
/// ([`ResourcesArtifact`][crate::resources::ResourcesArtifact]), and its revision is on that product's own
/// high-water mark.
///
/// The artifact is kept as the JSON value it was received as: the receipt checks
/// ([`compose::receive_artifact`][crate::compose::receive_artifact]) read it, and `artifact_hash` must equal the
/// [`ReceivedArtifact`][crate::ir::ReceivedArtifact] hash of exactly this document.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Envelope {
    /// The artifact's revision. Revisions strictly increase: one at or below the highest revision seen is stale.
    pub revision: u64,

    /// The hash of the artifact document as received, which approval binds to.
    pub artifact_hash: PlanHash,

    /// What the approver reviewed the artifact against, if the publisher says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_against: Option<ApprovedAgainst>,

    /// The policy artifact document, as received.
    pub artifact: Value,
}

impl Envelope {
    /// Reads an envelope from a config's payload.
    ///
    /// The payload is read as an IR document is: an object that repeats a member name, at any depth, is rejected, so
    /// that the artifact hashed here is the artifact every other reader of the payload sees.
    ///
    /// # Errors
    ///
    /// Returns an error if the payload is not JSON, repeats a member name, or does not have the envelope's members.
    pub fn parse(payload: &str) -> Result<Self, EnvelopeError> {
        let value = parse_document(payload).context(Json)?;
        serde_json::from_value(value).context(Shape)
    }
}

/// A payload that is not a readable envelope.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum EnvelopeError {
    /// The payload is not JSON, or an object in it repeats a member name.
    #[snafu(display("The payload is not a JSON document with unique member names: {}", source))]
    Json {
        /// The underlying error.
        source: serde_json::Error,
    },

    /// The payload is JSON, but not an envelope.
    #[snafu(display(
        "The payload is not an artifact envelope ({{\"revision\", \"artifact_hash\", \"approved_against\", \
         \"artifact\"}}): {}",
        source
    ))]
    Shape {
        /// The underlying error.
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HASH: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn envelopes_read_with_and_without_an_approval_base() {
        let payload = format!(
            r#"{{"revision": 7, "artifact_hash": "{0}", "approved_against": {{"active_plan_hash": "{0}"}},
                "artifact": {{"ir_version": 1}}, "future": null}}"#,
            HASH
        );
        let envelope = Envelope::parse(&payload).unwrap();
        let hash: PlanHash = HASH.parse().unwrap();
        assert_eq!(envelope.revision, 7);
        assert_eq!(envelope.artifact_hash, hash);
        assert_eq!(
            envelope.approved_against,
            Some(ApprovedAgainst { active_plan_hash: hash })
        );
        assert_eq!(envelope.artifact, json!({"ir_version": 1}));

        let payload = format!(r#"{{"revision": 1, "artifact_hash": "{}", "artifact": {{}}}}"#, HASH);
        assert_eq!(Envelope::parse(&payload).unwrap().approved_against, None);
    }

    #[test]
    fn envelopes_that_are_not_documents_or_lack_members_are_refused() {
        let repeated = format!(
            r#"{{"revision": 1, "artifact_hash": "{}", "artifact": {{"scope": "a", "scope": "b"}}}}"#,
            HASH
        );
        let cases = [
            ("not json", "is not a JSON document"),
            (repeated.as_str(), "is not a JSON document"),
            (r#"{"revision": 1, "artifact": {}}"#, "is not an artifact envelope"),
            (
                r#"{"revision": -1, "artifact_hash": "sha256:00", "artifact": {}}"#,
                "is not an artifact envelope",
            ),
        ];
        for (payload, expected) in cases {
            let error = Envelope::parse(payload).unwrap_err().to_string();
            assert!(error.contains(expected), "{}: {}", payload, error);
        }
    }
}

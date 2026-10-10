//! The artifact channel stub protocol, version 1: the messages ADP and the stub exchange.
//!
//! ADP polls `POST {stub_url}/v1/artifacts/poll` with its acknowledgments and a status summary, and receives a complete
//! snapshot of the `policy` product. Each config's `payload` is a JSON **string** holding the envelope, so that the
//! transport hash (`sha256`, lowercase hexadecimal of the payload's UTF-8 bytes) covers exact bytes.
//!
//! Readers ignore unknown fields, so the protocol can grow additively. The stub goes further with what ADP reports:
//! [`PollRequest::read`] and [`AdpStatus::read`] read each known field on its own, so that a field of an unexpected
//! type is noted and left out instead of making the whole message unreadable. Only `protocol_version` decides whether a
//! poll is answered. The protocol treats any answer other than `200 OK` as a transport failure, which keeps ADP on its
//! current feed, so a reporting mistake on ADP's side must never block delivery.

use std::collections::BTreeMap;

use saluki_metric_plan::canonical::PlanHash;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};

/// The protocol version this stub speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The product policy artifacts are delivered in.
pub const POLICY_PRODUCT: &str = "policy";

/// The config ID the demo publishes its artifact for the policy scope under.
pub const DEMO_CONFIG_ID: &str = "demo";

/// The scope whose artifact the top-level status members describe, and which the demo's config `demo` replaces.
pub const POLICY_SCOPE: &str = saluki_metric_plan::channel::POLICY_SCOPE;

/// Returns the config ID the demo publishes an artifact for `scope` under: one config per scope, `demo` for the policy
/// scope, as the first version of the demo published it, and `demo-<scope>` for any other.
pub fn config_id(scope: &str) -> String {
    if scope == POLICY_SCOPE {
        DEMO_CONFIG_ID.to_string()
    } else {
        format!("{}-{}", DEMO_CONFIG_ID, scope)
    }
}

/// The path ADP polls.
pub const POLL_PATH: &str = "/v1/artifacts/poll";

/// How much of an unreadable value a note quotes, in characters.
const NOTE_VALUE_CHARS: usize = 80;

/// A poll from ADP.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PollRequest {
    /// The protocol version ADP speaks, if the poll states one that can be read.
    pub protocol_version: Option<u32>,

    /// Who is polling.
    pub client: ClientInfo,

    /// The products ADP asks for.
    pub products: Vec<String>,

    /// The latest state of each config ADP has seen, at most one per config ID.
    pub config_states: Vec<ConfigState>,

    /// ADP's status summary.
    pub status: Option<ChannelStatus>,
}

impl PollRequest {
    /// Reads a poll from its JSON document, field by field.
    ///
    /// Returns the poll and a note for each known field that could not be read. Such a field is left at its default,
    /// and a config state that is not an object is left out. Missing fields are not noted.
    pub fn read(value: &Value) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let Some(poll) = object(value, "poll", &mut notes) else {
            return (Self::default(), notes);
        };
        let client = poll
            .get("client")
            .filter(|client| !client.is_null())
            .and_then(|client| object(client, "client", &mut notes))
            .map(|client| ClientInfo {
                id: member(client, "client.", "id", &mut notes).unwrap_or_default(),
                version: member(client, "client.", "version", &mut notes).unwrap_or_default(),
            })
            .unwrap_or_default();
        let request = Self {
            protocol_version: member(poll, "", "protocol_version", &mut notes),
            client,
            products: member::<Option<Vec<String>>>(poll, "", "products", &mut notes)
                .flatten()
                .unwrap_or_default(),
            config_states: config_states(poll, &mut notes),
            status: poll
                .get("status")
                .and_then(|status| ChannelStatus::read(status, "status", &mut notes)),
        };
        (request, notes)
    }
}

/// The polling process.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ClientInfo {
    /// A random ID per ADP process.
    pub id: String,

    /// ADP's version.
    pub version: String,
}

/// ADP's acknowledgment of one config.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ConfigState {
    /// The product.
    pub product: String,

    /// The config ID.
    pub id: String,

    /// The config version.
    pub version: Option<u64>,

    /// The artifact revision.
    pub revision: Option<u64>,

    /// The artifact hash, `sha256:<hex>`.
    pub artifact_hash: Option<String>,

    /// `pending`, `prepared`, `active`, `retired`, `rejected`, or `failed`.
    pub state: String,

    /// The generation the artifact runs in, once it has one.
    pub generation: Option<u64>,

    /// The cut time, when a cut is prepared.
    pub cut_at: Option<Value>,

    /// Why the artifact was rejected or failed, in plain text.
    pub error: Option<String>,
}

impl ConfigState {
    /// Reads one config state, noting the fields that cannot be read. Returns `None`, noted, if it is not an object.
    fn read(value: &Value, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        let state = object(value, path, notes)?;
        let path = format!("{}.", path);
        Some(Self {
            product: member(state, &path, "product", notes).unwrap_or_default(),
            id: member(state, &path, "id", notes).unwrap_or_default(),
            version: member(state, &path, "version", notes).flatten(),
            revision: member(state, &path, "revision", notes).flatten(),
            artifact_hash: member(state, &path, "artifact_hash", notes).flatten(),
            state: member(state, &path, "state", notes).unwrap_or_default(),
            generation: member(state, &path, "generation", notes).flatten(),
            cut_at: state.get("cut_at").filter(|cut_at| !cut_at.is_null()).cloned(),
            error: member(state, &path, "error", notes).flatten(),
        })
    }
}

/// ADP's status summary, sent with every poll and served by `GET /programmable-metrics/status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ChannelStatus {
    /// The hash of the default program's export.
    pub default_program_hash: Option<String>,

    /// The highest artifact revision ADP has seen.
    pub revision_high_water: Option<u64>,

    /// The active generation.
    pub active_generation: Option<u64>,

    /// The hash of the active composed plan.
    pub active_plan_hash: Option<String>,

    /// The active artifact, or `None` when the default program runs.
    pub active_artifact: Option<ActiveArtifact>,

    /// Whether the active artifact's base no longer matches the default program.
    pub stale_base: Option<bool>,

    /// Whether the active artifact was approved against another active plan than the one it was prepared against.
    pub stale_review: Option<bool>,

    /// Why ADP is degraded, if it is.
    pub degraded: Option<String>,

    /// How many generations are alive.
    pub live_generations: Option<u64>,

    /// The status of each scope, by scope, when ADP reports them: the top-level members describe the policy scope.
    pub scopes: BTreeMap<String, ScopeState>,
}

/// The status of one scope of ADP's default program.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScopeState {
    /// The base program hash of the scope's export.
    pub default_program_hash: Option<String>,

    /// The artifact active for the scope, or `None` when the default program's region runs there.
    pub active_artifact: Option<ActiveArtifact>,

    /// Whether the scope's active artifact's base no longer matches the default program.
    pub stale_base: Option<bool>,
}

impl ChannelStatus {
    /// Returns the artifact ADP reports active in each scope, by scope: those `scopes` names, and the top-level
    /// `active_artifact` for the policy scope when `scopes` does not describe it.
    pub fn active_artifacts(&self) -> BTreeMap<String, ActiveArtifact> {
        let mut active = self
            .scopes
            .iter()
            .filter_map(|(scope, state)| Some((scope.clone(), state.active_artifact.clone()?)))
            .collect::<BTreeMap<_, _>>();
        if !self.scopes.contains_key(POLICY_SCOPE) {
            if let Some(artifact) = &self.active_artifact {
                active.insert(POLICY_SCOPE.to_string(), artifact.clone());
            }
        }
        active
    }

    /// Reads a status summary, noting the fields that cannot be read. `null` is no status.
    fn read(value: &Value, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        let status = object(value, path, notes)?;
        Some(Self::read_members(status, &format!("{}.", path), notes))
    }

    fn read_members(status: &Map<String, Value>, path: &str, notes: &mut Vec<String>) -> Self {
        Self {
            default_program_hash: member(status, path, "default_program_hash", notes).flatten(),
            revision_high_water: member(status, path, "revision_high_water", notes).flatten(),
            active_generation: member(status, path, "active_generation", notes).flatten(),
            active_plan_hash: member(status, path, "active_plan_hash", notes).flatten(),
            active_artifact: ActiveArtifact::read(status, path, notes),
            stale_base: member(status, path, "stale_base", notes).flatten(),
            stale_review: member(status, path, "stale_review", notes).flatten(),
            degraded: member(status, path, "degraded", notes).flatten(),
            live_generations: member(status, path, "live_generations", notes).flatten(),
            scopes: ScopeState::read_all(status, path, notes),
        }
    }
}

impl ScopeState {
    /// Reads the `scopes` member of a status, noting the scopes and fields that cannot be read. A missing or `null`
    /// member is no scope.
    fn read_all(status: &Map<String, Value>, path: &str, notes: &mut Vec<String>) -> BTreeMap<String, Self> {
        let scopes_path = format!("{}scopes", path);
        let Some(scopes) = status
            .get("scopes")
            .filter(|scopes| !scopes.is_null())
            .and_then(|scopes| object(scopes, &scopes_path, notes))
        else {
            return BTreeMap::new();
        };
        scopes
            .iter()
            .filter_map(|(name, scope)| {
                let path = format!("{}.{}", scopes_path, name);
                let scope = object(scope, &path, notes)?;
                let path = format!("{}.", path);
                let state = Self {
                    default_program_hash: member(scope, &path, "default_program_hash", notes).flatten(),
                    active_artifact: ActiveArtifact::read(scope, &path, notes),
                    stale_base: member(scope, &path, "stale_base", notes).flatten(),
                };
                Some((name.clone(), state))
            })
            .collect()
    }
}

/// The artifact a status reports as active.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ActiveArtifact {
    /// The config ID.
    pub id: String,

    /// The artifact revision.
    pub revision: Option<u64>,

    /// The artifact hash, `sha256:<hex>`.
    pub artifact_hash: Option<String>,
}

impl ActiveArtifact {
    /// Reads the `active_artifact` member of a status or of a scope's status, noting the fields that cannot be read.
    /// `null` is no artifact.
    fn read(status: &Map<String, Value>, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        let artifact_path = format!("{}active_artifact", path);
        status
            .get("active_artifact")
            .filter(|artifact| !artifact.is_null())
            .and_then(|artifact| object(artifact, &artifact_path, notes))
            .map(|artifact| {
                let path = format!("{}.", artifact_path);
                Self {
                    id: member(artifact, &path, "id", notes).unwrap_or_default(),
                    revision: member(artifact, &path, "revision", notes).flatten(),
                    artifact_hash: member(artifact, &path, "artifact_hash", notes).flatten(),
                }
            })
    }
}

/// ADP's full status, as `GET /programmable-metrics/status` serves it: the poll's status plus its config states.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AdpStatus {
    /// The status summary.
    #[serde(flatten)]
    pub status: ChannelStatus,

    /// The latest state of each config ADP has seen.
    pub config_states: Vec<ConfigState>,
}

impl AdpStatus {
    /// Reads ADP's full status from its JSON document, field by field, as [`PollRequest::read`] reads a poll.
    pub fn read(value: &Value) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let Some(document) = object(value, "status", &mut notes) else {
            return (Self::default(), notes);
        };
        let status = Self {
            status: ChannelStatus::read_members(document, "", &mut notes),
            config_states: config_states(document, &mut notes),
        };
        (status, notes)
    }
}

/// Reads the `config_states` member of a poll or a status.
fn config_states(document: &Map<String, Value>, notes: &mut Vec<String>) -> Vec<ConfigState> {
    match document.get("config_states") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(states)) => states
            .iter()
            .enumerate()
            .filter_map(|(index, state)| ConfigState::read(state, &format!("config_states[{}]", index), notes))
            .collect(),
        Some(other) => {
            notes.push(note("", "config_states", "expected a list", other));
            Vec::new()
        }
    }
}

/// Returns `value` as an object, or notes that it is not one.
fn object<'a>(value: &'a Value, path: &str, notes: &mut Vec<String>) -> Option<&'a Map<String, Value>> {
    match value {
        Value::Object(members) => Some(members),
        other => {
            notes.push(note("", path, "expected an object", other));
            None
        }
    }
}

/// Reads one member of an object. A member of another type is noted and read as `None`; a missing one is `None`.
fn member<T>(object: &Map<String, Value>, path: &str, name: &str, notes: &mut Vec<String>) -> Option<T>
where
    T: DeserializeOwned,
{
    let value = object.get(name)?;
    match T::deserialize(value) {
        Ok(read) => Some(read),
        Err(error) => {
            notes.push(note(path, name, &error.to_string(), value));
            None
        }
    }
}

/// Describes a field that could not be read, quoting the start of its value.
fn note(path: &str, name: &str, problem: &str, value: &Value) -> String {
    let mut quoted = value.to_string();
    if quoted.chars().count() > NOTE_VALUE_CHARS {
        quoted = quoted
            .chars()
            .take(NOTE_VALUE_CHARS)
            .chain(std::iter::once('…'))
            .collect();
    }
    format!("{}{}: {} (got {})", path, name, problem, quoted)
}

/// The stub's answer to a poll: a complete snapshot per product.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PollResponse {
    /// The protocol version.
    pub protocol_version: u32,

    /// The configs of each product. An empty list, or a missing product, revokes.
    pub products: BTreeMap<String, Vec<ConfigEntry>>,
}

/// One config of a snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigEntry {
    /// The config ID.
    pub id: String,

    /// The config version.
    pub version: u64,

    /// The lowercase hexadecimal SHA-256 of the payload's UTF-8 bytes.
    pub sha256: String,

    /// The envelope, as JSON text.
    pub payload: String,
}

/// The envelope inside a config's payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Envelope {
    /// The artifact revision, which must exceed ADP's revision high-water mark.
    pub revision: u64,

    /// The plan crate's hash of the artifact document as received.
    pub artifact_hash: String,

    /// What the approver reviewed the artifact against, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_against: Option<ApprovedAgainst>,

    /// The policy artifact document.
    pub artifact: Value,
}

/// What an approval was made against.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovedAgainst {
    /// ADP's active plan hash at the time of review.
    pub active_plan_hash: String,
}

/// Returns the transport hash of a payload: the lowercase hexadecimal SHA-256 of its UTF-8 bytes.
pub fn transport_hash(payload: &str) -> String {
    PlanHash::of_bytes(payload.as_bytes())
        .digest()
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn transport_hashes_are_the_hexadecimal_sha256_of_the_payload_bytes() {
        // The SHA-256 of "abc" (FIPS 180-2, appendix B.1) and of the empty string.
        assert_eq!(
            transport_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            transport_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn polls_with_unknown_and_missing_fields_are_read() {
        let (poll, notes) = PollRequest::read(&json!({
            "protocol_version": 1, "client": {"id": "a1", "version": "1.8.0", "build": "x"},
            "products": ["policy"], "future_field": [1, 2],
            "config_states": [{"product": "policy", "id": "demo", "version": 7, "revision": 7,
                               "artifact_hash": "sha256:00", "state": "active", "generation": 12,
                               "cut_at": null, "error": null, "phase": "steady"}],
            "status": {"revision_high_water": 7, "active_artifact": null, "live_generations": 2,
                       "new_status_field": true}
        }));
        assert!(notes.is_empty(), "{:?}", notes);
        assert_eq!(poll.protocol_version, Some(1));
        assert_eq!(poll.client.id, "a1");
        assert_eq!(poll.products, ["policy"]);
        assert_eq!(poll.config_states[0].state, "active");
        assert_eq!(poll.config_states[0].generation, Some(12));
        let status = poll.status.unwrap();
        assert_eq!(status.revision_high_water, Some(7));
        assert_eq!(status.active_artifact, None);
        assert_eq!(status.live_generations, Some(2));
        assert_eq!(status.default_program_hash, None);

        let (empty, notes) = PollRequest::read(&json!({}));
        assert_eq!(empty, PollRequest::default());
        assert!(notes.is_empty());
    }

    #[test]
    fn mistyped_fields_are_noted_and_left_out_while_the_rest_of_the_poll_reads() {
        let (poll, notes) = PollRequest::read(&json!({
            "protocol_version": 1,
            "client": {"id": null, "version": "1.8.0"},
            "products": "policy",
            "config_states": [
                {"product": "policy", "id": "demo", "revision": 3, "state": null, "generation": "12"},
                "not a state"
            ],
            "status": {"revision_high_water": 3, "degraded": {"reason": "slow"}, "active_generation": 4,
                       "active_artifact": {"id": "demo", "revision": "3"}}
        }));
        // What could be read is kept.
        assert_eq!(poll.protocol_version, Some(1));
        assert_eq!(poll.client.version, "1.8.0");
        assert_eq!(poll.config_states.len(), 1);
        assert_eq!(poll.config_states[0].revision, Some(3));
        assert_eq!(poll.config_states[0].generation, None);
        let status = poll.status.as_ref().unwrap();
        assert_eq!(
            (status.revision_high_water, status.active_generation),
            (Some(3), Some(4))
        );
        assert_eq!(status.degraded, None);
        assert_eq!(status.active_artifact.as_ref().unwrap().revision, None);
        // Each unreadable field is noted with where it is and what it held.
        let expected = [
            "client.id: invalid type: null, expected a string (got null)",
            "products: invalid type: string \"policy\", expected a sequence (got \"policy\")",
            "config_states[0].state: invalid type: null, expected a string (got null)",
            "config_states[0].generation: invalid type: string \"12\", expected u64 (got \"12\")",
            "config_states[1]: expected an object (got \"not a state\")",
            "status.active_artifact.revision: invalid type: string \"3\", expected u64 (got \"3\")",
            "status.degraded: invalid type: map, expected a string (got {\"reason\":\"slow\"})",
        ];
        assert_eq!(notes, expected);

        // A protocol version of the wrong type is unreadable, not version 0.
        let (poll, notes) = PollRequest::read(&json!({"protocol_version": "1"}));
        assert_eq!(poll.protocol_version, None);
        assert_eq!(notes.len(), 1);
        let (_, notes) = PollRequest::read(&json!([1]));
        assert_eq!(notes, ["poll: expected an object (got [1])"]);
    }

    #[test]
    fn each_scopes_status_is_read_field_by_field() {
        let (status, notes) = AdpStatus::read(&json!({
            "active_artifact": {"id": "demo", "revision": 3, "artifact_hash": "sha256:03"},
            "scopes": {
                "dogstatsd_pre_aggregation": {"default_program_hash": "sha256:11", "stale_base": true,
                    "active_artifact": {"id": "demo-dogstatsd_pre_aggregation", "revision": 4,
                                        "artifact_hash": "sha256:04"}},
                "post_enrich": {"default_program_hash": "sha256:22", "active_artifact": null, "stale_base": "no",
                                "future_field": 1},
                "broken": 7
            }
        }));
        let scopes = &status.status.scopes;
        assert_eq!(
            scopes.keys().collect::<Vec<_>>(),
            ["dogstatsd_pre_aggregation", "post_enrich"]
        );
        let pre = &scopes["dogstatsd_pre_aggregation"];
        assert_eq!(pre.default_program_hash.as_deref(), Some("sha256:11"));
        assert_eq!(pre.stale_base, Some(true));
        assert_eq!(pre.active_artifact.as_ref().unwrap().revision, Some(4));
        let post = &scopes["post_enrich"];
        assert_eq!((post.active_artifact.as_ref(), post.stale_base), (None, None));
        assert_eq!(
            notes,
            [
                "scopes.broken: expected an object (got 7)",
                "scopes.post_enrich.stale_base: invalid type: string \"no\", expected a boolean (got \"no\")",
            ]
        );
        // A status without scopes, as the first version of the protocol writes it, has none.
        let (status, notes) = AdpStatus::read(&json!({"revision_high_water": 1}));
        assert!(status.status.scopes.is_empty() && notes.is_empty());
        assert_eq!(config_id(POLICY_SCOPE), DEMO_CONFIG_ID);
        assert_eq!(config_id("dogstatsd_pre_aggregation"), "demo-dogstatsd_pre_aggregation");
    }

    #[test]
    fn the_privileged_status_reads_as_the_poll_status_plus_config_states() {
        let (status, notes) = AdpStatus::read(&json!({
            "default_program_hash": "sha256:11", "revision_high_water": 3, "active_generation": 4,
            "config_states": [{"product": "policy", "id": "demo", "state": "rejected", "error": "stale revision"}]
        }));
        assert!(notes.is_empty(), "{:?}", notes);
        assert_eq!(status.status.default_program_hash.as_deref(), Some("sha256:11"));
        assert_eq!(status.status.revision_high_water, Some(3));
        assert_eq!(status.config_states[0].error.as_deref(), Some("stale revision"));

        let (status, notes) = AdpStatus::read(&json!({"stale_base": "yes", "config_states": {}}));
        assert_eq!(status, AdpStatus::default());
        assert_eq!(
            notes,
            [
                "stale_base: invalid type: string \"yes\", expected a boolean (got \"yes\")",
                "config_states: expected a list (got {})"
            ]
        );
    }
}

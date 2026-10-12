//! The artifact channel stub protocol, version 1: the messages ADP and the stub exchange.
//!
//! ADP polls `POST {stub_url}/v1/artifacts/poll` with its acknowledgments and a status summary, and receives a complete
//! snapshot of each product it asks for: `policy`, and, from stage 7, `resources`. Each config's `payload` is a JSON
//! **string** holding the envelope, so that the transport hash (`sha256`, lowercase hexadecimal of the payload's UTF-8
//! bytes) covers exact bytes.
//!
//! The `resources` product holds at most one config, whose envelope carries a resources artifact (the recipients and
//! outputs it declares) under revisions of its own: ADP keeps a separate revision high-water mark for it, which its
//! status reports as `resources.revision_high_water`. The privileged status route also lists the recipients of ADP's
//! delivery hosts (`recipients`, see [`RecipientEntry`]), which the demo's Recipients panel shows.
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

/// The product resources artifacts are delivered in.
pub const RESOURCES_PRODUCT: &str = "resources";

/// The config ID the demo publishes its artifact for the policy scope under.
pub const DEMO_CONFIG_ID: &str = "demo";

/// The config ID the demo publishes its resources artifact under: the product's only config.
pub const RESOURCES_CONFIG_ID: &str = "demo-resources";

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

    /// Why the artifact was rejected or failed, or why it waits, in plain text; for an `active` artifact, what its
    /// composition says beside its state: why it runs degraded, or a note such as advice to rebase.
    pub error: Option<String>,

    /// The outputs whose emits an `active` artifact runs without, which its `error` names (see [`dropped_outputs`]):
    /// the artifact is degraded if there are any. The demo derives this; it is not a member ADP writes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped_outputs: Vec<String>,
}

impl ConfigState {
    /// Reads one config state, noting the fields that cannot be read. Returns `None`, noted, if it is not an object.
    fn read(value: &Value, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        let state = object(value, path, notes)?;
        let path = format!("{}.", path);
        let mut read = Self {
            product: member(state, &path, "product", notes).unwrap_or_default(),
            id: member(state, &path, "id", notes).unwrap_or_default(),
            version: member(state, &path, "version", notes).flatten(),
            revision: member(state, &path, "revision", notes).flatten(),
            artifact_hash: member(state, &path, "artifact_hash", notes).flatten(),
            state: member(state, &path, "state", notes).unwrap_or_default(),
            generation: member(state, &path, "generation", notes).flatten(),
            cut_at: state.get("cut_at").filter(|cut_at| !cut_at.is_null()).cloned(),
            error: member(state, &path, "error", notes).flatten(),
            dropped_outputs: Vec::new(),
        };
        if read.state == "active" {
            read.dropped_outputs = read
                .error
                .as_deref()
                .map(dropped_outputs)
                .unwrap_or_default()
                .into_iter()
                .map(str::to_string)
                .collect();
        }
        Some(read)
    }
}

/// How each reason an `active` acknowledgment gives for a degraded artifact starts: the output it is about.
const DROPPED_OUTPUT_NOTE: &str = "output `";

/// What each reason an `active` acknowledgment gives for a degraded artifact says: composition replaced the emits to
/// an output that is no longer declared, or now delivers elsewhere, with counted drops.
const EMITS_DROPPED: &str = "its emits are dropped";

/// Returns the outputs an `active` acknowledgment's error says the artifact's emits to are dropped, each once, in the
/// order it names them: the artifact runs degraded if there are any.
///
/// ADP joins what composition says about an artifact in effect with `; `, and each reason for a degraded artifact
/// starts with the output and says its emits are dropped (``output `resources_team_d` is no longer declared; its emits
/// are dropped until it returns``). Other notes, such as the advice to rebase an artifact whose version 1 fingerprints
/// still match, name no dropped output: such an artifact runs as written.
pub fn dropped_outputs(error: &str) -> Vec<&str> {
    // Where each note about an output starts: the start of the text, or after the separator.
    let starts = error
        .match_indices(DROPPED_OUTPUT_NOTE)
        .map(|(index, _)| index)
        .filter(|&index| index == 0 || error[..index].ends_with("; "))
        .collect::<Vec<_>>();
    let mut outputs = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(error.len());
        let note = &error[start + DROPPED_OUTPUT_NOTE.len()..end];
        let Some((output, said)) = note.split_once('`') else {
            continue;
        };
        if said.contains(EMITS_DROPPED) && !outputs.contains(&output) {
            outputs.push(output);
        }
    }
    outputs
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

    /// The status of the resources product, once ADP has seen a resources config.
    pub resources: Option<ResourcesStatus>,
}

/// The status of the resources product, as ADP reports it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ResourcesStatus {
    /// The highest resources revision ADP has seen: the product's own mark, apart from the policy product's.
    pub revision_high_water: Option<u64>,

    /// The resources artifact ADP's active generation was lowered with, or `None` when it runs with none.
    pub active_artifact: Option<ActiveArtifact>,
}

impl ResourcesStatus {
    /// Reads the `resources` member of a status, noting the fields that cannot be read. A missing or `null` member is
    /// no resources status.
    fn read(status: &Map<String, Value>, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        let resources_path = format!("{}resources", path);
        let resources = status
            .get("resources")
            .filter(|resources| !resources.is_null())
            .and_then(|resources| object(resources, &resources_path, notes))?;
        let path = format!("{}.", resources_path);
        Some(Self {
            revision_high_water: member(resources, &path, "revision_high_water", notes).flatten(),
            active_artifact: ActiveArtifact::read(resources, &path, notes),
        })
    }
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
            resources: ResourcesStatus::read(status, path, notes),
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

/// ADP's full status, as `GET /programmable-metrics/status` serves it: the poll's status plus its config states and,
/// from stage 7, its delivery hosts' recipients.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AdpStatus {
    /// The status summary.
    #[serde(flatten)]
    pub status: ChannelStatus,

    /// The latest state of each config ADP has seen.
    pub config_states: Vec<ConfigState>,

    /// The recipients of ADP's delivery hosts, or `None` if the status has no `recipients` member.
    pub recipients: Option<Vec<RecipientEntry>>,
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
            recipients: RecipientEntry::read_all(document, &mut notes),
        };
        (status, notes)
    }
}

/// One recipient of one of ADP's delivery hosts, as the privileged status route lists it under `recipients`.
///
/// ADP lists every recipient its resource table holds, native and declared, and each one that retired recently, with
/// state `retired` and the report of its drain, so that a drain's outcome can be read after the recipient is gone.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecipientEntry {
    /// The delivery host, such as `dd_out`.
    pub host: String,

    /// The recipient's identity, such as `dd_out/primary/0` or `dd_out/declared/team_d/http://127.0.0.1:17105`.
    pub id: String,

    /// The recipient's slot in its host's table.
    pub slot: Option<u64>,

    /// The recipient's incarnation: a recipient prepared again, after it retired or failed, is a new incarnation of
    /// the same identity.
    pub incarnation: Option<u64>,

    /// `preparing`, `live`, `draining`, `failed`, `exiting`, or `retired`.
    pub state: String,

    /// Who declared it: `native`, or the resources revision that declared it, as ADP writes it.
    pub provenance: Option<Value>,

    /// The bytes its retry queue holds.
    pub queue_bytes: Option<u64>,

    /// The report of its last drain, once it retired.
    pub last_drain: Option<DrainReport>,
}

/// What a recipient's drain did with the work it held when it was released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct DrainReport {
    /// Transactions delivered during the drain.
    pub delivered: Option<u64>,

    /// Transactions written to disk when the drain ended.
    pub persisted: Option<u64>,

    /// Transactions dropped when the drain ended, which the forwarder's telemetry counts too.
    pub dropped: Option<u64>,

    /// Whether the drain ended at its deadline, rather than with its queue empty.
    pub deadline_reached: Option<bool>,

    /// When the recipient retired, in milliseconds since the Unix epoch.
    pub retired_ms: Option<u64>,
}

impl RecipientEntry {
    /// Reads the `recipients` member of a status, noting the entries and fields that cannot be read. A missing or
    /// `null` member is `None`: an ADP that does not report recipients.
    fn read_all(document: &Map<String, Value>, notes: &mut Vec<String>) -> Option<Vec<Self>> {
        match document.get("recipients") {
            None | Some(Value::Null) => None,
            Some(Value::Array(entries)) => Some(
                entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| Self::read(entry, &format!("recipients[{}]", index), notes))
                    .collect(),
            ),
            Some(other) => {
                notes.push(note("", "recipients", "expected a list", other));
                None
            }
        }
    }

    /// Reads one recipient, noting the fields that cannot be read. Returns `None`, noted, if it is not an object.
    fn read(value: &Value, path: &str, notes: &mut Vec<String>) -> Option<Self> {
        let entry = object(value, path, notes)?;
        let drain_path = format!("{}.last_drain", path);
        let last_drain = entry
            .get("last_drain")
            .filter(|drain| !drain.is_null())
            .and_then(|drain| object(drain, &drain_path, notes))
            .map(|drain| {
                let path = format!("{}.", drain_path);
                DrainReport {
                    delivered: member(drain, &path, "delivered", notes).flatten(),
                    persisted: member(drain, &path, "persisted", notes).flatten(),
                    dropped: member(drain, &path, "dropped", notes).flatten(),
                    deadline_reached: member(drain, &path, "deadline_reached", notes).flatten(),
                    retired_ms: member(drain, &path, "retired_ms", notes).flatten(),
                }
            });
        let path = format!("{}.", path);
        Some(Self {
            host: member(entry, &path, "host", notes).unwrap_or_default(),
            id: member(entry, &path, "id", notes).unwrap_or_default(),
            slot: member(entry, &path, "slot", notes).flatten(),
            incarnation: member(entry, &path, "incarnation", notes).flatten(),
            state: member(entry, &path, "state", notes).unwrap_or_default(),
            provenance: entry
                .get("provenance")
                .filter(|provenance| !provenance.is_null())
                .cloned(),
            queue_bytes: member(entry, &path, "queue_bytes", notes).flatten(),
            last_drain,
        })
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
///
/// A resources config's envelope has the same members, without `approved_against`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Envelope {
    /// The artifact revision, which must exceed ADP's revision high-water mark of the config's product.
    pub revision: u64,

    /// The plan crate's hash of the artifact document as received: the digest of its canonical JSON form.
    pub artifact_hash: String,

    /// What the approver reviewed the artifact against, if known (policy configs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_against: Option<ApprovedAgainst>,

    /// The artifact document: a policy artifact, or a resources artifact.
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
    fn an_active_acknowledgment_is_degraded_only_by_reasons_that_drop_emits() {
        // The reasons and the note as the plan crate writes them, joined as ADP joins them.
        let removed = "output `resources_team_d` is no longer declared; its emits are dropped until it returns";
        let rebound = "output `b` now delivers elsewhere; its emits are dropped; rebase";
        let renumbered = "output `metrics_routing_encode_0` renumbered: the recorded output is now \
                          `metrics_routing_encode_1`; its emits are dropped; rebase";
        let rebase = "the base records `dd_metrics_encode`, `metrics_routing_encode_0` with version 1 fingerprints, \
                      which still match; rebase to fingerprint version 2";
        assert_eq!(dropped_outputs(removed), ["resources_team_d"]);
        assert_eq!(dropped_outputs(rebase), Vec::<&str>::new());
        assert_eq!(
            dropped_outputs(&[rebound, renumbered, rebase, removed].join("; ")),
            ["b", "metrics_routing_encode_0", "resources_team_d"]
        );
        assert_eq!(dropped_outputs(&[rebase, rebound, rebound].join("; ")), ["b"]);
        // An output named inside a note does not start one, and a refusal drops nothing.
        assert_eq!(
            dropped_outputs("waiting for output `resources_team_d`"),
            Vec::<&str>::new()
        );
        assert_eq!(
            dropped_outputs("output `b` now delivers elsewhere; rebase"),
            Vec::<&str>::new()
        );

        let (status, notes) = AdpStatus::read(&json!({"config_states": [
            {"id": "demo", "revision": 4, "state": "active", "error": format!("{}; {}", removed, rebase)},
            {"id": "demo-2", "revision": 5, "state": "active", "error": rebase},
            {"id": "demo-3", "revision": 6, "state": "rejected", "error": rebound},
        ]}));
        assert!(notes.is_empty(), "{:?}", notes);
        let dropped = status
            .config_states
            .iter()
            .map(|state| state.dropped_outputs.clone())
            .collect::<Vec<_>>();
        assert_eq!(dropped, [vec!["resources_team_d".to_string()], Vec::new(), Vec::new()]);
        // The UI reads it as a member of the acknowledgment, absent when nothing is dropped.
        let written = serde_json::to_value(&status.config_states).unwrap();
        assert_eq!(written[0]["dropped_outputs"], json!(["resources_team_d"]));
        assert!(written[1].get("dropped_outputs").is_none());
    }

    #[test]
    fn the_resources_status_and_the_recipients_are_read_field_by_field() {
        let (status, notes) = AdpStatus::read(&json!({
            "revision_high_water": 7,
            "resources": {"revision_high_water": 3, "future_field": [],
                          "active_artifact": {"id": "demo-resources", "revision": 3, "artifact_hash": "sha256:03"}},
            "config_states": [{"product": "resources", "id": "demo-resources", "revision": 3, "state": "active"}],
            "recipients": [
                {"host": "dd_out", "id": "dd_out/primary/0", "slot": 0, "incarnation": 1, "state": "live",
                 "provenance": "native", "queue_bytes": 2048, "future_field": true},
                {"host": "dd_out", "id": "dd_out/declared/team_d/http://127.0.0.1:17105", "slot": 1, "incarnation": "2",
                 "state": "retired", "provenance": {"declared": {"revision": 3, "hash": "sha256:03"}},
                 "last_drain": {"delivered": 4, "persisted": 0, "dropped": 2, "deadline_reached": true,
                                "retired_ms": 1_700_000_000_000_u64}},
                "not a recipient"
            ]
        }));
        // The two products' marks are apart.
        assert_eq!(status.status.revision_high_water, Some(7));
        let resources = status.status.resources.as_ref().unwrap();
        assert_eq!(resources.revision_high_water, Some(3));
        assert_eq!(resources.active_artifact.as_ref().unwrap().id, "demo-resources");
        assert_eq!(status.config_states[0].product, "resources");

        let recipients = status.recipients.as_ref().unwrap();
        assert_eq!(recipients.len(), 2);
        assert_eq!(
            (
                recipients[0].state.as_str(),
                recipients[0].queue_bytes,
                recipients[0].last_drain
            ),
            ("live", Some(2048), None)
        );
        assert_eq!(recipients[0].provenance, Some(json!("native")));
        let team_d = &recipients[1];
        assert_eq!(team_d.incarnation, None);
        assert_eq!(
            team_d.last_drain,
            Some(DrainReport {
                delivered: Some(4),
                persisted: Some(0),
                dropped: Some(2),
                deadline_reached: Some(true),
                retired_ms: Some(1_700_000_000_000),
            })
        );
        assert_eq!(
            notes,
            [
                "recipients[1].incarnation: invalid type: string \"2\", expected u64 (got \"2\")",
                "recipients[2]: expected an object (got \"not a recipient\")",
            ]
        );

        // A status of an ADP before stage 7 has neither, which is not the same as listing no recipients.
        let (status, notes) = AdpStatus::read(&json!({"revision_high_water": 1, "resources": null}));
        assert!(notes.is_empty());
        assert_eq!((status.status.resources, status.recipients), (None, None));
        let (status, _) = AdpStatus::read(&json!({"recipients": []}));
        assert_eq!(status.recipients, Some(Vec::new()));
        let (status, notes) = AdpStatus::read(&json!({"recipients": {}, "resources": 5}));
        assert_eq!((status.status.resources, status.recipients), (None, None));
        assert_eq!(
            notes,
            [
                "resources: expected an object (got 5)",
                "recipients: expected a list (got {})"
            ]
        );
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

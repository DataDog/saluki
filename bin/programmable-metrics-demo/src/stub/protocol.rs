//! The artifact channel stub protocol, version 2: the messages ADP and the stub exchange.
//!
//! ADP polls `POST {stub_url}/v1/artifacts/poll` with its acknowledgments and a status summary, and receives a complete
//! snapshot of the product it asks for, `policy`: at most one config, the program. Each config's `payload` is a JSON
//! **string** holding the envelope, so that the transport hash (`sha256`, lowercase hexadecimal of the payload's UTF-8
//! bytes) covers exact bytes.
//!
//! The privileged status route also lists the recipients of ADP's delivery hosts (`recipients`, see
//! [`RecipientEntry`]), which the demo's Recipients panel shows.
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
pub const PROTOCOL_VERSION: u32 = 2;

/// The product policy artifacts are delivered in.
pub const POLICY_PRODUCT: &str = "policy";

/// The config ID the demo publishes its program under.
pub const DEMO_CONFIG_ID: &str = "demo";

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

    /// `error` names (see [`dropped_outputs`]): the artifact is degraded if there are any. The demo derives this; it is
    /// not a member ADP writes.
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
/// an output that is no longer declared with counted drops.
const EMITS_DROPPED: &str = "its emits are dropped";

/// How each reason for an artifact's `emit`s addressed to recipients that are no longer available starts, followed by
/// the recipients as the program writes them (`to ["http://127.0.0.1:17105"]`) and [`RECIPIENT_EMITS_DROPPED`].
const DROPPED_RECIPIENTS_NOTE: &str = "the emits ";

/// What follows the recipients in a reason that the `emit`s to them are dropped.
const RECIPIENT_EMITS_DROPPED: &str = " are dropped until they are available again";

/// Returns what an `active` acknowledgment's error says the artifact's emits to are dropped, each once, in the order it
/// names them: the outputs, and the recipients written as `to [<name>, ...]`. The artifact runs degraded if there are
/// any.
///
/// ADP joins what composition says about an artifact in effect with `; `. A reason for a degraded artifact starts with
/// the output and says its emits are dropped (``output `implied_team_d` is no longer declared; its emits are dropped
/// until it returns``), or starts with the emits to recipients and says they are dropped (`the emits to
/// ["http://127.0.0.1:17105"] are dropped until they are available again: the host has no recipient ...`). Any other
/// note names nothing dropped: such an artifact runs as written.
pub fn dropped_outputs(error: &str) -> Vec<&str> {
    // Where each note starts: the start of the text, or after the separator.
    let starts = error
        .match_indices("; ")
        .map(|(index, _)| index + 2)
        .chain(std::iter::once(0))
        .filter(|&index| {
            error[index..].starts_with(DROPPED_OUTPUT_NOTE) || error[index..].starts_with(DROPPED_RECIPIENTS_NOTE)
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut dropped = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(error.len());
        let note = &error[start..end];
        let target = if let Some(note) = note.strip_prefix(DROPPED_OUTPUT_NOTE) {
            note.split_once('`')
                .filter(|(_, said)| said.contains(EMITS_DROPPED))
                .map(|(output, _)| output)
        } else {
            note.strip_prefix(DROPPED_RECIPIENTS_NOTE)
                .and_then(|note| note.find(RECIPIENT_EMITS_DROPPED).map(|at| &note[..at]))
        };
        if let Some(target) = target {
            if !dropped.contains(&target) {
                dropped.push(target);
            }
        }
    }
    dropped
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

    /// Whether the active artifact was approved against another active plan than the one it was prepared against.
    pub stale_review: Option<bool>,

    /// Why ADP is degraded, if it is.
    pub degraded: Option<String>,

    /// How many generations are alive.
    pub live_generations: Option<u64>,

    /// Which program the active generation runs, and whether native policy settings apply to it, when ADP says.
    pub program: Option<ProgramStatus>,
}

/// Which program ADP's active generation runs, as its status's `program` member says.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProgramStatus {
    /// `default` (the default program, lowered from native configuration) or `published`.
    #[serde(default)]
    pub runs: String,

    /// `applied` (the default program's native policy settings apply) or `not_applied` (a published program owns its
    /// policy).
    #[serde(default)]
    pub native_policy: String,

    /// The nodes of the default program, by name, whose native policy settings the running program does not run as
    /// they are now.
    #[serde(default)]
    pub differs: Vec<String>,
}

impl ChannelStatus {
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
            stale_review: member(status, path, "stale_review", notes).flatten(),
            degraded: member(status, path, "degraded", notes).flatten(),
            live_generations: member(status, path, "live_generations", notes).flatten(),
            program: member::<Option<ProgramStatus>>(status, path, "program", notes).flatten(),
        }
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
    /// Reads the `active_artifact` member of a status, noting the fields that cannot be read.
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
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Envelope {
    /// The artifact revision, which must exceed ADP's revision high-water mark of the config's product.
    pub revision: u64,

    /// The plan crate's hash of the artifact document as received: the digest of its canonical JSON form.
    pub artifact_hash: String,

    /// What the approver reviewed the artifact against, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_against: Option<ApprovedAgainst>,

    /// The artifact document: a program.
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
            "protocol_version": 2, "client": {"id": "a1", "version": "1.8.0", "build": "x"},
            "products": ["policy"], "future_field": [1, 2],
            "config_states": [{"product": "policy", "id": "demo", "version": 7, "revision": 7,
                               "artifact_hash": "sha256:00", "state": "active", "generation": 12,
                               "cut_at": null, "error": null, "phase": "steady"}],
            "status": {"revision_high_water": 7, "active_artifact": null, "live_generations": 2,
                       "new_status_field": true}
        }));
        assert!(notes.is_empty(), "{:?}", notes);
        assert_eq!(poll.protocol_version, Some(2));
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
            "protocol_version": 2,
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
        assert_eq!(poll.protocol_version, Some(2));
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
        let (poll, notes) = PollRequest::read(&json!({"protocol_version": "2"}));
        assert_eq!(poll.protocol_version, None);
        assert_eq!(notes.len(), 1);
        let (_, notes) = PollRequest::read(&json!([1]));
        assert_eq!(notes, ["poll: expected an object (got [1])"]);
    }

    #[test]
    fn an_active_acknowledgment_is_degraded_only_by_reasons_that_drop_emits() {
        // The reasons and the note as the plan crate writes them, joined as ADP joins them.
        let removed = "output `implied_team_d` is no longer declared; its emits are dropped until it returns";
        let other = "output `b` is no longer declared; its emits are dropped until it returns";
        let rebase = "a note about `dd_metrics_encode` and `metrics_routing_encode_0` that drops nothing";
        assert_eq!(dropped_outputs(removed), ["implied_team_d"]);
        assert_eq!(dropped_outputs(rebase), Vec::<&str>::new());
        assert_eq!(
            dropped_outputs(&[other, rebase, removed].join("; ")),
            ["b", "implied_team_d"]
        );
        assert_eq!(dropped_outputs(&[rebase, other, other].join("; ")), ["b"]);
        // An output named inside a note does not start one, and a refusal drops nothing.
        assert_eq!(
            dropped_outputs("waiting for output `implied_team_d`"),
            Vec::<&str>::new()
        );
        assert_eq!(
            dropped_outputs("output `b` is not declared by the default program"),
            Vec::<&str>::new()
        );
        // Emits addressed to recipients that are gone are named by their recipients, as the program writes them.
        let gone =
            "the emits to [\"http://127.0.0.1:50001\"] are dropped until they are available again: the host has \
                    no recipient `http://127.0.0.1:50001`";
        assert_eq!(dropped_outputs(gone), ["to [\"http://127.0.0.1:50001\"]"]);
        assert_eq!(
            dropped_outputs(&[removed, gone].join("; ")),
            ["implied_team_d", "to [\"http://127.0.0.1:50001\"]"]
        );

        let (status, notes) = AdpStatus::read(&json!({"config_states": [
            {"id": "demo", "revision": 4, "state": "active", "error": format!("{}; {}", removed, rebase)},
            {"id": "demo-2", "revision": 5, "state": "active", "error": rebase},
            {"id": "demo-3", "revision": 6, "state": "rejected", "error": other},
        ]}));
        assert!(notes.is_empty(), "{:?}", notes);
        let dropped = status
            .config_states
            .iter()
            .map(|state| state.dropped_outputs.clone())
            .collect::<Vec<_>>();
        assert_eq!(dropped, [vec!["implied_team_d".to_string()], Vec::new(), Vec::new()]);
        // The UI reads it as a member of the acknowledgment, absent when nothing is dropped.
        let written = serde_json::to_value(&status.config_states).unwrap();
        assert_eq!(written[0]["dropped_outputs"], json!(["implied_team_d"]));
        assert!(written[1].get("dropped_outputs").is_none());
    }

    #[test]
    fn the_recipients_are_read_field_by_field() {
        let (status, notes) = AdpStatus::read(&json!({
            "revision_high_water": 7,
            // A member of an earlier version, which is not read.
            "resources": {"revision_high_water": 3},
            "recipients": [
                {"host": "dd_out", "id": "dd_out/primary/0", "slot": 0, "incarnation": 1, "state": "live",
                 "queue_bytes": 2048, "future_field": true},
                {"host": "dd_out", "id": "dd_out/additional/0/http://127.0.0.1:17105", "slot": 1, "incarnation": "2",
                 "state": "retired",
                 "last_drain": {"delivered": 4, "persisted": 0, "dropped": 2, "deadline_reached": true,
                                "retired_ms": 1_700_000_000_000_u64}},
                "not a recipient"
            ]
        }));
        assert_eq!(status.status.revision_high_water, Some(7));

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

        // A status of an ADP before stage 7 lists no recipients, which is not the same as listing none.
        let (status, notes) = AdpStatus::read(&json!({"revision_high_water": 1}));
        assert!(notes.is_empty());
        assert_eq!(status.recipients, None);
        let (status, _) = AdpStatus::read(&json!({"recipients": []}));
        assert_eq!(status.recipients, Some(Vec::new()));
        let (status, notes) = AdpStatus::read(&json!({"recipients": {}}));
        assert_eq!(status.recipients, None);
        assert_eq!(notes, ["recipients: expected a list (got {})"]);
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

        let (status, notes) = AdpStatus::read(&json!({"stale_review": "yes", "config_states": {}}));
        assert_eq!(status, AdpStatus::default());
        assert_eq!(
            notes,
            [
                "stale_review: invalid type: string \"yes\", expected a boolean (got \"yes\")",
                "config_states: expected a list (got {})"
            ]
        );
    }
}

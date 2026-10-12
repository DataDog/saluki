//! The artifact channel stub: the server side of the channel that ADP polls for policy and resources artifacts.
//!
//! The stub implements the stub protocol (see [`protocol`]): every poll gets the complete current snapshot of each
//! product it asks for. The `policy` product holds at most one config per scope of the default program: `demo` for the
//! policy scope, and `demo-<scope>` for another (see [`protocol::config_id`]). The `resources` product holds at most
//! one config, `demo-resources`, with revisions of its own. The snapshots are durable (see [`store`]); revoking empties
//! one, or removes one scope's config. Every poll is recorded with its receive time, including polls that are refused
//! or partly unreadable, and consecutive polls that report the same thing are folded into one entry of the
//! acknowledgment timeline, which holds the acknowledgments of both products.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use saluki_error::GenericError;
use serde::Serialize;
use serde_json::{json, Value};

use crate::clock::unix_millis;

pub mod protocol;
use self::protocol::{
    ApprovedAgainst, ChannelStatus, ConfigState, Envelope, PollRequest, PollResponse, POLICY_PRODUCT, POLL_PATH,
    PROTOCOL_VERSION, RESOURCES_PRODUCT,
};

pub mod store;
use self::store::{ArtifactStore, HistoryEntry, PublishedConfig, PublishedResources};

/// How many raw polls the stub keeps.
const POLL_CAPACITY: usize = 120;

/// How many acknowledgment timeline entries the stub keeps.
const TIMELINE_CAPACITY: usize = 200;

/// One poll, as received.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PollRecord {
    /// When the poll arrived, in milliseconds since the Unix epoch.
    pub received_ms: u64,

    /// The polling process's ID.
    pub client_id: String,

    /// The polling process's version.
    pub client_version: String,

    /// The acknowledgments the poll carried.
    pub config_states: Vec<ConfigState>,

    /// The status summary the poll carried.
    pub status: Option<ChannelStatus>,

    /// The fields of the poll that could not be read (see [`PollRequest::read`]).
    pub notes: Vec<String>,

    /// Why the poll was refused, if it was.
    pub error: Option<String>,
}

/// A run of consecutive polls that reported the same acknowledgments and status.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimelineEntry {
    /// When the first poll of the run arrived.
    pub first_ms: u64,

    /// When the last poll of the run arrived.
    pub last_ms: u64,

    /// How many polls the run has.
    pub polls: u64,

    /// The polling process's ID.
    pub client_id: String,

    /// The acknowledgments.
    pub config_states: Vec<ConfigState>,

    /// The status summary.
    pub status: Option<ChannelStatus>,

    /// The fields of the polls that could not be read.
    pub notes: Vec<String>,

    /// Why the polls were refused, if they were.
    pub error: Option<String>,
}

impl TimelineEntry {
    fn reports_the_same_as(&self, record: &PollRecord) -> bool {
        self.client_id == record.client_id
            && self.config_states == record.config_states
            && self.status == record.status
            && self.notes == record.notes
            && self.error == record.error
    }
}

#[derive(Default)]
struct PollLog {
    polls_total: u64,
    records: VecDeque<PollRecord>,
    timeline: VecDeque<TimelineEntry>,
}

impl PollLog {
    /// Records a poll, and returns `true` if it starts a new timeline entry.
    fn record(&mut self, record: PollRecord) -> bool {
        self.polls_total += 1;
        let new_entry = match self.timeline.back_mut() {
            Some(entry) if entry.reports_the_same_as(&record) => {
                entry.last_ms = record.received_ms;
                entry.polls += 1;
                false
            }
            _ => {
                if self.timeline.len() == TIMELINE_CAPACITY {
                    self.timeline.pop_front();
                }
                self.timeline.push_back(TimelineEntry {
                    first_ms: record.received_ms,
                    last_ms: record.received_ms,
                    polls: 1,
                    client_id: record.client_id.clone(),
                    config_states: record.config_states.clone(),
                    status: record.status.clone(),
                    notes: record.notes.clone(),
                    error: record.error.clone(),
                });
                true
            }
        };
        if self.records.len() == POLL_CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(record);
        new_entry
    }

    /// Returns the latest status any accepted poll carried, with when that poll arrived.
    fn latest_status(&self) -> Option<(u64, &ChannelStatus)> {
        self.records
            .iter()
            .rev()
            .filter(|record| record.error.is_none())
            .find_map(|record| record.status.as_ref().map(|status| (record.received_ms, status)))
    }
}

/// What the stub knows, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ChannelView {
    /// The config served for the policy scope, if any.
    pub current: Option<CurrentConfig>,

    /// The config served for each scope, in scope order.
    pub configs: Vec<CurrentConfig>,

    /// The highest revision issued.
    pub revision_counter: u64,

    /// The revision the next publish would use, given what ADP has reported.
    pub next_revision: u64,

    /// Every poll received.
    pub polls_total: u64,

    /// The most recent poll.
    pub last_poll: Option<PollRecord>,

    /// The acknowledgment timeline, most recent first: the acknowledgments of both products.
    pub timeline: Vec<TimelineEntry>,

    /// The store's most recent changes of the policy product, most recent first.
    pub history: Vec<HistoryEntry>,

    /// What the stub serves for the resources product.
    pub resources: ResourcesView,
}

/// What the stub serves for the resources product, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ResourcesView {
    /// The resources config served, if any.
    pub current: Option<ResourcesConfig>,

    /// The highest resources revision issued.
    pub revision_counter: u64,

    /// The resources revision the next publish would use, given what ADP has reported.
    pub next_revision: u64,

    /// The store's most recent changes of the resources product, most recent first.
    pub history: Vec<HistoryEntry>,
}

/// The resources config the stub serves, without its payload.
#[derive(Clone, Debug, Serialize)]
pub struct ResourcesConfig {
    /// The config ID.
    pub id: String,

    /// The resources revision.
    pub revision: u64,

    /// The hash of the resources document.
    pub artifact_hash: String,

    /// The transport hash.
    pub sha256: String,

    /// When it was published.
    pub published_ms: u64,
}

impl From<&PublishedResources> for ResourcesConfig {
    fn from(config: &PublishedResources) -> Self {
        Self {
            id: config.id.clone(),
            revision: config.revision,
            artifact_hash: config.artifact_hash.clone(),
            sha256: config.sha256.clone(),
            published_ms: config.published_ms,
        }
    }
}

/// The config the stub serves for one scope, without its payload.
#[derive(Clone, Debug, Serialize)]
pub struct CurrentConfig {
    /// The config ID.
    pub id: String,

    /// The scope the config's artifact replaces.
    pub scope: String,

    /// The artifact revision.
    pub revision: u64,

    /// The artifact hash.
    pub artifact_hash: String,

    /// The transport hash.
    pub sha256: String,

    /// When it was published.
    pub published_ms: u64,
}

/// The artifact channel stub.
///
/// Cheap to clone: clones share the store and the poll log.
#[derive(Clone)]
pub struct StubChannel {
    store: Arc<Mutex<ArtifactStore>>,
    polls: Arc<Mutex<PollLog>>,
}

impl StubChannel {
    /// Creates a stub serving the given store.
    pub fn new(store: ArtifactStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            polls: Arc::new(Mutex::new(PollLog::default())),
        }
    }

    /// Answers a poll that arrived at `now_ms`, and records it with the notes from reading it.
    ///
    /// Every poll that states protocol version 1 is answered with the snapshot, whatever else in it could not be read.
    ///
    /// # Errors
    ///
    /// Returns an error message, and records the poll as refused, if the poll states another protocol version or none
    /// that can be read.
    pub fn poll(&self, request: PollRequest, notes: Vec<String>, now_ms: u64) -> Result<PollResponse, String> {
        let error = match request.protocol_version {
            Some(PROTOCOL_VERSION) => None,
            Some(version) => Some(format!(
                "Unsupported stub protocol version {}: this demo server speaks version {}.",
                version, PROTOCOL_VERSION
            )),
            None => Some(format!(
                "The poll has no readable protocol_version: this demo server speaks version {}.",
                PROTOCOL_VERSION
            )),
        };
        self.record(PollRecord {
            received_ms: now_ms,
            client_id: request.client.id,
            client_version: request.client.version,
            config_states: request.config_states,
            status: request.status,
            notes,
            error: error.clone(),
        });
        if let Some(error) = error {
            return Err(error);
        }

        let mut response = PollResponse {
            protocol_version: PROTOCOL_VERSION,
            ..PollResponse::default()
        };
        // A complete snapshot of every product asked for. A poll that names no products gets the policy product, as
        // the first version of the protocol had only that one; another product is served only to a poll that asks for
        // it, since ADP reads only the products it asks for.
        let asked = |name: &str| request.products.iter().any(|product| product == name);
        let store = self.lock_store();
        if request.products.is_empty() || asked(POLICY_PRODUCT) {
            response.products.insert(POLICY_PRODUCT.to_string(), store.snapshot());
        }
        if asked(RESOURCES_PRODUCT) {
            response
                .products
                .insert(RESOURCES_PRODUCT.to_string(), store.resources_snapshot());
        }
        Ok(response)
    }

    /// Records a poll whose body could not be read at all, which is refused.
    pub fn record_unreadable(&self, error: String, now_ms: u64) {
        self.record(PollRecord {
            received_ms: now_ms,
            client_id: String::new(),
            client_version: String::new(),
            config_states: Vec::new(),
            status: None,
            notes: Vec::new(),
            error: Some(error),
        });
    }

    fn record(&self, record: PollRecord) {
        let problem = record
            .error
            .clone()
            .or_else(|| (!record.notes.is_empty()).then(|| record.notes.join("; ")));
        let new_entry = self.lock_polls().record(record);
        // Logged once per run of identical polls, not once per poll.
        if let (true, Some(problem)) = (new_entry, problem) {
            tracing::warn!(%problem, "An artifact channel poll was refused or could not be read in full.");
        }
    }

    /// Returns the latest status ADP reported in a poll, with when the poll arrived.
    pub fn latest_status(&self) -> Option<(u64, ChannelStatus)> {
        self.lock_polls()
            .latest_status()
            .map(|(received_ms, status)| (received_ms, status.clone()))
    }

    /// Returns the config the stub serves for each scope, in scope order.
    pub fn configs(&self) -> Vec<CurrentConfig> {
        self.lock_store().configs().map(current_config).collect()
    }

    /// Returns the envelope of the config the stub serves for each scope, with the scope, in scope order: what ADP
    /// receives for it. A payload that is not an envelope, which the stub never writes, is left out.
    pub fn envelopes(&self) -> Vec<(String, Envelope)> {
        self.lock_store()
            .configs()
            .filter_map(|config| {
                let envelope = serde_json::from_str::<Envelope>(&config.payload).ok()?;
                Some((config.scope.clone(), envelope))
            })
            .collect()
    }

    /// Publishes an artifact document as the next revision.
    ///
    /// The revision is one more than the larger of the store's counter and ADP's revision high-water mark, taken from
    /// the latest poll and from `reported_high_water` (such as the privileged status route's), whichever is higher.
    ///
    /// # Errors
    ///
    /// Returns an error if the document cannot be hashed or the store cannot be written.
    pub fn publish(
        &self, artifact: Value, approved_against: Option<ApprovedAgainst>, reported_high_water: Option<u64>,
        now_ms: u64,
    ) -> Result<PublishedConfig, GenericError> {
        let high_water = self.high_water(reported_high_water);
        self.lock_store()
            .publish(artifact, approved_against, high_water, now_ms)
    }

    /// Removes the config of `scope` from the snapshot, or empties it when `scope` is `None`, revoking the artifacts.
    /// Returns `false`, and changes nothing, if there is nothing to remove.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be written.
    pub fn revoke(&self, scope: Option<&str>, now_ms: u64) -> Result<bool, GenericError> {
        self.lock_store().revoke(scope, now_ms)
    }

    /// Returns the revision the next publish would use, counting `reported_high_water` as [`publish`][Self::publish]
    /// does.
    pub fn next_revision(&self, reported_high_water: Option<u64>) -> u64 {
        let high_water = self.high_water(reported_high_water);
        self.lock_store().next_revision(high_water)
    }

    /// Returns the resources document the stub serves, with its config, if it serves one.
    pub fn resources(&self) -> Option<(ResourcesConfig, Value)> {
        let store = self.lock_store();
        let config = store.resources()?;
        let envelope = serde_json::from_str::<Envelope>(&config.payload).ok()?;
        Some((ResourcesConfig::from(config), envelope.artifact))
    }

    /// Publishes a resources document as the next resources revision.
    ///
    /// The revision is one more than the larger of the store's resources counter and ADP's resources revision
    /// high-water mark, taken from the latest poll and from `reported_high_water` (such as the privileged status
    /// route's), whichever is higher: the resources product's revisions are apart from the policy product's.
    ///
    /// # Errors
    ///
    /// Returns an error if the document cannot be hashed or the store cannot be written.
    pub fn publish_resources(
        &self, document: Value, reported_high_water: Option<u64>, now_ms: u64,
    ) -> Result<ResourcesConfig, GenericError> {
        let high_water = self.resources_high_water(reported_high_water);
        self.lock_store()
            .publish_resources(document, high_water, now_ms)
            .map(|config| ResourcesConfig::from(&config))
    }

    /// Empties the resources snapshot, revoking every resource the resources artifact declared. Returns `false`, and
    /// changes nothing, if it is already empty.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be written.
    pub fn revoke_resources(&self, now_ms: u64) -> Result<bool, GenericError> {
        self.lock_store().revoke_resources(now_ms)
    }

    /// Returns what the stub knows, with up to `timeline_limit` timeline entries.
    ///
    /// The next revisions count `reported_high_water` and `reported_resources_high_water` as
    /// [`publish`][Self::publish] and [`publish_resources`][Self::publish_resources] do.
    pub fn view(
        &self, timeline_limit: usize, reported_high_water: Option<u64>, reported_resources_high_water: Option<u64>,
    ) -> ChannelView {
        let high_water = self.high_water(reported_high_water);
        let resources_high_water = self.resources_high_water(reported_resources_high_water);
        let (current, configs, revision_counter, next_revision, history, resources) = {
            let store = self.lock_store();
            let recent = |resources: bool| -> Vec<HistoryEntry> {
                store
                    .history()
                    .iter()
                    .rev()
                    .filter(|entry| entry.is_resources() == resources)
                    .take(20)
                    .cloned()
                    .collect()
            };
            (
                store.current().map(current_config),
                store.configs().map(current_config).collect(),
                store.revision_counter(),
                store.next_revision(high_water),
                recent(false),
                ResourcesView {
                    current: store.resources().map(ResourcesConfig::from),
                    revision_counter: store.resources_revision_counter(),
                    next_revision: store.next_resources_revision(resources_high_water),
                    history: recent(true),
                },
            )
        };
        let polls = self.lock_polls();
        ChannelView {
            current,
            configs,
            revision_counter,
            next_revision,
            polls_total: polls.polls_total,
            last_poll: polls.records.back().cloned(),
            timeline: polls.timeline.iter().rev().take(timeline_limit).cloned().collect(),
            history,
            resources,
        }
    }

    /// Returns ADP's revision high-water mark: the larger of the latest poll's and `reported_high_water`.
    fn high_water(&self, reported_high_water: Option<u64>) -> Option<u64> {
        self.latest_status()
            .and_then(|(_, status)| status.revision_high_water)
            .max(reported_high_water)
    }

    /// Returns ADP's resources revision high-water mark: the larger of the latest poll's and `reported_high_water`.
    fn resources_high_water(&self, reported_high_water: Option<u64>) -> Option<u64> {
        self.latest_status()
            .and_then(|(_, status)| status.resources.and_then(|resources| resources.revision_high_water))
            .max(reported_high_water)
    }

    fn lock_store(&self) -> std::sync::MutexGuard<'_, ArtifactStore> {
        self.store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_polls(&self) -> std::sync::MutexGuard<'_, PollLog> {
        self.polls.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn current_config(config: &PublishedConfig) -> CurrentConfig {
    CurrentConfig {
        id: config.id.clone(),
        scope: config.scope.clone(),
        revision: config.revision,
        artifact_hash: config.artifact_hash.clone(),
        sha256: config.sha256.clone(),
        published_ms: config.published_ms,
    }
}

/// Builds the stub's router: `POST /v1/artifacts/poll`.
pub fn router(channel: StubChannel) -> Router {
    Router::new().route(POLL_PATH, post(handle_poll)).with_state(channel)
}

async fn handle_poll(State(channel): State<StubChannel>, body: Bytes) -> Response {
    let now_ms = unix_millis();
    let refused = |message: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => {
            let message = format!("The poll body is not JSON: {}", error);
            channel.record_unreadable(message.clone(), now_ms);
            return refused(message);
        }
    };
    let (request, notes) = PollRequest::read(&value);
    match channel.poll(request, notes, now_ms) {
        Ok(response) => Json(response).into_response(),
        Err(message) => refused(message),
    }
}

#[cfg(test)]
mod tests;

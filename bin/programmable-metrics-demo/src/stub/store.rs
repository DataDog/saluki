//! The durable artifact store: the snapshot the stub serves, the revision counter, and the history of changes.
//!
//! ADP persists nothing itself; persistence and redelivery belong to the artifact channel. The store keeps its state in
//! the run directory, so restarting the demo with the same run directory serves the same snapshot, and the revision
//! counter never goes back:
//!
//! - `store.json`: the revision counter and the current config, if any. Every change writes a new file, flushes it to
//!   disk, and renames it over the old one, so the file always holds one complete state. A publish or revoke takes
//!   effect exactly when this file is replaced: if writing it fails, nothing changes.
//! - `history.jsonl`: one line per publish or revoke, appended after the change. It is a log, not part of the state: a
//!   line that cannot be written is logged and the change stands. The store also reads the highest revision from it,
//!   so that the counter keeps increasing if `store.json` is moved aside.

use std::{
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{canonical::PlanHash, ir::ReceivedArtifact};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{transport_hash, ApprovedAgainst, ConfigEntry, Envelope, DEMO_CONFIG_ID};

/// The format version of `store.json`.
const STORE_FORMAT: u32 = 1;

/// How many history entries the store keeps in memory (the file keeps every one).
const HISTORY_IN_MEMORY: usize = 200;

/// The config the stub currently serves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublishedConfig {
    /// The config ID.
    pub id: String,

    /// The config version, equal to the revision.
    pub version: u64,

    /// The artifact revision.
    pub revision: u64,

    /// The plan crate's hash of the artifact document.
    pub artifact_hash: String,

    /// The transport hash of the payload.
    pub sha256: String,

    /// The envelope, as the exact JSON text served.
    pub payload: String,

    /// When the config was published, in milliseconds since the Unix epoch.
    pub published_ms: u64,
}

impl PublishedConfig {
    /// Returns the config as a snapshot entry.
    pub fn entry(&self) -> ConfigEntry {
        ConfigEntry {
            id: self.id.clone(),
            version: self.version,
            sha256: self.sha256.clone(),
            payload: self.payload.clone(),
        }
    }
}

/// What a history entry records.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAction {
    /// An artifact was published.
    Publish,

    /// The snapshot was emptied, revoking the artifact.
    Revoke,
}

/// One change to the store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    /// When the change was made, in milliseconds since the Unix epoch.
    pub at_ms: u64,

    /// What was done.
    pub action: HistoryAction,

    /// The revision published, for a publish.
    pub revision: Option<u64>,

    /// The artifact hash published, for a publish.
    pub artifact_hash: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoreFile {
    format: u32,
    revision_counter: u64,
    current: Option<PublishedConfig>,
}

/// The durable artifact store.
pub struct ArtifactStore {
    dir: PathBuf,
    revision_counter: u64,
    current: Option<PublishedConfig>,
    history: Vec<HistoryEntry>,
}

impl ArtifactStore {
    /// Opens the store in `dir`, creating the directory if it does not exist.
    ///
    /// The revision counter is the larger of the store file's and the highest revision in the history.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, or if a store file in it cannot be read.
    pub fn open(dir: &Path) -> Result<Self, GenericError> {
        fs::create_dir_all(dir)
            .with_error_context(|| format!("Failed to create the artifact store directory '{}'.", dir.display()))?;

        let store_path = dir.join("store.json");
        let (revision_counter, current) = match fs::read_to_string(&store_path) {
            Ok(text) => {
                let file: StoreFile = serde_json::from_str(&text).with_error_context(|| {
                    format!(
                        "Failed to read the artifact store '{}'. Move it aside to start with an empty store; the \
                         revision counter then continues from the history.",
                        store_path.display()
                    )
                })?;
                if file.format != STORE_FORMAT {
                    return Err(generic_error!(
                        "The artifact store '{}' has format {}, but this demo reads format {}. Use a new run \
                         directory.",
                        store_path.display(),
                        file.format,
                        STORE_FORMAT
                    ));
                }
                (file.revision_counter, file.current)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, None),
            Err(error) => {
                return Err(generic_error!(
                    "Failed to read the artifact store '{}': {}",
                    store_path.display(),
                    error
                ))
            }
        };

        let history_path = dir.join("history.jsonl");
        let mut history = match fs::read_to_string(&history_path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let history_high_water = history.iter().filter_map(|entry| entry.revision).max().unwrap_or(0);
        if history.len() > HISTORY_IN_MEMORY {
            history.drain(..history.len() - HISTORY_IN_MEMORY);
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            revision_counter: revision_counter.max(history_high_water),
            current,
            history,
        })
    }

    /// Returns the config the stub serves, if any.
    pub fn current(&self) -> Option<&PublishedConfig> {
        self.current.as_ref()
    }

    /// Returns the highest revision the store has issued.
    pub fn revision_counter(&self) -> u64 {
        self.revision_counter
    }

    /// Returns the most recent changes, oldest first.
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Returns the revision the next publish uses: one more than the larger of the store's counter and ADP's
    /// revision high-water mark.
    pub fn next_revision(&self, high_water: Option<u64>) -> u64 {
        self.revision_counter.max(high_water.unwrap_or(0)) + 1
    }

    /// Returns the snapshot of the policy product: the current config, or nothing.
    pub fn snapshot(&self) -> Vec<ConfigEntry> {
        self.current.iter().map(PublishedConfig::entry).collect()
    }

    /// Publishes an artifact document as the next revision, replacing the current config.
    ///
    /// The envelope's `artifact_hash` is computed here, from the document, with the plan crate's
    /// [`ReceivedArtifact`], exactly as ADP recomputes it on receipt.
    ///
    /// # Errors
    ///
    /// Returns an error if the document is not a policy artifact or has no canonical form, or if the store file cannot
    /// be written. A failed publish changes nothing.
    pub fn publish(
        &mut self, artifact: Value, approved_against: Option<ApprovedAgainst>, high_water: Option<u64>, now_ms: u64,
    ) -> Result<PublishedConfig, GenericError> {
        let received = ReceivedArtifact::from_value(artifact.clone())
            .map_err(|error| generic_error!("The artifact cannot be published: {}", error))?;
        let revision = self.next_revision(high_water);
        let config = build_config(revision, &received.hash, approved_against, artifact, now_ms)?;

        self.write(revision, Some(config.clone()))?;
        self.revision_counter = revision;
        self.current = Some(config.clone());
        self.append_history(HistoryEntry {
            at_ms: now_ms,
            action: HistoryAction::Publish,
            revision: Some(revision),
            artifact_hash: Some(config.artifact_hash.clone()),
        });
        Ok(config)
    }

    /// Empties the snapshot, which revokes the artifact: ADP installs the default program.
    ///
    /// Returns `false`, and changes nothing, if the snapshot is already empty.
    ///
    /// # Errors
    ///
    /// Returns an error if the store file cannot be written. A failed revoke changes nothing.
    pub fn revoke(&mut self, now_ms: u64) -> Result<bool, GenericError> {
        if self.current.is_none() {
            return Ok(false);
        }
        self.write(self.revision_counter, None)?;
        self.current = None;
        self.append_history(HistoryEntry {
            at_ms: now_ms,
            action: HistoryAction::Revoke,
            revision: None,
            artifact_hash: None,
        });
        Ok(true)
    }

    /// Replaces `store.json`: writes the new state to a temporary file, flushes it to disk, renames it over the store
    /// file, and flushes the directory, so that a crash leaves either the old state or the new one.
    fn write(&self, revision_counter: u64, current: Option<PublishedConfig>) -> Result<(), GenericError> {
        let file = StoreFile {
            format: STORE_FORMAT,
            revision_counter,
            current,
        };
        let text = serde_json::to_string_pretty(&file).error_context("Failed to serialize the artifact store.")?;
        let path = self.dir.join("store.json");
        let temporary = self.dir.join("store.json.tmp");
        File::create(&temporary)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .with_error_context(|| format!("Failed to write the artifact store '{}'.", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_error_context(|| format!("Failed to replace the artifact store '{}'.", path.display()))?;
        // The rename has happened; flushing the directory makes it durable. A failure here cannot undo the change, so
        // it is logged rather than returned. (Only Unix can open a directory to flush it.)
        #[cfg(unix)]
        if let Err(error) = File::open(&self.dir).and_then(|dir| dir.sync_all()) {
            tracing::warn!(%error, dir = %self.dir.display(), "Failed to flush the artifact store's directory.");
        }
        Ok(())
    }

    /// Appends a line to the history. A line that cannot be written is logged: the change it records has already
    /// taken effect.
    fn append_history(&mut self, entry: HistoryEntry) {
        let path = self.dir.join("history.jsonl");
        let written = serde_json::to_string(&entry)
            .map_err(std::io::Error::other)
            .and_then(|mut line| {
                line.push('\n');
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut file| file.write_all(line.as_bytes()))
            });
        if let Err(error) = written {
            tracing::warn!(
                %error,
                path = %path.display(),
                "Failed to append to the artifact history; the change stands."
            );
        }
        if self.history.len() == HISTORY_IN_MEMORY {
            self.history.remove(0);
        }
        self.history.push(entry);
    }
}

/// Builds the config for an artifact: its envelope as JSON text, and that text's transport hash.
fn build_config(
    revision: u64, artifact_hash: &PlanHash, approved_against: Option<ApprovedAgainst>, artifact: Value, now_ms: u64,
) -> Result<PublishedConfig, GenericError> {
    let envelope = Envelope {
        revision,
        artifact_hash: artifact_hash.to_string(),
        approved_against,
        artifact,
    };
    let payload = serde_json::to_string(&envelope).error_context("Failed to serialize the envelope.")?;
    Ok(PublishedConfig {
        id: DEMO_CONFIG_ID.to_string(),
        version: revision,
        revision,
        artifact_hash: envelope.artifact_hash,
        sha256: transport_hash(&payload),
        payload,
        published_ms: now_ms,
    })
}

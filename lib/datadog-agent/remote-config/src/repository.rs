//! The worker's protocol state: its cursor, its cache of assigned files, and the statuses it reports.
//!
//! All of this is discarded when the worker restarts, so a restarted worker fetches and decodes everything again.

use std::collections::{BTreeMap, HashMap};

use datadog_protos::remote_config::{
    Client, ClientAgent, ClientGetConfigsRequest, ClientGetConfigsResponse, ClientState, ConfigState, ConfigStatus,
    TargetFileHash, TargetFileMeta,
};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error, info, warn};

use crate::decoder::{Outcome, Verdict, PANICKED};
use crate::metrics::{Metrics, PollOutcome, RejectionStage};
use crate::protocol::{root_version, ConfigPath, TargetMeta, Targets};
use crate::registry::{Generation, Shared};
use crate::{ClientKind, ConfigId, RcClientConfiguration};

/// Apply states, as the Agent numbers them.
const UNACKNOWLEDGED: u64 = 1;
const ACKNOWLEDGED: u64 = 2;
const ERROR: u64 = 3;

pub(crate) const COLLISION: &str =
    "Several configuration files share this configuration ID, so none of them was applied.";

/// One assigned configuration file.
struct CachedFile {
    path: ConfigPath,
    meta: TargetMeta,

    /// The opaque product-specific payload.
    ///
    /// Retained because the Agent omits unchanged files that the client advertises as cached. A decoder is rebuilt from
    /// these bytes whenever a product's assignment changes, so the client caches payloads rather than decoded values
    /// and has no decoded state to invalidate.
    contents: Vec<u8>,
}

/// What a product was last decoded from, and the verdicts that decoding produced.
struct Decoded {
    /// The subscription the snapshot was published to.
    generation: Generation,

    /// The product's assigned paths and their hashes. The product is decoded again only when these change.
    inputs: Vec<(String, [u8; 32])>,

    /// Each configuration's verdict from its product's last decoding.
    verdicts: BTreeMap<ConfigId, Verdict>,
}

pub(crate) struct Repository {
    root_version: u64,
    targets_version: u64,
    backend_state: Vec<u8>,

    /// Assigned files keyed by their full path.
    files: BTreeMap<String, CachedFile>,

    /// Subscribed products that have been decoded since their subscription was created.
    ///
    /// A subscribed product missing from here makes the next request ask for the full assignment; see
    /// [`request`](Self::request).
    decoded: BTreeMap<String, Decoded>,

    /// Why the last poll failed, reported to the Agent on the next one.
    pub(crate) last_error: Option<String>,

    /// Whether the last response reported the Agent's configuration as expired.
    expired: bool,
}

impl Repository {
    pub(crate) fn new() -> Self {
        Self {
            // The Agent rejects a root version below 1, and does not send the initial root.
            root_version: 1,
            targets_version: 0,
            backend_state: Vec::new(),
            files: BTreeMap::new(),
            decoded: BTreeMap::new(),
            last_error: None,
            expired: false,
        }
    }

    /// Forgets the files and statuses of products that are no longer subscribed, and the decoding of any product that
    /// has been subscribed again since.
    pub(crate) fn prune(&mut self, live: &BTreeMap<String, Generation>) {
        self.files.retain(|_, file| live.contains_key(&file.path.product));
        self.decoded
            .retain(|product, decoded| live.get(product) == Some(&decoded.generation));
    }

    /// Builds the next poll for the products in `live`, which should have been passed to [`prune`](Self::prune) first.
    pub(crate) fn request(
        &self, client_id: &str, config: &RcClientConfiguration, live: &BTreeMap<String, Generation>,
    ) -> ClientGetConfigsRequest {
        // The Agent answers with nothing at all while our targets version matches its own, even when our product list
        // has changed. Sending version 0 makes it send the full assignment, omitting only the files we advertise as
        // cached, so a product subscribed after the first poll is delivered without waiting for an unrelated change.
        let targets_version = if live.keys().all(|product| self.decoded.contains_key(product)) {
            self.targets_version
        } else {
            0
        };

        let ClientKind::Agent(agent) = &config.kind;
        ClientGetConfigsRequest {
            client: Some(Client {
                state: Some(ClientState {
                    root_version: self.root_version,
                    targets_version,
                    config_states: self.config_states(),
                    has_error: self.last_error.is_some(),
                    error: self.last_error.clone().unwrap_or_default(),
                    backend_client_state: self.backend_state.clone(),
                }),
                id: client_id.to_owned(),
                products: live.keys().cloned().collect(),
                is_agent: true,
                client_agent: Some(ClientAgent {
                    name: agent.name.clone(),
                    version: agent.version.clone(),
                    cluster_name: agent.cluster_name.clone().unwrap_or_default(),
                    cluster_id: agent.cluster_id.clone().unwrap_or_default(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            cached_target_files: self
                .files
                .values()
                // The Agent rejects a whole request that advertises an empty file.
                .filter(|file| file.meta.length > 0)
                .map(|file| TargetFileMeta {
                    path: file.path.raw.clone(),
                    length: file.meta.length as i64,
                    hashes: vec![TargetFileHash {
                        algorithm: "sha256".to_owned(),
                        hash: faster_hex::hex_string(&file.meta.sha256),
                    }],
                })
                .collect(),
        }
    }

    /// Reports one row per configuration ID, which is the finest identity the protocol can report against.
    fn config_states(&self) -> Vec<ConfigState> {
        let mut rows: BTreeMap<(&str, &ConfigId), ConfigState> = BTreeMap::new();
        for file in self.files.values() {
            let product = file.path.product.as_str();
            let id = &file.path.config_id;
            let row = rows.entry((product, id)).or_insert_with(|| {
                let verdict = self.decoded.get(product).and_then(|decoded| decoded.verdicts.get(id));
                let (apply_state, apply_error) = match verdict {
                    None => (UNACKNOWLEDGED, String::new()),
                    Some(Verdict::Acknowledged) => (ACKNOWLEDGED, String::new()),
                    Some(Verdict::DecodeRejected(reason))
                    | Some(Verdict::BuildRejected(reason))
                    | Some(Verdict::Collided(reason)) => (ERROR, reason.clone()),
                    Some(Verdict::Panicked) => (ERROR, PANICKED.to_owned()),
                };
                ConfigState {
                    id: id.to_string(),
                    version: 0,
                    product: product.to_owned(),
                    apply_state,
                    apply_error,
                }
            });
            // Colliding files share a row; report the newest of their versions.
            row.version = row.version.max(file.meta.version);
        }
        rows.into_values().collect()
    }

    /// Applies a response to a poll for the products in `requested`, publishing to every product whose inputs changed.
    ///
    /// Returns how the response left the client's assignment, which counts the poll.
    ///
    /// # Errors
    ///
    /// Returns an error if the response is malformed or a payload does not match its metadata. Nothing is then
    /// committed or published, so the next poll resumes from the same cursor.
    pub(crate) fn apply(
        &mut self, response: ClientGetConfigsResponse, requested: &BTreeMap<String, Generation>, shared: &Shared,
        metrics: &Metrics,
    ) -> Result<PollOutcome, GenericError> {
        let ClientGetConfigsResponse {
            roots,
            targets,
            target_files,
            client_configs,
            config_status,
        } = response;

        // The Agent sends an empty response when nothing has changed.
        if roots.is_empty() && targets.is_empty() && target_files.is_empty() && client_configs.is_empty() {
            return Ok(PollOutcome::Ok);
        }

        // Validate everything before changing anything.
        let targets = Targets::parse(&targets)?;
        let root_version = match roots.last() {
            Some(root) => root_version(root)?,
            None => self.root_version,
        };
        let mut sent: HashMap<String, Vec<u8>> = target_files.into_iter().map(|file| (file.path, file.raw)).collect();
        let mut assigned = Vec::with_capacity(client_configs.len());
        for raw in &client_configs {
            let path = ConfigPath::parse(raw)
                .ok_or_else(|| generic_error!("Assigned configuration path {raw} is malformed."))?;
            if !requested.contains_key(&path.product) {
                continue;
            }
            let meta = targets.meta(raw)?;
            match sent.get(raw) {
                Some(payload) => meta.verify(raw, payload)?,
                None => {
                    let cached = self.files.get(raw);
                    if !cached
                        .is_some_and(|cached| cached.meta.length == meta.length && cached.meta.sha256 == meta.sha256)
                    {
                        return Err(generic_error!(
                            "Configuration {raw} is assigned but neither sent nor cached."
                        ));
                    }
                }
            }
            assigned.push((path, meta));
        }

        let mut previous = std::mem::take(&mut self.files);
        for (path, meta) in assigned {
            let contents = match sent.remove(&path.raw) {
                Some(payload) => payload,
                None => match previous.remove(&path.raw) {
                    Some(cached) => cached.contents,
                    // The same path was assigned twice and its cached copy has already been moved.
                    None => continue,
                },
            };
            self.files.insert(path.raw.clone(), CachedFile { path, meta, contents });
        }
        self.root_version = root_version;
        self.targets_version = targets.version;
        self.backend_state = targets.backend_state;

        let expired = config_status == ConfigStatus::Expired as i32;
        if expired && !self.expired {
            warn!("The Agent reports its Remote Configuration as expired; configurations are withdrawn until it recovers.");
        } else if !expired && self.expired {
            info!("The Agent's Remote Configuration is no longer expired.");
        }
        self.expired = expired;

        for product in requested.keys() {
            self.decode(product, shared, metrics);
        }
        Ok(if expired { PollOutcome::Expired } else { PollOutcome::Ok })
    }

    /// Decodes and publishes `product` if its inputs changed since it was last decoded.
    fn decode(&mut self, product: &str, shared: &Shared, metrics: &Metrics) {
        let files: Vec<&CachedFile> = self
            .files
            .values()
            .filter(|file| file.path.product == product)
            .collect();
        let inputs: Vec<(String, [u8; 32])> = files
            .iter()
            .map(|file| (file.path.raw.clone(), file.meta.sha256))
            .collect();
        if self
            .decoded
            .get(product)
            .is_some_and(|decoded| decoded.inputs == inputs)
        {
            return;
        }

        let mut by_id: BTreeMap<&ConfigId, Vec<&CachedFile>> = BTreeMap::new();
        for file in &files {
            by_id.entry(&file.path.config_id).or_default().push(file);
        }
        let mut assignment = Vec::with_capacity(by_id.len());
        let mut collisions = Vec::new();
        for (id, group) in by_id {
            match group.as_slice() {
                [file] => assignment.push((id.clone(), file.contents.as_slice())),
                _ => {
                    warn!(
                        product,
                        config_id = %id,
                        paths = ?group.iter().map(|file| file.path.raw.as_str()).collect::<Vec<_>>(),
                        "Rejected configurations that share a configuration ID."
                    );
                    collisions.push((id.clone(), Verdict::Collided(COLLISION.to_owned())));
                }
            }
        }

        let Some((generation, evaluation)) = shared.assign(product, assignment) else {
            // Unsubscribed since the poll was sent; the next poll forgets it.
            self.decoded.remove(product);
            return;
        };

        match &evaluation.outcome {
            Outcome::Accepted(()) => {
                debug!(product, "Published a configuration snapshot.");
                metrics.count_snapshot_published(product);
            }
            Outcome::Rejected(reason) => warn!(product, reason, "Rejected a configuration snapshot."),
            Outcome::Panicked => {
                error!(
                    product,
                    "Product decoder panicked; rejected its configuration snapshot."
                );
                // Every assigned configuration panicked; an empty assignment still counts its panic once.
                let panicked = evaluation.verdicts.len().max(1) as u64;
                metrics.count_rejected(product, RejectionStage::Panic, panicked);
            }
        }
        let mut verdicts = BTreeMap::new();
        for (id, verdict) in evaluation.verdicts.into_iter().chain(collisions) {
            // A build or panic rejection was logged once for the whole snapshot above, and a panic was counted there.
            match &verdict {
                Verdict::Acknowledged | Verdict::Panicked => {}
                Verdict::DecodeRejected(reason) => {
                    warn!(product, config_id = %id, reason, "Rejected a configuration.");
                    metrics.count_rejected(product, RejectionStage::Decode, 1);
                }
                Verdict::BuildRejected(_) => metrics.count_rejected(product, RejectionStage::Build, 1),
                Verdict::Collided(_) => metrics.count_rejected(product, RejectionStage::Collision, 1),
            }
            verdicts.insert(id, verdict);
        }

        self.decoded.insert(
            product.to_owned(),
            Decoded {
                generation,
                inputs,
                verdicts,
            },
        );
    }
}

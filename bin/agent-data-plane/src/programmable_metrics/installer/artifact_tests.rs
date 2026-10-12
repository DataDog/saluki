//! Artifacts from the channel, in a running plan-driven topology on the connected configuration path.
//!
//! The installer is connected to an artifact channel whose transport is the test. The core's artifact paths are
//! replaced by a double ([`ScriptedArtifacts`]) that stands in for snapshot handling and composition, following the
//! core's documented rules closely enough to drive the worker through what the core decides:
//!
//! - For a snapshot of one config, it reads the envelope and proposes the plan the test scripted for the envelope's
//!   artifact hash, as an artifact of the envelope's revision, unless that artifact is the one in effect. An artifact
//!   whose plan is the one already running is in effect without a generation of its own, as the core attributes it.
//! - For an empty snapshot, it asks for the default program, as a revocation does.
//! - For a configuration change while an artifact is in effect, it composes the artifact again, by making the
//!   artifact's edit to the new default program. A composition equal to the active plan is unchanged; any other is
//!   refused, as the core refuses an artifact it cannot compose with the changed default program (one emitting to an
//!   output the change removed): the double has no way to install a new composition under the same revision, so it
//!   stands in for a re-composition the core rejects, with the core's report and reason.
//! - It reports the artifact the active generation serves as the active artifact, as the core's status summary does,
//!   and, as the core does, adds to the summary's `degraded` why that artifact was not composed again.
//!
//! Everything else is the real installer and core: preparation in the regions, the commit, rollback on a generation
//! failure, the revision high-water mark, and what the worker reports.
//!
//! Scripted DogStatsD batches flow through the topology while artifacts install, as in the live configuration tests:
//! timestamped counters, which the no-aggregation pipeline sends past aggregation, so each reaches the main output with
//! the header it was admitted with: the generation that processed it. Every test checks, once the topology has
//! stopped, that every batch arrived exactly once, and whole: each of its kept metrics once, all processed by the same
//! generation.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use metrics::Key;
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::{
    accounting::ComponentRegistry,
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{Tag, TagSet},
    },
    execution::GenerationId,
};
use saluki_metric_plan::{
    canonical::ResolvedValues,
    channel::{ActiveArtifact, ArtifactConfig, ArtifactConfigId, ArtifactProduct, Envelope},
    install::Rejection,
};
use saluki_metrics::test::TestRecorder;
use serde_json::json;

use super::*;
use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        catalog::POST_ENRICH_SCOPE,
        telemetry::PLAN_ACTIVE_REVISION,
        test_support::{
            configuration, connected_configuration_system, facts, programmable_metrics, update, ConfigCell,
        },
        test_topology::{Aggregation, Delivery, MetricsPath, Options, TestTopology, PATIENCE},
    },
};

/// The metrics every batch carries that no artifact in these tests drops.
const KEPT: [&str; 2] = ["kept.a", "kept.b"];

/// The config ID the tests deliver every artifact in.
const CONFIG_ID: &str = "demo";

/// One artifact the test scripted.
struct Scripted {
    /// The metric name the artifact drops, or `None` for an unmodified export, which composes to the default program
    /// itself.
    drops: Option<String>,

    /// The artifact composed into the default program of the base configuration, and its validation report.
    plan: Arc<Plan>,
    report: ValidationReport,
}

impl Scripted {
    /// Composes the artifact, `revision` and `hash`, into `default`.
    fn compose(&self, default: &Plan, revision: u64, hash: PlanHash) -> Plan {
        match &self.drops {
            Some(name) => dropping(default, name, revision, hash),
            None => default.clone(),
        }
    }
}

/// What the double knows: the artifacts the test scripted, what is in effect, and what the worker handed it.
#[derive(Default)]
struct Script {
    /// The artifacts, by artifact hash.
    artifacts: BTreeMap<PlanHash, Scripted>,
    /// The catalog compositions are validated with.
    catalog: Option<AdpCatalog>,
    /// Whether the last snapshot revoked.
    revoked: bool,
    /// The artifact of the last snapshot, if its plan was the one already running, so that no generation was created
    /// for it: its revision and hash, and the plan's hash.
    attributed: Option<(u64, PlanHash, PlanHash)>,
    /// The revision of every snapshot's config the worker handed the double, in order, or `None` for a snapshot
    /// without exactly one config.
    handed: Vec<Option<u64>>,
    /// Whether to panic while handling the next snapshot, as a bug in the core would.
    panic_on_next_snapshot: bool,
    /// The default programs the worker handed the double, in order.
    default_programs: Vec<Arc<Plan>>,
    /// Why the artifact in effect, by revision and hash, was not installed again after the last configuration change,
    /// if it was not.
    policy_error: Option<((u64, PlanHash), String)>,
}

impl Script {
    /// Returns the revision and hash of the artifact in effect: the one the active generation serves, unless the
    /// generation failed under supervision or the channel revoked the artifact.
    fn in_effect(&self, core: &InstallerCore) -> Option<(u64, PlanHash)> {
        if self.revoked {
            return None;
        }
        let active = core.status().active?;
        match active.origin {
            Origin::Artifact { revision, hash } if active.state != GenerationState::Failed => Some((revision, hash)),
            Origin::Artifact { .. } => None,
            Origin::DefaultProgram { .. } => self
                .attributed
                .filter(|(_, _, plan_hash)| *plan_hash == active.plan_hash)
                .map(|(revision, hash, _)| (revision, hash)),
        }
    }

    /// Proposes `plan`, with `report`, as the artifact `hash` of `revision`, as the core proposes a composed artifact.
    fn propose(
        core: &mut InstallerCore, plan: Arc<Plan>, report: ValidationReport, revision: u64, hash: PlanHash,
    ) -> (CandidateId, Vec<Action>) {
        core.propose(Candidate {
            plan,
            report,
            origin: Origin::Artifact { revision, hash },
        })
    }
}

/// A double of the core's artifact paths, as the [module](self) describes.
#[derive(Clone, Default)]
struct ScriptedArtifacts {
    script: Arc<StdMutex<Script>>,
}

impl ArtifactPaths for ScriptedArtifacts {
    fn on_snapshot(
        &mut self, core: &mut InstallerCore, snapshot: &ArtifactSnapshot, _composer: &dyn Composer,
    ) -> Vec<Action> {
        let mut script = self.script.lock().unwrap();
        let envelope = match snapshot.configs.as_slice() {
            [config] => Some(Envelope::parse(&config.payload).expect("the test delivers envelopes")),
            _ => None,
        };
        script.handed.push(envelope.as_ref().map(|envelope| envelope.revision));
        if std::mem::take(&mut script.panic_on_next_snapshot) {
            drop(script);
            panic!("the double panics while handling a snapshot, as the test asked");
        }
        let Some(envelope) = envelope else {
            if snapshot.configs.is_empty() {
                script.revoked = true;
                script.attributed = None;
                return vec![Action::InstallDefaultProgram];
            }
            return Vec::new();
        };
        // Idempotency, as the core's rule says: the artifact in effect acknowledges the current state.
        if script.in_effect(core).map(|(_, hash)| hash) == Some(envelope.artifact_hash) {
            return Vec::new();
        }
        script.revoked = false;
        script.attributed = None;
        let scripted = script
            .artifacts
            .get(&envelope.artifact_hash)
            .expect("the test scripted every artifact it delivers");
        let (plan, report) = (Arc::clone(&scripted.plan), scripted.report.clone());
        let plan_hash = plan.hash(ResolvedValues::Include).expect("scripted plans hash");
        let (candidate, actions) = Script::propose(core, plan, report, envelope.revision, envelope.artifact_hash);
        // An artifact whose plan already runs is served by the generation that runs it.
        let unchanged = actions.iter().any(|action| match action {
            Action::Report(Report {
                candidate: reported,
                outcome: Outcome::Unchanged,
                ..
            }) => *reported == candidate,
            _ => false,
        });
        if unchanged {
            script.attributed = Some((envelope.revision, envelope.artifact_hash, plan_hash));
        }
        actions
    }

    fn on_default_program(
        &mut self, core: &mut InstallerCore, candidate: Candidate, _composer: &dyn Composer,
    ) -> (CandidateId, Vec<Action>) {
        let mut script = self.script.lock().unwrap();
        script.default_programs.push(Arc::clone(&candidate.plan));
        let Some((revision, hash)) = script.in_effect(core) else {
            return core.propose(candidate);
        };
        // The artifact in effect, composed again against the new default program.
        let plan = script.artifacts[&hash].compose(&candidate.plan, revision, hash);
        let catalog = script.catalog.as_ref().expect("the harness sets the catalog");
        let report = Validator::new(&OperatorRegistry::native(), catalog).validate(&plan);
        let running = core
            .active_plan()
            .map(|active| active.hash(ResolvedValues::Include).ok());
        if running == Some(plan.hash(ResolvedValues::Include).ok()) {
            // Unchanged, as the core reports a composition equal to the active plan (under the default program's
            // origin here, since the double cannot report under the artifact's).
            script.policy_error = None;
            return core.propose(Candidate {
                plan: Arc::new(plan),
                report,
                origin: candidate.origin,
            });
        }
        // Refused as the core refuses a re-composition it cannot compose. A proposal under the artifact's own revision
        // changes nothing in the core, which rejects it as stale, and gives the double a candidate to report the
        // refusal under, in place of the stale rejection.
        let message = recomposition_refused(revision, &removed_output());
        let (candidate, actions) = Script::propose(core, Arc::new(plan), report, revision, hash);
        let actions = actions
            .into_iter()
            .map(|action| match action {
                Action::Report(Report {
                    candidate: reported,
                    origin,
                    generation,
                    outcome: Outcome::Rejected(_),
                }) if reported == candidate => Action::Report(Report {
                    candidate,
                    origin,
                    generation,
                    outcome: Outcome::Rejected(Rejection::Refused {
                        message: message.clone(),
                    }),
                }),
                other => other,
            })
            .collect();
        script.policy_error = Some(((revision, hash), message));
        (candidate, actions)
    }

    fn apply_states(
        &self, core: &InstallerCore, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates {
        // The script installs `post_enrich` artifacts only, so it reports the first version's form, with that scope's
        // base alone.
        let default_program_hash = bases
            .iter()
            .find(|(scope, _)| scope.as_str() == POST_ENRICH_SCOPE)
            .map(|(_, hash)| *hash);
        let script = self.script.lock().unwrap();
        let in_effect = script.in_effect(core);
        // The core reports why the artifact it serves was not composed again, after the host's own reason.
        let policy_error = script
            .policy_error
            .as_ref()
            .filter(|(artifact, _)| in_effect == Some(*artifact))
            .map(|(_, message)| message.clone());
        let degraded = match (degraded, policy_error) {
            (Some(host), Some(policy)) => Some(format!("{}; {}", host, policy)),
            (host, policy) => host.or(policy),
        };
        let mut states = core.apply_states(default_program_hash, degraded);
        states.status.active_artifact = in_effect.map(|(revision, artifact_hash)| ActiveArtifact {
            id: ArtifactConfigId::new(CONFIG_ID),
            revision,
            artifact_hash,
        });
        states
    }
}

/// Returns why the artifact of `revision` was not installed again after a configuration change, as the core reports
/// it in the status summary.
/// The composition error the double stands in for: the configuration change removed an output the artifact emits to.
fn removed_output() -> ComposeError {
    ComposeError::PendingOutput {
        output: OutputId::new("af_metrics_encode").unwrap(),
    }
}

/// Returns why the core refuses a re-composition of artifact revision `revision` that fails with `error`, as it
/// reports the refusal and the host degraded.
fn recomposition_refused(revision: u64, error: &ComposeError) -> String {
    format!(
        "artifact revision {} cannot be composed with the changed default program, and stays as it was composed \
         before: {}",
        revision, error
    )
}

/// Returns batch `index`: a timestamped counter of every name in `names`, each tagged with the batch.
fn batch(index: usize, names: &[&str]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| {
            let tags = TagSet::from_iter([Tag::from(format!("batch:{}", index))]);
            Metric::counter(
                Context::from_parts(name.to_string(), tags),
                (1_700_000_000 + index as u64, 1.0),
            )
        })
        .collect()
}

/// For each metric name and batch, the generation of every buffer the main output received it in.
type Received = BTreeMap<(String, usize), Vec<Option<u64>>>;

/// Returns, for each metric name and batch, the generation of every buffer the main output received it in.
fn received(deliveries: &[Delivery]) -> Received {
    let mut received = BTreeMap::<_, Vec<_>>::new();
    for delivery in deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode")
    {
        for metric in &delivery.metrics {
            let batch = metric
                .context()
                .tags()
                .into_iter()
                .find_map(|tag| tag.as_str().strip_prefix("batch:").and_then(|batch| batch.parse().ok()))
                .expect("every metric belongs to a batch");
            received
                .entry((metric.context().name().to_string(), batch))
                .or_default()
                .push(delivery.generation);
        }
    }
    received
}

/// Returns whether the metric `name` of batch `index` arrived.
fn arrived(received: &Received, name: &str, index: usize) -> bool {
    received.contains_key(&(name.to_string(), index))
}

/// Returns a copy of `default` with an artifact node in front of the main output that drops the metrics named `name`,
/// both of its nodes with the provenance of artifact `revision`, `hash`.
fn dropping(default: &Plan, name: &str, revision: u64, hash: PlanHash) -> Plan {
    let provenance = json!({"artifact": {"revision": revision, "hash": hash}});
    let mut value = serde_json::to_value(default).unwrap();
    value["provenance"] = json!({"artifacts": [{"revision": revision, "hash": hash}]});
    let edges = value["edges"].as_array_mut().unwrap();
    let main = edges
        .iter_mut()
        .find(|edge| edge["to"] == "metrics_policy_0.in")
        .expect("the main output is emitted by the policy tee");
    main["to"] = json!("policy/drop.in");
    edges.push(json!({"from": "policy/drop.true", "to": "policy/drop_true.in"}));
    edges.push(json!({"from": "policy/drop.false", "to": "metrics_policy_0.in"}));
    let nodes = value["nodes"].as_array_mut().unwrap();
    nodes.push(json!({
        "id": "policy/drop", "op": "split",
        "params": {"predicate": {"intake_name": {"matches": {"prefix": false, "values": [name]}}}},
        "provenance": provenance
    }));
    nodes.push(json!({"id": "policy/drop_true", "op": "drop", "provenance": provenance}));
    Plan::from_value(value).expect("the artifact's plan is a plan")
}

/// Returns the config delivering artifact `hash` of `revision`.
fn artifact_config(revision: u64, hash: PlanHash) -> ArtifactConfig {
    let payload = json!({
        "revision": revision, "artifact_hash": hash,
        "artifact": {"scope": "post_enrich", "scripted": hash}
    })
    .to_string();
    ArtifactConfig {
        id: ArtifactConfigId::new(CONFIG_ID),
        version: revision,
        sha256: ArtifactConfig::transport_hash(&payload),
        payload,
    }
}

/// What a stopped harness received.
struct Finished {
    /// What the main output received.
    received: Received,
    /// The generation that processed each batch, by batch index.
    processed: Vec<u64>,
}

/// A running plan-driven topology with an artifact channel, its transport, and the double of the core's artifact
/// paths.
/// The core's artifact paths a [`Harness`] runs with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Paths {
    /// The double, [`ScriptedArtifacts`].
    Double,
    /// The core's own: real envelopes, receipt, composition, and acknowledgments.
    Core,
}

struct Harness {
    topology: TestTopology,
    status: Arc<ArcSwap<PlanStatus>>,
    transport: Option<TransportEnd>,
    script: Arc<StdMutex<Script>>,
    failures: GenerationFailureReporter,
    stream: mpsc::Sender<ConfigUpdate>,
    /// The default program of the base configuration, and its catalog.
    default_program: (Plan, AdpCatalog),
    /// The batches sent so far.
    sent: usize,
    /// The generation that processed each batch sent and waited for, as it arrived, by batch index.
    seen: BTreeMap<usize, Option<u64>>,
}

impl Harness {
    /// Starts the topology with `options`, connected to an artifact channel and to the double.
    async fn start(options: Options) -> Self {
        Self::start_with(options, Paths::Double).await
    }

    /// Starts the topology with `options`, connected to an artifact channel, and to the core's artifact `paths`.
    async fn start_with(mut options: Options, paths: Paths) -> Self {
        let (system, updates, stream) = connected_configuration_system("").await;
        let config = SalukiConfiguration::clone(&system.config());
        let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
            .expect("the configuration lowers")
            .expect("a metrics pipeline is required")
            .lower(&config)
            .expect("the configuration lowers");

        let artifacts = ScriptedArtifacts::default();
        let script = Arc::clone(&artifacts.script);
        script.lock().unwrap().catalog = Some(lowered.catalog.clone());
        let transport = Arc::new(StdMutex::new(None));
        let failures = Arc::new(StdMutex::new(None));
        options.before_topology = Some(Box::new({
            let failures = Arc::clone(&failures);
            move |boot: &mut BootTopology| *failures.lock().unwrap() = Some(Arc::clone(&boot.failures))
        }));
        options.before_install = Some(Box::new({
            let transport = Arc::clone(&transport);
            move |metrics: &mut ProgrammableMetrics| {
                *transport.lock().unwrap() = Some(metrics.connect_artifact_channel());
                if paths == Paths::Double {
                    metrics.with_artifact_paths(Box::new(artifacts));
                }
            }
        }));
        let topology = TestTopology::start(&system, options, |supervisor| {
            supervisor.add_worker(ConfigUpdatesWorker::new(updates))
        })
        .await;
        let status = topology.status().expect("the plan-driven path has an installer");
        let transport = transport.lock().unwrap().take();
        let failures = failures.lock().unwrap().take().expect("the boot topology was prepared");
        Self {
            topology,
            status,
            transport,
            script,
            failures,
            stream,
            default_program: (lowered.plan, lowered.catalog),
            sent: 0,
            seen: BTreeMap::new(),
        }
    }

    /// Scripts artifact `revision`, which drops the metrics named `name`, and returns the config delivering it.
    fn artifact(&self, revision: u64, name: &str) -> ArtifactConfig {
        let hash = PlanHash::of_bytes(format!("artifact {} dropping {}", revision, name).as_bytes());
        self.script(revision, hash, Some(name))
    }

    /// Scripts artifact `revision`, an unmodified export, and returns the config delivering it.
    fn unmodified(&self, revision: u64) -> ArtifactConfig {
        let hash = PlanHash::of_bytes(format!("artifact {} unmodified", revision).as_bytes());
        self.script(revision, hash, None)
    }

    fn script(&self, revision: u64, hash: PlanHash, drops: Option<&str>) -> ArtifactConfig {
        let (default, catalog) = &self.default_program;
        let drops = drops.map(str::to_string);
        let plan = match &drops {
            Some(name) => dropping(default, name, revision, hash),
            None => default.clone(),
        };
        let report = Validator::new(&OperatorRegistry::native(), catalog).validate(&plan);
        assert!(report.is_valid(), "{:?}", report.errors().collect::<Vec<_>>());
        let scripted = Scripted {
            drops,
            plan: Arc::new(plan),
            report,
        };
        self.script.lock().unwrap().artifacts.insert(hash, scripted);
        artifact_config(revision, hash)
    }

    /// Delivers a snapshot of `configs`.
    fn deliver(&self, configs: Vec<ArtifactConfig>) {
        let transport = self.transport.as_ref().expect("the channel is connected");
        assert!(transport
            .publisher
            .publish(ArtifactSnapshot::new(ArtifactProduct::Policy, configs)));
    }

    /// Reports that a region failed `generation`.
    fn fail(&self, generation: u64, reason: &str) {
        (self.failures)(GenerationId::new(generation), reason);
    }

    /// Returns the revision of every snapshot's config the worker handed the double so far.
    fn handed(&self) -> Vec<Option<u64>> {
        self.script.lock().unwrap().handed.clone()
    }

    /// Sends the next batch of `names`, waits until the main output has received its first kept metric, and returns
    /// the generation that processed it.
    async fn send(&mut self, names: &[&str]) -> Option<u64> {
        let index = self.sent;
        self.topology.feed("dsd_in.metrics", batch(index, names));
        self.sent += 1;
        let deliveries = self
            .topology
            .wait_for(&format!("batch {}", index), |deliveries| {
                received(deliveries).contains_key(&(KEPT[0].to_string(), index))
            })
            .await;
        let generation = received(deliveries)[&(KEPT[0].to_string(), index)][0];
        self.seen.insert(index, generation);
        generation
    }

    /// Sends batches until one is processed by `generation`, and returns the index of that batch.
    async fn send_until(&mut self, names: &[&str], generation: u64) -> usize {
        loop {
            if self.send(names).await == Some(generation) {
                return self.sent - 1;
            }
            assert!(self.sent < 10_000, "generation {} never processed a batch", generation);
        }
    }

    /// Sends batches of `names`, 32 every millisecond, without waiting for any of them, until the status satisfies
    /// `done`, and then twenty more at once.
    async fn feed_until(&mut self, names: &[&str], done: impl Fn(&PlanStatus) -> bool) {
        let started = tokio::time::Instant::now();
        while !done(&self.status.load()) {
            assert!(
                started.elapsed() < PATIENCE,
                "the installer's status never changed as expected: {}",
                self.status.load()
            );
            for _ in 0..32 {
                self.feed(names);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for _ in 0..20 {
            self.feed(names);
        }
    }

    /// Sends the next batch of `names` without waiting for it.
    fn feed(&mut self, names: &[&str]) {
        self.topology.feed("dsd_in.metrics", batch(self.sent, names));
        self.sent += 1;
    }

    /// Waits until the status satisfies `done`, returning it.
    async fn status_where(&self, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
        tokio::time::timeout(PATIENCE, async {
            loop {
                let current = self.status.load_full();
                if done(&current) {
                    return PlanStatus::clone(&current);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the installer's status never changed as expected: {}",
                self.status.load()
            )
        })
    }

    /// Stops the topology, and checks that every batch sent arrived exactly once and whole: each of its kept metrics
    /// once, all processed by the same generation, the one it was seen processed by if the test waited for it.
    async fn finish(self) -> Finished {
        let received = received(&self.topology.stop().await);
        let processed = (0..self.sent)
            .map(|index| {
                let generations = KEPT
                    .iter()
                    .map(|name| {
                        let arrivals = received.get(&(name.to_string(), index)).cloned().unwrap_or_default();
                        match arrivals.as_slice() {
                            [Some(generation)] => *generation,
                            _ => panic!(
                                "batch {}: `{}` arrived as {:?}, not exactly once",
                                index, name, arrivals
                            ),
                        }
                    })
                    .collect::<Vec<_>>();
                assert!(
                    generations.iter().all(|generation| *generation == generations[0]),
                    "batch {} was processed by several generations: {:?}",
                    index,
                    generations
                );
                if let Some(seen) = self.seen.get(&index) {
                    assert_eq!(Some(generations[0]), *seen, "batch {}", index);
                }
                generations[0]
            })
            .collect();
        Finished { received, processed }
    }
}

fn active(status: &PlanStatus) -> Option<u64> {
    status.installer.active.as_ref().map(|active| active.id.get())
}

fn active_origin(status: &PlanStatus) -> Option<Origin> {
    status.installer.active.as_ref().map(|active| active.origin)
}

fn live(status: &PlanStatus) -> Vec<u64> {
    status
        .installer
        .live
        .iter()
        .map(|generation| generation.get())
        .collect()
}

fn record(status: &PlanStatus, generation: u64) -> Option<(GenerationState, Option<String>)> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .map(|record| (record.state, record.reason.clone()))
}

/// Returns the revision of the artifact in effect, as the status summary reports it.
fn active_revision(status: &PlanStatus) -> Option<u64> {
    status
        .apply_states
        .status
        .active_artifact
        .as_ref()
        .map(|artifact| artifact.revision)
}

#[tokio::test]
async fn an_artifact_installs_and_its_revocation_restores_the_default_program_at_batch_boundaries_without_loss() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    let names = [KEPT[0], KEPT[1], "dropped"];
    for _ in 0..5 {
        assert_eq!(harness.send(&names).await, Some(1));
    }

    // The artifact installs while batches flow: it is prepared in every region and committed, and the default program's
    // generation is kept, prepared, to roll back to.
    let artifact = harness.artifact(1, "dropped");
    harness.deliver(vec![artifact.clone()]);
    let first_artifact_batch = harness.send_until(&names, 2).await;
    for _ in 0..5 {
        assert_eq!(harness.send(&names).await, Some(2));
    }
    let installed = harness.status_where(|status| active(status) == Some(2)).await;
    let hash = PlanHash::of_bytes(b"artifact 1 dropping dropped");
    assert_eq!(active_origin(&installed), Some(Origin::Artifact { revision: 1, hash }));
    assert_eq!(installed.installer.revision_high_water, Some(1));
    assert_eq!(
        installed.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
        Some(1)
    );
    assert_eq!(
        record(&installed, 1).map(|(state, _)| state),
        Some(GenerationState::Prepared)
    );
    assert_eq!(live(&installed), [1, 2]);
    assert_eq!(
        (
            installed.last_error.as_ref(),
            installed.apply_states.status.degraded.as_ref()
        ),
        (None, None)
    );
    assert_eq!(active_revision(&installed), Some(1));
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(1.0));

    // The status serves the artifact's plan, and describes it.
    let served = installed.active.as_ref().expect("a generation is active");
    assert_eq!(served.generation, GenerationId::new(2));
    assert_eq!(*served.plan, *harness.script.lock().unwrap().artifacts[&hash].plan);
    assert_eq!(
        served.explain.plan.hash,
        Some(installed.installer.active.as_ref().unwrap().plan_hash)
    );
    assert!(served
        .explain
        .graph
        .nodes
        .iter()
        .any(|node| node.id.to_string() == "policy/drop"));

    // What the installer reports through the channel is its status summary: generation 2 is active.
    let transport = harness.transport.as_ref().unwrap();
    let reported = transport.apply_states.latest();
    assert_eq!(reported.status.active_generation, Some(2));
    assert_eq!(reported.status.revision_high_water, Some(1));
    assert_eq!(*reported, installed.apply_states);

    // An empty snapshot revokes: the default program is installed again.
    harness.deliver(Vec::new());
    let first_revoked_batch = harness.send_until(&names, 3).await;
    for _ in 0..5 {
        assert_eq!(harness.send(&names).await, Some(3));
    }
    let revoked = harness.status_where(|status| live(status) == [3]).await;
    assert!(matches!(active_origin(&revoked), Some(Origin::DefaultProgram { .. })));
    assert_eq!(
        record(&revoked, 2).map(|(state, _)| state),
        Some(GenerationState::Retired)
    );
    assert_eq!(
        record(&revoked, 1).map(|(state, _)| state),
        Some(GenerationState::Retired)
    );
    // The default program's plan is the one the configuration lowers to.
    assert_eq!(*revoked.active.as_ref().unwrap().plan, harness.default_program.0);
    assert_eq!(active_revision(&revoked), None);
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(0.0));
    assert_eq!(harness.handed(), [Some(1), None]);
    let finished = harness.finish().await;

    // Each batch was processed whole by one generation: generation 1 up to the artifact's commit, the artifact's
    // generation 2 until its revocation, and generation 3 after it. The artifact dropped exactly the batches it
    // processed.
    for (index, generation) in finished.processed.iter().enumerate() {
        let expected = if index < first_artifact_batch {
            1
        } else if index < first_revoked_batch {
            2
        } else {
            3
        };
        assert_eq!(*generation, expected, "batch {}", index);
        let dropped = finished
            .received
            .get(&("dropped".to_string(), index))
            .cloned()
            .unwrap_or_default();
        if expected == 2 {
            assert_eq!(dropped, [], "batch {}: the artifact let `dropped` through", index);
        } else {
            assert_eq!(dropped, [Some(expected)], "batch {}: `dropped`", index);
        }
    }
}

#[tokio::test]
async fn a_failed_artifact_rolls_back_to_last_known_good_and_a_stale_revision_is_then_rejected() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    let names = [KEPT[0], KEPT[1], "dropped"];
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    let installed = harness.status_where(|status| active(status) == Some(2)).await;
    assert_eq!(installed.installer.revision_high_water, Some(1));
    assert_eq!(harness.send(&names).await, Some(2));

    // A region fails the artifact's generation: the installer rolls back to the generation kept as last-known-good,
    // with one schedule store, and the next batch runs the default program again.
    harness.fail(
        2,
        "it exceeded its panic budget, with 4 panics, the last in node `policy/drop`",
    );
    let rolled_back = harness.status_where(|status| active(status) == Some(1)).await;
    assert_eq!(
        record(&rolled_back, 2),
        Some((
            GenerationState::Failed,
            Some("it exceeded its panic budget, with 4 panics, the last in node `policy/drop`".to_string())
        ))
    );
    assert!(matches!(
        active_origin(&rolled_back),
        Some(Origin::DefaultProgram { .. })
    ));
    assert_eq!(active_revision(&rolled_back), None);
    // A failure does not change the revision high-water mark.
    assert_eq!(rolled_back.installer.revision_high_water, Some(1));
    assert_eq!(harness.send(&names).await, Some(1));

    // A later proposal must still exceed the failed revision: another artifact under revision 1 is stale. The active
    // generation keeps running, and a rejected artifact is not a configuration change that failed to install.
    harness.deliver(vec![harness.artifact(1, "kept.b")]);
    tokio::time::timeout(PATIENCE, async {
        while harness.handed().len() < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the worker handed the double the stale snapshot");
    assert_eq!(harness.send(&names).await, Some(1));
    let after_stale = harness.status.load_full();
    assert_eq!(active(&after_stale), Some(1));
    assert!(record(&after_stale, 3).is_none(), "{}", after_stale);
    assert_eq!(after_stale.last_error, None);
    assert_eq!(after_stale.apply_states.status.degraded, None);
    assert_eq!(after_stale.installer.revision_high_water, Some(1));

    // Revision 2 installs.
    harness.deliver(vec![harness.artifact(2, "dropped")]);
    harness.status_where(|status| active(status) == Some(3)).await;
    harness.send_until(&names, 3).await;
    let finished = harness.finish().await;

    // Every batch arrived exactly once: the artifacts' generations dropped `dropped`, the rolled-back default program
    // did not, and the stale artifact never ran.
    for (index, generation) in finished.processed.iter().enumerate() {
        assert_eq!(
            arrived(&finished.received, "dropped", index),
            *generation == 1,
            "batch {}, generation {}",
            index,
            generation
        );
    }
    assert_eq!(finished.processed[..3], [2, 1, 1]);
}

#[tokio::test]
async fn failures_fall_back_from_artifact_to_last_known_good_artifact_to_default_program() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    let names = [KEPT[0], KEPT[1], "first", "second"];
    harness.deliver(vec![harness.artifact(1, "first")]);
    harness.status_where(|status| active(status) == Some(2)).await;
    // The second artifact replaces the first, which is kept to roll back to; the default program's generation goes.
    harness.deliver(vec![harness.artifact(2, "second")]);
    let second = harness
        .status_where(|status| active(status) == Some(3) && live(status) == [2, 3])
        .await;
    assert_eq!(
        second.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
        Some(2)
    );
    assert_eq!(harness.send(&names).await, Some(3));

    // The second artifact fails: the first is active again.
    harness.fail(3, "it exceeded its panic budget");
    let first = harness.status_where(|status| active(status) == Some(2)).await;
    assert_eq!(active_revision(&first), Some(1));
    assert_eq!(harness.send(&names).await, Some(2));

    // The first fails too, with nothing kept: the default program is lowered and installed.
    harness.fail(2, "it exceeded its panic budget");
    let fallen_back = harness.status_where(|status| active(status) == Some(4)).await;
    assert!(matches!(
        active_origin(&fallen_back),
        Some(Origin::DefaultProgram { .. })
    ));
    assert_eq!(*fallen_back.active.as_ref().unwrap().plan, harness.default_program.0);
    assert_eq!(fallen_back.installer.revision_high_water, Some(2));
    assert_eq!(active_revision(&fallen_back), None);
    harness.send_until(&names, 4).await;
    let finished = harness.finish().await;

    // Every batch arrived exactly once, and each generation dropped what its plan says.
    assert_eq!(finished.processed[..2], [3, 2]);
    for (index, generation) in finished.processed.iter().enumerate() {
        let (first, second) = match generation {
            3 => (true, false),
            2 => (false, true),
            _ => (true, true),
        };
        assert_eq!(
            (
                arrived(&finished.received, "first", index),
                arrived(&finished.received, "second", index)
            ),
            (first, second),
            "batch {}, generation {}",
            index,
            generation
        );
    }
}

#[tokio::test]
async fn continuous_input_across_installs_a_rollback_and_a_revocation_arrives_exactly_once_in_whole_batches() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    let names = [KEPT[0], KEPT[1], "dropped"];

    // Batches are sent one after another without waiting for any of them, so that every commit, including the
    // rollback's, happens with batches in flight.
    harness.feed_until(&names, |_| true).await;
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    harness.feed_until(&names, |status| active(status) == Some(2)).await;
    // The batches in flight reach the source's admission point in their own time: the artifact's generation fails
    // only once it processed one, so that the rollback replaces a generation that ran.
    harness.send_until(&names, 2).await;
    harness.fail(2, "it exceeded its panic budget");
    harness.feed_until(&names, |status| active(status) == Some(1)).await;
    harness.deliver(vec![harness.artifact(2, "dropped")]);
    harness.feed_until(&names, |status| active(status) == Some(3)).await;
    harness.deliver(Vec::new());
    harness.feed_until(&names, |status| active(status) == Some(4)).await;
    let finished = harness.finish().await;

    // Every batch arrived exactly once and whole, and the generations processed the batches in the order the schedule
    // held them: the default program, the artifact, the default program rolled back to, the second artifact, and the
    // default program after the revocation.
    let mut order = finished.processed.clone();
    order.dedup();
    assert_eq!(order, [1, 2, 1, 3, 4]);
    for (index, generation) in finished.processed.iter().enumerate() {
        let dropping = matches!(generation, 2 | 3);
        assert_eq!(
            arrived(&finished.received, "dropped", index),
            !dropping,
            "batch {}, generation {}",
            index,
            generation
        );
    }
}

#[tokio::test]
async fn a_configuration_change_the_artifact_in_effect_cannot_follow_is_reported_degraded_while_the_artifact_runs() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    harness.status_where(|status| active(status) == Some(2)).await;

    // A configuration change reaches the core's artifact paths with the default program lowered from it.
    harness
        .stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    let degraded = harness
        .status_where(|status| status.config_changes == 1 && status.last_error.is_some())
        .await;
    {
        let script = harness.script.lock().unwrap();
        assert_eq!(script.default_programs.len(), 1);
        assert_ne!(*script.default_programs[0], harness.default_program.0);
        assert!(serde_json::to_string(&*script.default_programs[0])
            .unwrap()
            .contains("\"blocked\""));
    }

    // The core refused the artifact composed again, so the artifact keeps running, and the installer reports itself
    // degraded until a new artifact or configuration resolves it.
    let reason = format!(
        "artifact revision 1 cannot be composed with the changed default program, and stays as it was composed \
         before: {}",
        removed_output()
    );
    let reason = reason.as_str();
    assert_eq!(degraded.last_error.as_deref(), Some(reason));
    assert_eq!(active(&degraded), Some(2));
    assert_eq!(active_revision(&degraded), Some(1));
    // The summary says it once, as the core reports it: the installer does not repeat the core's reason.
    assert_eq!(degraded.apply_states.status.degraded.as_deref(), Some(reason));
    let transport = harness.transport.as_ref().unwrap();
    assert_eq!(transport.apply_states.latest().status.degraded.as_deref(), Some(reason));
    assert_eq!(harness.send(&[KEPT[0], KEPT[1], "dropped"]).await, Some(2));

    // A revocation installs the default program of the current configuration, which resolves it.
    harness.deliver(Vec::new());
    let resolved = harness.status_where(|status| active(status) == Some(3)).await;
    assert_eq!(
        (resolved.last_error, resolved.apply_states.status.degraded),
        (None, None)
    );
    let batch = harness.send_until(&[KEPT[0], KEPT[1], "blocked"], 3).await;
    let finished = harness.finish().await;
    assert!(!arrived(&finished.received, "blocked", batch));
}

#[tokio::test]
async fn reverting_a_configuration_change_the_artifact_in_effect_could_not_follow_resolves_the_degraded_report() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    harness.status_where(|status| active(status) == Some(2)).await;

    // The artifact cannot follow the change: degraded.
    harness
        .stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    let degraded = harness
        .status_where(|status| status.config_changes == 1 && status.last_error.is_some())
        .await;
    assert!(degraded.apply_states.status.degraded.is_some());

    // The change is reverted: the artifact composed again is the plan that runs, so the configuration in effect is
    // installed, and the installer is no longer degraded, without a new generation.
    harness
        .stream
        .send(update("metric_filterlist", json!([])))
        .await
        .unwrap();
    let resolved = harness.status_where(|status| status.config_changes == 2).await;
    assert_eq!(
        (
            resolved.last_error.as_ref(),
            resolved.apply_states.status.degraded.as_ref()
        ),
        (None, None)
    );
    assert_eq!(
        harness
            .transport
            .as_ref()
            .unwrap()
            .apply_states
            .latest()
            .status
            .degraded,
        None
    );
    assert_eq!((active(&resolved), active_revision(&resolved)), (Some(2), Some(1)));
    assert!(record(&resolved, 3).is_none(), "{}", resolved);
    assert_eq!(harness.send(&[KEPT[0], KEPT[1], "dropped"]).await, Some(2));
    let finished = harness.finish().await;
    assert!(!arrived(&finished.received, "dropped", 0));
}

#[tokio::test]
async fn an_artifact_that_composes_to_the_running_plan_is_in_effect_without_a_generation_of_its_own() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;

    // An unmodified export composes to the default program that runs: no generation is created for it, and the default
    // program's generation serves it. What the status summary says is in effect, the gauge says too.
    harness.deliver(vec![harness.unmodified(1)]);
    let attributed = harness
        .status_where(|status| status.installer.revision_high_water == Some(1))
        .await;
    assert_eq!(active(&attributed), Some(1));
    assert!(matches!(
        active_origin(&attributed),
        Some(Origin::DefaultProgram { .. })
    ));
    assert_eq!(active_revision(&attributed), Some(1));
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(1.0));
    assert_eq!(harness.send(&[KEPT[0], KEPT[1]]).await, Some(1));

    // A revocation: the default program is in effect again.
    harness.deliver(Vec::new());
    let revoked = harness.status_where(|status| active_revision(status).is_none()).await;
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(0.0));
    assert!(record(&revoked, 2).is_none(), "{}", revoked);
    harness.finish().await;
}

/// Runs the installer worker as a supervisor would, until `shutdown` triggers or the task is aborted.
fn run_installer(installer: &Arc<InstallerWorker>, shutdown: ShutdownHandle) -> impl std::future::Future<Output = ()> {
    let installer = Arc::clone(installer);
    async move {
        let run = match installer.initialize(shutdown).await {
            Ok(run) => run,
            Err(_) => panic!("the installer initializes"),
        };
        run.await.expect("the installer stops cleanly");
    }
}

#[tokio::test]
async fn the_channel_reads_the_status_before_the_installer_runs_and_a_restarted_worker_handles_an_unfinished_snapshot()
{
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.supervise_installer = false;
    let mut harness = Harness::start(options).await;
    let installer = Arc::new(harness.topology.take_installer());

    // Connecting the channel reported the boot generation's status, before the worker ever ran.
    let transport = harness.transport.as_ref().unwrap();
    let reported = transport.apply_states.latest();
    assert_eq!(reported.status.active_generation, Some(1));
    assert_eq!(reported.status.live_generations, 1);
    assert_eq!(*reported, harness.status.load().apply_states);

    // The first run installs the artifact, then stops abruptly, as a worker aborted at an await point does.
    let first = tokio::spawn(run_installer(&installer, ShutdownHandle::noop()));
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    harness.status_where(|status| active(status) == Some(2)).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(harness.handed(), [Some(1)]);

    // The second run does not hand the core the snapshot the first one handled: handling it again could change what
    // it acknowledges. A configuration change, which the worker handles after any snapshot, shows that it did not.
    let second = tokio::spawn(run_installer(&installer, ShutdownHandle::noop()));
    harness
        .stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    harness.status_where(|status| status.config_changes == 1).await;
    assert_eq!(harness.handed(), [Some(1)]);

    // The second run panics while handling the next snapshot, as a bug in the core would.
    harness.script.lock().unwrap().panic_on_next_snapshot = true;
    harness.deliver(vec![harness.artifact(2, "dropped")]);
    assert!(tokio::time::timeout(PATIENCE, second)
        .await
        .expect("the second run panics")
        .unwrap_err()
        .is_panic());
    assert_eq!(harness.handed(), [Some(1), Some(2)]);

    // The third run hands the core the snapshot whose handling the panic interrupted, which installs.
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let third = tokio::spawn(run_installer(&installer, shutdown));
    let installed = harness.status_where(|status| active(status) == Some(3)).await;
    assert_eq!(harness.handed(), [Some(1), Some(2), Some(2)]);
    assert_eq!(
        (active_revision(&installed), installed.installer.revision_high_water),
        (Some(2), Some(2))
    );
    assert_eq!(harness.send(&[KEPT[0], KEPT[1], "dropped"]).await, Some(3));

    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, third).await.unwrap().unwrap();
    harness.finish().await;
}

#[tokio::test]
async fn the_installer_keeps_the_last_snapshot_and_follows_the_configuration_once_the_transport_is_gone() {
    let mut harness = Harness::start(Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    harness.deliver(vec![harness.artifact(1, "dropped")]);
    harness.status_where(|status| active(status) == Some(2)).await;

    // The transport goes away: the artifact stays in effect, and the installer still follows the configuration.
    drop(harness.transport.take());
    harness
        .stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    let changed = harness.status_where(|status| status.config_changes == 1).await;
    assert_eq!(active(&changed), Some(2));
    assert_eq!(harness.script.lock().unwrap().default_programs.len(), 1);
    assert_eq!(harness.send(&[KEPT[0], KEPT[1], "dropped"]).await, Some(2));
    harness.finish().await;
}

/// Performs `actions` as the worker does, except that every preparation is acknowledged by every slot at once, in
/// place of regions: for tests of the installer without a running topology.
async fn settle(installer: &mut Installer, actions: Vec<Action>) {
    let mut queue = VecDeque::from(actions);
    while let Some(action) = queue.pop_front() {
        match action {
            Action::Prepare(prepare) => {
                for slot in &prepare.slots {
                    queue.extend(
                        installer
                            .core
                            .prepare_acknowledged(prepare.generation.id(), slot.slot, Ok(())),
                    );
                }
                // The delivery hosts, whose recipients the generation requires, acknowledge their parts too.
                for part in &prepare.hosts {
                    queue.extend(installer.core.prepare_acknowledged(
                        prepare.generation.id(),
                        saluki_metric_plan::install::PrepareSite::Host(part.host.clone()),
                        Ok(()),
                    ));
                }
            }
            action => {
                installer.perform(action).await;
                queue.extend(installer.actions.drain(..));
            }
        }
    }
}

/// Returns the default program of `yaml` and its catalog.
async fn lowered(yaml: &str) -> (Plan, AdpCatalog) {
    let config = configuration(yaml).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .expect("the configuration lowers")
        .expect("a metrics pipeline is required")
        .lower(&config)
        .expect("the configuration lowers");
    (lowered.plan, lowered.catalog)
}

#[tokio::test]
async fn a_default_program_that_does_not_validate_is_reported_degraded_whenever_a_rollback_makes_it_active_again() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut path = programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
        .expect("a metrics pipeline is required");
    let installer = &mut path.installer;

    // Boot keeps the boot program's validation errors with its candidate, as for every default program. No
    // configuration lowers to a program that lays out but does not validate (the implementation design's amendment 14
    // records those that do not validate, none of which lays out), so the test puts errors in their place.
    let boot_candidate = installer
        .core
        .status()
        .active
        .expect("the boot generation is active")
        .candidate;
    assert_eq!(installer.degraded.get(&boot_candidate), Some(&Vec::new()));
    let errors = vec!["error[accepts] node `metrics_enrich`: the boot program's finding".to_string()];
    installer.degraded.insert(boot_candidate, errors.clone());
    installer.active_degraded = errors.clone();
    let reason = format!("the active default program does not validate: {}", errors[0]);
    assert_eq!(
        installer.current_status().apply_states.status.degraded,
        Some(reason.clone())
    );

    let (default, catalog) = lowered("").await;
    for (revision, generation) in [(1, 2), (2, 3)] {
        // An artifact that validates replaces the default program, which is kept to roll back to.
        let hash = PlanHash::of_bytes(format!("artifact {}", revision).as_bytes());
        let plan = dropping(&default, "dropped", revision, hash);
        let report = Validator::new(&OperatorRegistry::native(), &catalog).validate(&plan);
        assert!(report.is_valid(), "{:?}", report.errors().collect::<Vec<_>>());
        let (_, actions) = Script::propose(&mut installer.core, Arc::new(plan), report, revision, hash);
        settle(installer, actions).await;
        let installed = installer.current_status();
        assert_eq!(active(&installed), Some(generation));
        assert_eq!(
            installed.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
            Some(1)
        );
        assert_eq!(
            (
                installed.degraded.as_slice(),
                installed.apply_states.status.degraded.as_ref()
            ),
            (&[][..], None)
        );

        // The artifact fails: the default program is active again, and reported degraded again, each time.
        let actions = installer
            .core
            .generation_failed(GenerationId::new(generation), "it exceeded its panic budget");
        settle(installer, actions).await;
        let rolled_back = installer.current_status();
        assert_eq!(active(&rolled_back), Some(1), "after generation {} failed", generation);
        assert_eq!(rolled_back.degraded, errors, "after generation {} failed", generation);
        assert_eq!(
            rolled_back.apply_states.status.degraded.as_ref(),
            Some(&reason),
            "after generation {} failed",
            generation
        );
    }
}

#[tokio::test]
async fn an_artifact_panic_budget_applies_only_in_the_regions_that_run_its_nodes() {
    let (default, _) = lowered("").await;
    let registry = OperatorRegistry::native();
    let frame = Frame::compute_with_hosts(
        &default,
        &registry,
        &slot_name,
        &crate::programmable_metrics::test_support::frame_hosts(),
    )
    .expect("the default program lays out");
    let budgets = |plan: &Plan, origin: Origin| {
        frame
            .slots()
            .iter()
            .map(|slot| slot.name.to_string())
            .zip(slot_budgets(plan, &origin, &registry, &frame))
            .collect::<Vec<_>>()
    };
    let slots = |budgets: [PanicBudget; 3]| {
        ["dsd_pre_aggregation", "dsd_agg", "metrics_enrich"]
            .map(str::to_string)
            .into_iter()
            .zip(budgets)
            .collect::<Vec<_>>()
    };
    let (zero, artifact) = (PanicBudget::DEFAULT_PROGRAM, PanicBudget::ARTIFACT);
    let hash = PlanHash::of_bytes(b"budgets");
    let origin = Origin::Artifact { revision: 1, hash };

    // An artifact that adds nodes in front of the main output, in `metrics_enrich`: only that region may survive
    // panics. A panic in `dsd_pre_aggregation` or in aggregation's region, which run native nodes only, fails it, as
    // the native component does.
    let edited = dropping(&default, "dropped", 1, hash);
    assert_eq!(budgets(&edited, origin), slots([zero, zero, artifact]));

    // An artifact whose nodes are all native, such as an unmodified export composed again: no region survives panics.
    assert_eq!(budgets(&default, origin), slots([zero, zero, zero]));

    // The default program's budget is zero everywhere, whatever its nodes' provenance.
    let default_origin = Origin::DefaultProgram { config_change: 0 };
    assert_eq!(budgets(&edited, default_origin), slots([zero, zero, zero]));

    // The installer sends each region the budget of the plan the core prepares.
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut path = programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
        .expect("a metrics pipeline is required");
    let installer = &mut path.installer;
    let (_, catalog) = lowered("").await;
    let report = Validator::new(&registry, &catalog).validate(&edited);
    let (_, actions) = Script::propose(&mut installer.core, Arc::new(edited), report, 1, hash);
    assert!(actions
        .iter()
        .any(|action| matches!(action, Action::Prepare(prepare) if prepare.generation.id() == GenerationId::new(2))));
    // The budget covers the artifact's nodes only, so that a panic in a native node beside them, such as host
    // enrichment in `metrics_enrich`, fails the region.
    let artifact_nodes = ["policy/drop", "policy/drop_true"]
        .map(|node| node.parse::<NodeId>().unwrap())
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        installer.preparing_budgets(GenerationId::new(2)),
        (vec![zero, zero, artifact], artifact_nodes)
    );
    // A generation the core is not preparing gets the default program's budget: its acknowledgments are ignored.
    assert_eq!(
        installer.preparing_budgets(GenerationId::new(1)),
        (vec![zero, zero, zero], BTreeSet::new())
    );
}

/// Returns the config delivering `artifact` as revision `revision`, with its hash as the core computes it.
fn real_config(revision: u64, artifact: serde_json::Value) -> ArtifactConfig {
    let hash = saluki_metric_plan::ir::ReceivedArtifact::from_json(&artifact.to_string())
        .expect("the artifact is readable")
        .hash;
    let payload = json!({"revision": revision, "artifact_hash": hash, "artifact": artifact}).to_string();
    ArtifactConfig {
        id: ArtifactConfigId::new(CONFIG_ID),
        version: revision,
        sha256: ArtifactConfig::transport_hash(&payload),
        payload,
    }
}

/// Returns the acknowledgment of the config the tests deliver, as the installer last reported it.
fn ack(status: &PlanStatus) -> Option<(saluki_metric_plan::channel::ApplyState, Option<u64>, Option<u64>)> {
    status
        .apply_states
        .config_states
        .iter()
        .find(|ack| ack.id == ArtifactConfigId::new(CONFIG_ID))
        .map(|ack| (ack.state, ack.revision, ack.generation))
}

#[tokio::test]
async fn an_exported_artifact_installs_follows_configuration_and_rolls_back_through_the_real_core() {
    use saluki_metric_plan::channel::ApplyState;

    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let mut harness = Harness::start_with(Options::new(MetricsPath::Plan, Aggregation::Native), Paths::Core).await;
    let names = [KEPT[0], KEPT[1], "dropped", "blocked"];
    assert_eq!(harness.send(&names).await, Some(1));

    // The export of `post_enrich`, as the control surface serves it.
    let boot = harness.status.load_full();
    let scope = ScopeName::new(POST_ENRICH_SCOPE).unwrap();
    let bundle = boot.exports[&scope].clone().expect("the scope exports");
    let exported = serde_json::to_value(&bundle.artifact).unwrap();
    assert_eq!(
        boot.apply_states.status.default_program_hash,
        Some(bundle.artifact.base.program_hash)
    );

    // The unmodified export is in effect without a generation of its own.
    harness.deliver(vec![real_config(1, exported.clone())]);
    let unmodified = harness
        .status_where(|status| ack(status).is_some_and(|(state, ..)| state == ApplyState::Active))
        .await;
    assert_eq!(active(&unmodified), Some(1));
    assert_eq!(active_revision(&unmodified), Some(1));
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(1.0));

    // An edited export: a split in front of the main output that drops `dropped`.
    let mut edited = exported.clone();
    let edges = edited["edges"].as_array_mut().unwrap();
    let main = edges
        .iter_mut()
        .find(|edge| edge["to"] == "metrics_policy_0.in")
        .expect("the export emits the main output");
    main["to"] = json!("drop.in");
    edges.push(json!({"from": "drop.true", "to": "drop_true.in"}));
    edges.push(json!({"from": "drop.false", "to": "metrics_policy_0.in"}));
    let nodes = edited["nodes"].as_array_mut().unwrap();
    nodes.push(json!({
        "id": "drop", "op": "split",
        "params": {"predicate": {"intake_name": {"matches": {"prefix": false, "values": ["dropped"]}}}}
    }));
    nodes.push(json!({"id": "drop_true", "op": "drop"}));
    harness.deliver(vec![real_config(2, edited)]);
    let installed = harness.status_where(|status| active(status) == Some(2)).await;
    assert!(matches!(
        active_origin(&installed),
        Some(Origin::Artifact { revision: 2, .. })
    ));
    assert_eq!(ack(&installed), Some((ApplyState::Active, Some(2), Some(2))));
    assert_eq!(harness.send_until(&names, 2).await, harness.sent - 1);

    // A configuration change reaches the artifact through its composition: a new artifact generation under the same
    // revision, committed at a batch boundary.
    harness
        .stream
        .send(update("metric_filterlist", json!(["blocked"])))
        .await
        .unwrap();
    let recomposed = harness.status_where(|status| active(status) == Some(3)).await;
    assert!(matches!(
        active_origin(&recomposed),
        Some(Origin::Artifact { revision: 2, .. })
    ));
    assert_eq!(
        (
            recomposed.last_error.as_ref(),
            recomposed.apply_states.status.degraded.as_ref()
        ),
        (None, None)
    );
    assert_eq!(active_revision(&recomposed), Some(2));
    let plan = serde_json::to_string(&*recomposed.active.as_ref().unwrap().plan).unwrap();
    assert!(plan.contains("\"blocked\"") && plan.contains("policy/drop"), "{}", plan);
    harness.send_until(&names, 3).await;

    // It fails under supervision: the core rolls back to the generation kept, which serves the unmodified export of
    // revision 1, and composes that again against the current configuration.
    harness.fail(3, "it exceeded its panic budget");
    let fallen_back = harness.status_where(|status| active(status) == Some(4)).await;
    assert!(matches!(
        active_origin(&fallen_back),
        Some(Origin::Artifact { revision: 1, .. })
    ));
    assert_eq!(ack(&fallen_back).map(|(state, ..)| state), Some(ApplyState::Failed));
    assert_eq!(active_revision(&fallen_back), Some(1));
    assert_eq!(recorder.gauge(Key::from_name(PLAN_ACTIVE_REVISION)), Some(1.0));
    let generation = active(&fallen_back).unwrap();
    harness.send_until(&names, generation).await;
    let finished = harness.finish().await;
    for (index, generation) in finished.processed.iter().enumerate() {
        let (dropped, blocked) = match generation {
            1 => (true, true),
            2 => (false, true),
            3 => (false, false),
            _ => (true, false),
        };
        assert_eq!(
            (
                arrived(&finished.received, "dropped", index),
                arrived(&finished.received, "blocked", index)
            ),
            (dropped, blocked),
            "batch {}, generation {}",
            index,
            generation
        );
    }
}

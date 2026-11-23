//! Participant cases: cases written as participants and a procedure, which the runner runs through the lifecycle.
//!
//! Discovery sends a case file without a top-level `type` here. The case is loaded and planned for every OS it
//! declares, so a mistake for any OS is an error on any host, and the plan for the run's subject OS is kept. A case
//! that does not declare that OS is skipped, and the skip is recorded with its reason.
//!
//! Before the first case, the run checks once what the selected cases need: each realization they use, a Docker daemon
//! for containers and pods, every image they run, pulled once if the daemon lacks it, the courier they get, and, for
//! pods, a kind cluster that holds the images. For host processes it checks that the runner host runs the OS the case
//! declares and that every host program outside the participant's own directory can be run. Host processes need no
//! Docker daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use saluki_error::generic_error;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tracing::warn;

use crate::config::{runtime_of, KIND_RUNTIME};
use crate::core::backend::Backends;
use crate::core::case::{self, CaseError};
use crate::core::container::{ContainerBackend, LINUX};
use crate::core::host::{locate, HostBackend};
use crate::core::lifecycle::{self, CaseRun};
use crate::core::participant::{Enclosure, Os};
use crate::core::plan::{self, CasePlan, Launch};
use crate::core::pod::PodBackend;
use crate::core::registry::Registry;
use crate::core::result::TestResult;
use crate::events::TestEvent;
use crate::kind::KindLifecycle;
use crate::test::{Test, TestContext, TestSuite};

/// The file in a case's log directory that holds its plan when it does not pass.
const PLAN_FILE: &str = "plan.json";

/// How long pulling one image may take before the first case.
const PULL_TIME: Duration = Duration::from_secs(15 * 60);

/// How long setting up the kind cluster may take before the first case: creating it, pulling the images, and loading
/// them.
const KIND_SETUP_TIME: Duration = Duration::from_secs(30 * 60);

/// One `--set NAME=REFERENCE`: the image or host program a case names as `NAME` runs as `REFERENCE`.
#[derive(Clone, Debug)]
pub(crate) struct Replacement {
    pub(crate) name: String,
    pub(crate) reference: String,
}

impl FromStr for Replacement {
    type Err = String;

    /// Parses `name=reference`, splitting at the first `=`.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no `=`, or when either side of it is empty.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('=') {
            Some((name, reference)) if !name.is_empty() && !reference.is_empty() => Ok(Self {
                name: name.to_string(),
                reference: reference.to_string(),
            }),
            _ => Err(format!("expected <name>=<reference>, got '{}'", s)),
        }
    }
}

/// The run's replacements, by the reference each replaces.
///
/// # Errors
///
/// Returns an error if one reference is replaced twice.
pub(crate) fn replacements(entries: &[Replacement]) -> Result<BTreeMap<String, String>, String> {
    let mut set = BTreeMap::new();
    for entry in entries {
        if set.insert(entry.name.clone(), entry.reference.clone()).is_some() {
            return Err(format!("--set names '{}' twice.", entry.name));
        }
    }
    Ok(set)
}

/// A participant case, planned for the run's subject OS.
pub(crate) struct ParticipantCase {
    name: String,
    dir: PathBuf,
    suite: TestSuite,
    description: Option<String>,
    timeout: Duration,
    runtime: &'static str,
    /// Discovery leaves the case out.
    pub(crate) disabled: bool,
    /// The plan for the run's subject OS, or why the case is skipped.
    pub(crate) plan: Result<CasePlan, String>,
}

impl ParticipantCase {
    /// Loads the case in `dir` and plans it for every OS it declares, with the run's replacements `set`, then keeps the
    /// plan for `subject_os`. Returns the case with every reference its plans name, as written: images and host program
    /// paths.
    ///
    /// # Errors
    ///
    /// Returns every problem found while loading and planning, on every OS.
    pub(crate) fn load(
        dir: &Path, subject_os: Os, set: &BTreeMap<String, String>, registry: &Registry,
    ) -> Result<(Self, BTreeSet<String>), Vec<CaseError>> {
        let case = case::load(dir, registry)?;
        let mut plans = plan::plan_all(&case, registry, set)?;
        let references = references(plans.values());
        let plan = plans.remove(&subject_os).ok_or_else(|| {
            let declared: Vec<String> = case.subject_os.iter().map(ToString::to_string).collect();
            format!(
                "The case does not declare subject OS {}. It declares {}.",
                subject_os,
                declared.join(", ")
            )
        });
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let participant_case = Self {
            name: case.info.name.clone(),
            suite: suite(&dir),
            dir,
            description: case.info.description.clone(),
            timeout: case.info.timeout.duration,
            runtime: match &plan {
                Ok(plan) if uses_pods([plan]) => KIND_RUNTIME,
                _ => runtime_of(subject_os),
            },
            disabled: case.info.disabled,
            plan,
        };
        Ok((participant_case, references))
    }
}

/// Every image and host program path `plans` name, as written: what `--set` may replace.
pub(crate) fn references<'a>(plans: impl IntoIterator<Item = &'a CasePlan>) -> BTreeSet<String> {
    plans
        .into_iter()
        .flat_map(|plan| plan.participants.values())
        .flat_map(|participant| match &participant.launch {
            Launch::Image { image, .. } => vec![image.written.clone()],
            Launch::Programs(programs) => programs.iter().map(|program| program.path.written.clone()).collect(),
        })
        .collect()
}

/// The suite of the case in `dir`: the directory above its cases directory names it, as in `test/correctness/cases`.
fn suite(dir: &Path) -> TestSuite {
    let named = dir.parent().and_then(Path::parent).and_then(Path::file_name);
    if named == Some(OsStr::new("correctness")) {
        TestSuite::Correctness
    } else {
        TestSuite::Integration
    }
}

#[async_trait]
impl Test for ParticipantCase {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn suite(&self) -> TestSuite {
        self.suite
    }

    fn description(&self) -> Option<String> {
        self.description.clone()
    }

    fn case_path(&self) -> PathBuf {
        self.dir.clone()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    /// None: a participant case names its images in its plan, and `--set` replaces them.
    fn images(&self) -> BTreeMap<&str, String> {
        BTreeMap::new()
    }

    fn runtime(&self) -> String {
        self.runtime.to_string()
    }

    fn runs_host_processes(&self) -> bool {
        self.plan.as_ref().is_ok_and(|plan| {
            plan.participants
                .values()
                .any(|planned| planned.realization.enclosure == Enclosure::HostProcessGroup)
        })
    }

    fn participant_case(&self) -> Option<&ParticipantCase> {
        Some(self)
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        let plan = match &self.plan {
            Ok(plan) => plan,
            Err(reason) => return TestResult::skipped(&self.name, reason),
        };
        let cancel = tctx.test_cancel_token();
        let run = CaseRun {
            backends: &tctx.settings.backends,
            ledger: &tctx.ledger,
            tasks: &tctx.tasks,
            cancel: &cancel,
            deadline: tctx.deadline,
            log_dir: tctx.log_dir(),
            phases: &tctx.phases,
            record: &tctx.record,
        };
        let result = lifecycle::run(plan, &run).await;
        if !result.outcome.is_passed() {
            write_plan(plan, tctx.log_dir());
        }
        result
    }
}

/// Writes `plan` to the case's log directory `log_dir`, as `panoramic plan` prints it.
fn write_plan(plan: &CasePlan, log_dir: &Path) {
    let path = log_dir.join(PLAN_FILE);
    let written = serde_json::to_string_pretty(plan)
        .map_err(|e| e.to_string())
        .and_then(|json| std::fs::write(&path, json + "\n").map_err(|e| e.to_string()));
    if let Err(e) = written {
        warn!(path = %path.display(), error = %e, "Failed to write the plan of a case that did not pass.");
    }
}

/// What the checks before the first case need to set up a kind cluster.
pub(crate) struct KindSetup {
    /// The cluster's name.
    pub(crate) cluster: String,
    /// Where setup reports its progress, for the run's display.
    pub(crate) events: UnboundedSender<TestEvent>,
}

/// How far the checks before the first case got with the kind cluster.
#[derive(Default)]
pub(crate) enum KindState {
    /// Setup never started, so the checks did not touch kind.
    #[default]
    NotStarted,
    /// Setup started and did not finish, so a cluster may exist.
    Started,
    /// Setup finished, and the cluster holds the images.
    Ready(KindLifecycle),
}

/// Whether any of `plans` runs a participant in a pod, which needs a kind cluster.
pub(crate) fn uses_pods<'a>(plans: impl IntoIterator<Item = &'a CasePlan>) -> bool {
    plans.into_iter().any(|plan| {
        plan.participants
            .values()
            .any(|planned| planned.realization.enclosure == Enclosure::KubernetesPod)
    })
}

/// Checks, once and before the first case, what the selected participant cases need: each realization their
/// participants use, every image, pulled if the Docker daemon lacks it, the courier, and, for pods, a kind cluster that
/// holds the images. For host processes, the runner host must run the OS the participant declares, and every program
/// that is not under the participant's own directory must be found. `plans` holds the plan of each case that runs, by
/// name.
///
/// A kind cluster is made if it is missing, and a run with any pod sets it up once here, outside every case's timeout.
/// `kind_state` records how far setup got: `Ready` holds the cluster once setup succeeds, so the caller can remove it
/// even when connecting to it fails, and `Started` means setup began and a cluster may be left over. Setup that outlasts
/// `KIND_SETUP_TIME` fails and stops its commands.
///
/// Returns the backend of each realization the cases use.
///
/// # Errors
///
/// Returns every problem found: a realization the runner host cannot provide, an image that cannot be pulled, with the
/// cases that use it, a host program that cannot be found, with the cases that use it, a missing courier, or a kind
/// cluster that cannot be set up, with the cases that need it.
pub(crate) async fn check_before_the_first_case(
    plans: &[(String, &CasePlan)], kind: KindSetup, kind_state: &mut KindState,
) -> Result<Backends, Vec<String>> {
    let mut errors = Vec::new();
    let mut container_images: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut pod_images: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut pod_cases = Vec::new();
    let mut containers = false;
    let mut hosts = false;
    let mut host_programs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, plan) in plans {
        for (participant, planned) in &plan.participants {
            let images = match planned.realization.enclosure {
                Enclosure::LinuxContainer => {
                    containers = true;
                    &mut container_images
                }
                Enclosure::KubernetesPod => {
                    pod_cases.push(name.as_str());
                    &mut pod_images
                }
                Enclosure::HostProcessGroup => {
                    hosts = true;
                    let host = Os::host();
                    // TODO: stop host processes where there are no process groups, with the Windows backend.
                    if !cfg!(unix) {
                        errors.push(format!(
                            "Case '{}' runs participant '{}' as host processes, and the runner host {} cannot stop them: \
                             that backend is not built.",
                            name, participant, host
                        ));
                    } else if planned.realization.os != host {
                        errors.push(format!(
                            "Case '{}' runs participant '{}' as host processes on {}, and the runner host runs {}.",
                            name, participant, planned.realization.os, host
                        ));
                    } else if let Launch::Programs(programs) = &planned.launch {
                        for program in programs.iter().filter(|program| !program.path.used().starts_with("./")) {
                            host_programs
                                .entry(program.path.used().to_string())
                                .or_default()
                                .push(format!("{} (participant '{}')", name, participant));
                        }
                    }
                    continue;
                }
                // TODO: check the other realizations with the backends that run them.
                enclosure => {
                    errors.push(format!(
                        "Case '{}' runs participant '{}' as a {}, and the runner host cannot: that backend is not built.",
                        name, participant, enclosure
                    ));
                    continue;
                }
            };
            if let Launch::Image { image, .. } = &planned.launch {
                images.entry(image.used()).or_default().push(name);
            }
        }
    }
    pod_cases.dedup();
    let pods = !pod_cases.is_empty();
    for (path, cases) in host_programs {
        if let Err(why) = locate(&path, Path::new("")) {
            errors.push(format!(
                "A host program cannot be run, and {} use it: {}",
                cases.join(", "),
                why
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut backends = Backends::default();
    if hosts {
        match Enclosure::HostProcessGroup.facts().root {
            Some(root) => backends.insert(
                Enclosure::HostProcessGroup,
                Arc::new(HostBackend::new(PathBuf::from(root))),
            ),
            None => return Err(vec!["Host processes have no state directory.".to_string()]),
        }
    }
    if !(containers || pods) {
        return Ok(backends);
    }

    let docker = match airlock::docker::connect() {
        Ok(docker) => docker,
        Err(e) => return Err(vec![format!("Containers and pods need a Docker daemon: {:#}", e)]),
    };
    if let Err(e) = docker.ping().await {
        return Err(vec![format!("Containers and pods need a Docker daemon: {}", e)]);
    }
    for (image, mut cases) in container_images {
        cases.dedup();
        if let Err(e) = airlock::image::ensure_image(&docker, image, Instant::now() + PULL_TIME).await {
            errors.push(format!(
                "Image '{}' cannot be pulled, and cases {} use it: {:#}",
                image,
                cases.join(", "),
                e
            ));
        }
    }
    // The repository root is the directory panoramic runs in, as in every Make target and CI job.
    let mut courier = Vec::new();
    if containers || pods {
        let path = LINUX.courier_binary();
        match std::fs::read(&path) {
            Ok(bytes) => courier = bytes,
            Err(e) => errors.push(format!(
                "Containers and pods need the courier at {}, relative to the directory panoramic runs in, which `make build-courier` builds: {}",
                path.display(),
                e
            )),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    if containers {
        let backend = ContainerBackend::new(docker.clone(), &LINUX, courier.clone().into());
        backends.insert(Enclosure::LinuxContainer, Arc::new(backend));
    }
    if pods {
        let images = pod_images.keys().map(|image| image.to_string()).collect();
        let KindSetup { cluster, events } = kind;
        *kind_state = KindState::Started;
        let ensured = tokio::time::timeout(KIND_SETUP_TIME, KindLifecycle::ensure(cluster.clone(), images, events))
            .await
            .unwrap_or_else(|_| Err(generic_error!("Setup took longer than {:?}.", KIND_SETUP_TIME)));
        match ensured {
            Ok(ready) => *kind_state = KindState::Ready(ready),
            Err(e) => {
                return Err(vec![format!(
                    "Kind cluster '{}' cannot be set up for cases {}: {:#}",
                    cluster,
                    pod_cases.join(", "),
                    e
                )])
            }
        }
        match PodBackend::connect(&cluster, docker, courier.into()).await {
            Ok(backend) => backends.insert(Enclosure::KubernetesPod, Arc::new(backend)),
            Err(e) => {
                return Err(vec![format!(
                    "Kind cluster '{}' cannot run cases {}: {:#}",
                    cluster,
                    pod_cases.join(", "),
                    e
                )])
            }
        }
    }
    Ok(backends)
}

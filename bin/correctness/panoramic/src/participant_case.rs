//! Participant cases: cases written as participants and a procedure, which the runner runs through the lifecycle.
//!
//! Discovery loads each case directory here. The case is loaded and planned for every OS it declares, so a mistake for
//! any OS is an error on any host, and the plan for the run's subject OS is kept. A case that does not declare that OS
//! is skipped, and the skip is recorded with its reason.
//!
//! Before the first case, the run checks once what the selected cases need: each realization they use, a Docker daemon
//! for containers and pods, every image they run, pulled once if the daemon lacks it, the courier they get for each
//! container OS, and, for pods, a kind cluster that holds the images. A Windows container needs a Windows runner host.
//! For host processes it checks that the runner host runs the OS the case declares and that every host program outside
//! the participant's own directory can be run. Host processes need no Docker daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::core::backend::Backends;
use crate::core::case::{self, CaseError};
use crate::core::container::{ContainerBackend, LINUX, WINDOWS};
use crate::core::host::{locate, HostBackend};
use crate::core::lifecycle::{self, CaseRun};
use crate::core::participant::{Enclosure, Os};
use crate::core::plan::{self, CasePlan, Launch};
use crate::core::pod::PodBackend;
use crate::core::registry::Registry;
use crate::core::result::TestResult;
use crate::events::TestEvent;
use crate::kind::KindLifecycle;
use crate::test::{Test, TestContext};

/// The runtime name of a case that runs participants in pods, which need a kind cluster.
const KIND_RUNTIME: &str = "kubernetes_in_docker";

/// The runtime name of the subject OS `os`, which `list --json` and `run.json` report.
pub(crate) fn runtime_of(os: Os) -> &'static str {
    match os {
        Os::Linux => "linux",
        Os::Macos => "mac",
        Os::Windows => "windows",
    }
}

/// How long pulling one image may take before the first case.
const PULL_TIME: Duration = Duration::from_secs(15 * 60);

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

/// Loads every case in the direct subdirectories of `dirs` that holds a `config.yaml`, for `subject_os`, sorted by name.
///
/// A case that does not declare `subject_os` is included and skipped when it runs. A disabled case is loaded and
/// planned all the same, so it cannot rot, and is left out. `set` holds the run's replacements of the images and host
/// programs the cases name.
///
/// # Errors
///
/// Returns an error if a directory cannot be read, a case does not load or plan, or `set` replaces an image or host
/// program no case uses. A run reports every bad case at once.
pub(crate) fn discover(
    dirs: &[PathBuf], subject_os: Os, set: &BTreeMap<String, String>,
) -> Result<Vec<Box<dyn Test>>, GenericError> {
    let registry = crate::registry();
    let mut tests: Vec<Box<dyn Test>> = Vec::new();
    let mut case_errors = Vec::new();
    let mut replaced = BTreeSet::new();

    for base_path in dirs {
        if !base_path.is_dir() {
            return Err(generic_error!("Test directory does not exist: {}", base_path.display()));
        }

        let entries = std::fs::read_dir(base_path)
            .error_context(format!("Failed to read test directory: {}", base_path.display()))?;
        for entry in entries {
            let path = entry.error_context("Failed to read directory entry")?.path();
            if !path.is_dir() || !path.join(case::CASE_FILE).exists() {
                continue;
            }
            match ParticipantCase::load(&path, subject_os, set, &registry) {
                Ok((case, _)) if case.disabled => {}
                Ok((case, references)) => {
                    replaced.extend(references.into_iter().filter(|reference| set.contains_key(reference)));
                    tests.push(Box::new(case));
                }
                Err(errors) => case_errors.extend(errors.iter().map(ToString::to_string)),
            }
        }
    }

    // `replaced` holds each `--set` name some discovered case uses. Only when every case loaded: a case that failed may
    // be the one that uses the name, so `replaced` is incomplete then.
    if case_errors.is_empty() {
        case_errors.extend(
            set.keys()
                .filter(|name| !replaced.contains(*name))
                .map(|name| format!("--set names '{}', which no discovered case uses.", name)),
        );
    }
    if !case_errors.is_empty() {
        return Err(generic_error!("{}", case_errors.join("\n")));
    }

    tests.sort_by_key(|test| test.name());
    Ok(tests)
}

/// A participant case, planned for the run's subject OS.
pub(crate) struct ParticipantCase {
    name: String,
    dir: PathBuf,
    suite: String,
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

/// The suite of the case in `dir`: the name of the directory above its cases directory, as `correctness` for
/// `test/correctness/cases/x`, or the name of the cases directory when there is none above it.
fn suite(dir: &Path) -> String {
    let cases = dir.parent();
    cases
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .or_else(|| cases.and_then(Path::file_name))
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[async_trait]
impl Test for ParticipantCase {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn suite(&self) -> String {
        self.suite.clone()
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
        lifecycle::run(plan, &run).await
    }
}

/// What the checks before the first case need to set up a kind cluster.
pub(crate) struct KindSetup {
    /// The cluster's name.
    pub(crate) cluster: String,
    /// How long setup may take: creating the cluster, pulling the images, and loading them.
    pub(crate) timeout: Duration,
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
/// participants use, every image, pulled if the Docker daemon lacks it, the courier of each container OS, and, for
/// pods, a kind cluster that holds the images. A Windows container needs a Windows runner host. For host processes, the
/// runner host must run the OS the participant declares, and every program that is not under the participant's own
/// directory must be found. `plans` holds the plan of each case that runs, by
/// name.
///
/// A kind cluster is made if it is missing, and a run with any pod sets it up once here, outside every case's timeout.
/// `kind_state` records how far setup got: `Ready` holds the cluster once setup succeeds, so the caller can remove it
/// even when connecting to it fails, and `Started` means setup began and a cluster may be left over. Setup that outlasts
/// `kind.timeout` fails and stops its commands.
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
    let mut windows = false;
    let mut hosts = false;
    let mut host_programs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, plan) in plans {
        for (participant, planned) in &plan.participants {
            let images = match planned.realization.enclosure {
                Enclosure::LinuxContainer => {
                    containers = true;
                    &mut container_images
                }
                Enclosure::WindowsContainer => {
                    if Os::host() != Os::Windows {
                        errors.push(format!(
                            "Case '{}' runs participant '{}' as a Windows container, and the runner host runs {}.",
                            name,
                            participant,
                            Os::host()
                        ));
                        continue;
                    }
                    windows = true;
                    &mut container_images
                }
                Enclosure::KubernetesPod => {
                    pod_cases.push(name.as_str());
                    &mut pod_images
                }
                Enclosure::HostProcessGroup => {
                    hosts = true;
                    let host = Os::host();
                    // Stopping a tree of host processes on Windows needs a Job object, which nothing builds yet.
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
    if !(containers || windows || pods) {
        return Ok(backends);
    }

    let docker = match airlock::docker::connect() {
        Ok(docker) => docker,
        Err(e) => {
            return Err(vec![format!(
                "Linux and Windows containers and pods need a Docker daemon: {:#}",
                e
            )])
        }
    };
    if let Err(e) = docker.ping().await {
        return Err(vec![format!(
            "Linux and Windows containers and pods need a Docker daemon: {}",
            e
        )]);
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
    let mut windows_courier = Vec::new();
    if windows {
        let path = WINDOWS.courier_binary();
        match std::fs::read(&path) {
            Ok(bytes) => windows_courier = bytes,
            Err(e) => errors.push(format!(
                "Windows containers need the courier at {}, relative to the directory panoramic runs in, which \
                 ci/tooling/windows-integration-tests.ps1 builds: {}",
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
    if windows {
        let backend = ContainerBackend::new(docker.clone(), &WINDOWS, windows_courier.into());
        backends.insert(Enclosure::WindowsContainer, Arc::new(backend));
    }
    if pods {
        let images = pod_images.keys().map(|image| image.to_string()).collect();
        let KindSetup {
            cluster,
            timeout,
            events,
        } = kind;
        *kind_state = KindState::Started;
        let ensured = tokio::time::timeout(timeout, KindLifecycle::ensure(cluster.clone(), images, events))
            .await
            .unwrap_or_else(|_| Err(generic_error!("Setup took longer than {:?}.", timeout)));
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

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: &str = "timeout: 1m\nsubject_os: [linux]\nparticipants:\n  app:\n    subject: true\n    image: alpine:3.20\nprocedure: []\n";

    /// A directory of cases, each written as `(name, config.yaml)`.
    fn cases(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        for (name, config) in files {
            let case = dir.path().join(name);
            std::fs::create_dir(&case).expect("the case directory should be creatable");
            std::fs::write(case.join(case::CASE_FILE), config).expect("the case file should be writable");
        }
        dir
    }

    #[test]
    fn a_suite_is_the_directory_above_the_cases_directory_or_the_cases_directory_itself() {
        for (dir, suite) in [
            ("repo/test/correctness/cases/dsd-plain", "correctness"),
            ("repo/test/integration/cases/adp-cmd-host", "integration"),
            ("cases/some-case", "cases"),
        ] {
            assert_eq!(super::suite(Path::new(dir)), suite, "{dir}");
        }
    }

    fn names(tests: &[Box<dyn Test>]) -> Vec<String> {
        tests.iter().map(|test| test.name()).collect()
    }

    #[test]
    fn discovery_sorts_cases_and_leaves_out_disabled_ones_and_directories_without_a_case_file() {
        let dir = cases(&[
            ("b", CASE),
            ("a", CASE),
            ("off", &format!("disabled: true\n{CASE}")),
            ("shared", ""),
        ]);
        std::fs::remove_file(dir.path().join("shared").join(case::CASE_FILE)).unwrap();

        let tests = discover(&[dir.path().to_path_buf()], Os::Linux, &BTreeMap::new()).unwrap();

        assert_eq!(names(&tests), ["a", "b"]);
    }

    #[test]
    fn discovery_reports_every_bad_case_and_a_set_name_no_case_uses() {
        let bad = cases(&[("one", "type: integration\n"), ("two", "nonsense: 1\n")]);
        let error = discover(&[bad.path().to_path_buf()], Os::Linux, &BTreeMap::new())
            .err()
            .expect("two bad cases should fail discovery")
            .to_string();
        assert!(error.contains("case 'one'") && error.contains("case 'two'"), "{error}");

        let good = cases(&[("a", CASE)]);
        let set = BTreeMap::from([("alpine:3.20".to_string(), "mirror/alpine:3.20".to_string())]);
        assert_eq!(
            discover(&[good.path().to_path_buf()], Os::Linux, &set).unwrap().len(),
            1
        );

        let set = BTreeMap::from([("postgres:16".to_string(), "mirror/postgres:16".to_string())]);
        let error = discover(&[good.path().to_path_buf()], Os::Linux, &set)
            .err()
            .expect("a set name no case uses should fail discovery")
            .to_string();
        assert!(
            error.contains("--set names 'postgres:16', which no discovered case uses"),
            "{error}"
        );
    }
}

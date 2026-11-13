//! Participant cases: cases in the one case schema, which the shared runner runs through the one lifecycle.
//!
//! Discovery sends a case file without a top-level `type` here. The case is loaded and planned for every OS it
//! declares, so a mistake for any OS is an error on any host, and the plan for the run's subject OS is kept. A case
//! that does not declare that OS is skipped, and the skip is recorded with its reason.
//!
//! Before the first case, the run checks once what the selected cases need: each realization they use, a Docker daemon
//! for Linux containers, every image they run, pulled once if the daemon lacks it, and the courier the containers get.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tracing::warn;

use crate::config::runtime_of;
use crate::core::case::{self, CaseError};
use crate::core::container::{ContainerBackend, LINUX};
use crate::core::lifecycle::{self, CaseRun};
use crate::core::participant::{Enclosure, Os};
use crate::core::plan::{self, CasePlan, Launch};
use crate::core::registry::Registry;
use crate::core::result::{ErrorKind, TestResult};
use crate::test::{Test, TestContext, TestSuite};

/// The file in a case's log directory that holds its plan when it does not pass.
const PLAN_FILE: &str = "plan.json";

/// How long pulling one image may take before the first case.
const PULL_TIME: Duration = Duration::from_secs(15 * 60);

/// One `--set NAME=IMAGE`: the image a case names as `NAME` runs as `IMAGE`.
#[derive(Clone, Debug)]
pub(crate) struct Replacement {
    pub(crate) name: String,
    pub(crate) image: String,
}

impl FromStr for Replacement {
    type Err = String;

    /// Parses `name=image`, splitting at the first `=`.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no `=`, or when either side of it is empty.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('=') {
            Some((name, image)) if !name.is_empty() && !image.is_empty() => Ok(Self {
                name: name.to_string(),
                image: image.to_string(),
            }),
            _ => Err(format!("expected <name>=<image>, got '{}'", s)),
        }
    }
}

/// The run's replacements, by the image each replaces.
///
/// # Errors
///
/// Returns an error if one image is replaced twice.
pub(crate) fn replacements(entries: &[Replacement]) -> Result<BTreeMap<String, String>, String> {
    let mut set = BTreeMap::new();
    for entry in entries {
        if set.insert(entry.name.clone(), entry.image.clone()).is_some() {
            return Err(format!("--set names image '{}' twice.", entry.name));
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
    /// plan for `subject_os`. Returns the case with every image its plans name, as written.
    ///
    /// # Errors
    ///
    /// Returns every problem found while loading and planning, on every OS.
    pub(crate) fn load(
        dir: &Path, subject_os: Os, set: &BTreeMap<String, String>, registry: &Registry,
    ) -> Result<(Self, BTreeSet<String>), Vec<CaseError>> {
        let case = case::load(dir, registry)?;
        let mut plans = plan::plan_all(&case, registry, set)?;
        let images = images(plans.values());
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
            runtime: runtime_of(subject_os),
            disabled: case.info.disabled,
            plan,
        };
        Ok((participant_case, images))
    }
}

/// Every image `plans` name, as written.
pub(crate) fn images<'a>(plans: impl IntoIterator<Item = &'a CasePlan>) -> BTreeSet<String> {
    plans
        .into_iter()
        .flat_map(|plan| plan.participants.values())
        .filter_map(|participant| match &participant.launch {
            Launch::Image { image, .. } => Some(image.written.clone()),
            Launch::Programs(_) => None,
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

    fn participant_case(&self) -> Option<&ParticipantCase> {
        Some(self)
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        let plan = match &self.plan {
            Ok(plan) => plan,
            Err(reason) => return TestResult::skipped(&self.name, reason),
        };
        let Some(containers) = &tctx.settings.containers else {
            return TestResult::errored(
                &self.name,
                ErrorKind::Setup,
                "The run has no container backend: the checks before the first case did not run.",
                Duration::ZERO,
                Vec::new(),
            );
        };
        let cancel = tctx.test_cancel_token();
        let run = CaseRun {
            containers,
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

/// Checks, once and before the first case, what the selected participant cases need: each realization their
/// participants use, every image, pulled if the Docker daemon lacks it, and the courier. `plans` holds the plan of each
/// case that runs, by name.
///
/// Returns the backend that realizes the cases' Linux containers, if they have any.
///
/// # Errors
///
/// Returns every problem found: a realization the runner host cannot provide, an image that cannot be pulled, with the
/// cases that use it, or a missing courier.
pub(crate) async fn check_before_the_first_case(
    plans: &[(String, &CasePlan)],
) -> Result<Option<ContainerBackend>, Vec<String>> {
    let mut errors = Vec::new();
    let mut images: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut containers = false;
    for (name, plan) in plans {
        for (participant, planned) in &plan.participants {
            match planned.realization.enclosure {
                Enclosure::LinuxContainer => containers = true,
                // TODO: check the other realizations with the backends that run them.
                enclosure => errors.push(format!(
                    "Case '{}' runs participant '{}' as a {}, and the runner host cannot: that backend is not built.",
                    name, participant, enclosure
                )),
            }
            if let Launch::Image { image, .. } = &planned.launch {
                images.entry(image.used()).or_default().push(name);
            }
        }
    }
    if !containers || !errors.is_empty() {
        return if errors.is_empty() { Ok(None) } else { Err(errors) };
    }

    let docker = match airlock::docker::connect() {
        Ok(docker) => docker,
        Err(e) => return Err(vec![format!("Linux containers need a Docker daemon: {:#}", e)]),
    };
    if let Err(e) = docker.ping().await {
        return Err(vec![format!("Linux containers need a Docker daemon: {}", e)]);
    }
    for (image, mut cases) in images {
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
    let courier = LINUX.courier_binary();
    let courier = match std::fs::read(&courier) {
        Ok(courier) => Some(courier),
        Err(e) => {
            errors.push(format!(
                "Linux containers need the courier at {}, relative to the directory panoramic runs in, which `make build-courier` builds: {}",
                courier.display(),
                e
            ));
            None
        }
    };
    match courier {
        Some(courier) if errors.is_empty() => Ok(Some(ContainerBackend::new(docker, &LINUX, courier.into()))),
        _ => Err(errors),
    }
}

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use panoramic_core::case::{deserialize_case, deserialize_env_map, CaseSchema};
use panoramic_core::{
    artifact::ArtifactManifest,
    layout::Os,
    plan::{Compiled, Planner},
};
use saluki_error::{generic_error, GenericError};
use serde::{de::DeserializeOwned, Deserialize};

use self::runner_artifacts::RunnerArtifacts;
use self::typed_case::{LoadedCase, LoadedPlan};
use crate::correctness::config::Config as CorrectnessConfig;
use crate::image_override::ImageOverrides;
use crate::test::Test;

/// Runtime identifier for integration tests that run as host processes on macOS (no Docker, no
/// virtualization). Validated on macOS only today; future host-process runtimes for other Unix
/// platforms will get their own identifiers.
pub const MAC_RUNTIME: &str = "mac";

/// Runtime identifier for integration tests that run inside a Linux container.
pub const LINUX_RUNTIME: &str = "linux";

/// Runtime identifier for integration tests that run inside a Windows container.
pub const WINDOWS_RUNTIME: &str = "windows";

/// Returns the integration runtime identifier that runs cases for `os`.
pub(crate) fn runtime_for_os(os: Os) -> &'static str {
    match os {
        Os::Linux => LINUX_RUNTIME,
        Os::MacOs => MAC_RUNTIME,
        Os::Windows => WINDOWS_RUNTIME,
    }
}

/// Maps an integration runtime identifier to its subject OS, or `None` for other identifiers, such as `docker`.
pub(crate) fn os_for_runtime(runtime: &str) -> Option<Os> {
    match runtime {
        LINUX_RUNTIME => Some(Os::Linux),
        MAC_RUNTIME => Some(Os::MacOs),
        WINDOWS_RUNTIME => Some(Os::Windows),
        _ => None,
    }
}

/// A single variant in a `correctness_matrix` test.
///
/// Each variant expands into an independent correctness test. The variant's `env` is overlaid on
/// the environment variables of both the baseline and comparison targets in the base
/// configuration, allowing a single test directory to exercise multiple agent configurations
/// without duplicating the full config layout.
#[derive(Clone, Deserialize)]
pub struct MatrixVariant {
    /// Name suffix for this variant.
    ///
    /// The expanded test name is `{base_name}/{variant_name}`, for example,
    /// `dsd-origin-detection-matrix/unified`.
    pub name: String,

    /// Environment variables overlaid on both the baseline and comparison targets.
    ///
    /// Same mapping form as `env` on [`crate::correctness::config::TargetConfig`]. A variant entry replaces the base
    /// configuration's value for the same variable name; base entries the variant does not name are
    /// left alone.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
}

/// Variant metadata parsed separately from the shared correctness schema.
#[derive(Deserialize)]
struct MatrixVariants {
    variants: Vec<MatrixVariant>,
}

impl MatrixVariants {
    /// Expands environments without resolving paths or changing the base data.
    fn expand(self, base: &CorrectnessConfig, base_name: &str) -> Vec<(String, CorrectnessConfig)> {
        self.variants
            .into_iter()
            .map(|variant| {
                let mut config = base.clone();
                config.baseline.env.extend(variant.env.clone());
                config.comparison.env.extend(variant.env);
                (format!("{}/{}", base_name, variant.name), config)
            })
            .collect()
    }
}

/// A deserialized case that records its source file.
pub(crate) trait LoadedFrom {
    /// Records the canonical configuration file path after deserialization.
    fn set_loaded_from(&mut self, path: PathBuf);
}

/// Parses one test case configuration and records `source` as its file path.
///
/// The caller supplies a canonical source path. Every case type uses plain YAML deserialization.
/// Values come from the text alone: an unquoted `true` is a boolean; `"true"` is a string.
/// Images a case declares can be replaced after loading; see [`crate::image_override`].
///
/// # Errors
///
/// Returns an error if `content` does not deserialize into `T`.
fn parse_case<T>(content: &str, source: &Path) -> Result<T, GenericError>
where
    T: LoadedFrom + DeserializeOwned,
{
    let mut case: T = deserialize_case(content, source)?;

    case.set_loaded_from(source.to_path_buf());

    Ok(case)
}

/// Loads the manifest and validates its roles, returning error messages for the caller to report.
pub(crate) fn load_planner(artifacts: Option<&Path>) -> Result<Planner<'static>, String> {
    let manifest = artifacts
        .map(ArtifactManifest::load)
        .transpose()
        .map_err(|error| format!("{error:#}"))?
        .unwrap_or_default();
    Planner::new(crate::REGISTRIES, manifest).map_err(|error| format!("Invalid artifact roles: {error:#}"))
}

/// Enabled cases partitioned by the selected subject OS, with each list sorted by name.
pub(crate) struct Discovered {
    /// Cases that declare the selected subject OS, planned for it.
    pub(crate) plans: Vec<LoadedPlan>,
    /// Cases whose `subject_os` omits the selected OS and whose compilation checks passed.
    pub(crate) excluded: Vec<LoadedCase>,
}

/// Discovers and compiles enabled cases, sorted by name, using the caller's run planner.
pub(crate) fn discover_in_scope(
    planner: &mut Planner<'_>, dirs: &[PathBuf], subject_os: Os,
) -> Result<Discovered, GenericError> {
    let cases = crate::CASE_LOADER.discover_cases(dirs)?;
    let mut plans = Vec::new();
    let mut excluded = Vec::new();
    let mut errors = Vec::new();
    for case in cases.into_iter().filter(|case| !case.is_disabled()) {
        match planner.compile(case, subject_os) {
            Ok(Compiled::Planned(plan)) => plans.push(*plan),
            Ok(Compiled::Excluded(case)) => excluded.push(*case),
            Err(error) => errors.push(format!("{error:#}").replace('\n', "\n    ")),
        }
    }
    if !errors.is_empty() {
        return Err(generic_error!(
            "Failed to plan test cases:\n  - {}",
            errors.join("\n  - ")
        ));
    }
    Ok(Discovered { plans, excluded })
}

/// A compiled case and its execution state.
///
/// `P` is `LoadedPlan` when the runner task owns the plan, or `&LoadedPlan` when it borrows one.
pub(crate) struct BuiltCase<P = LoadedPlan> {
    /// The plan supplying the case metadata and selected subject OS.
    pub(crate) plan: P,
    pub(crate) execution: Execution,
}

/// The executable case body or the host's setup failure.
pub(crate) enum Execution {
    /// The typed runner.
    Runner(Box<dyn Test>),
    /// The panoramic-core lifecycle.
    Native,
    /// A setup error for a case the host cannot provide, with its message.
    Unprovidable(String),
}

impl<P> BuiltCase<P> {
    pub(crate) fn runner(&self) -> Option<&dyn Test> {
        match &self.execution {
            Execution::Runner(runner) => Some(runner.as_ref()),
            Execution::Native | Execution::Unprovidable(_) => None,
        }
    }
}

/// Builds executions in input order and validates image overrides across typed runners.
///
/// # Errors
///
/// Returns an error if an image override is duplicate or unmatched.
pub(crate) fn build_runners<P>(
    plans: impl IntoIterator<Item = P>, artifacts: &RunnerArtifacts<'_>,
) -> Result<Vec<BuiltCase<P>>, GenericError>
where
    P: Borrow<LoadedPlan>,
{
    let mut built = Vec::new();
    for plan in plans {
        let borrowed = plan.borrow();
        let execution = match borrowed.case().schema() {
            CaseSchema::Typed(config) => Execution::Runner(config.build_test(borrowed.case().name(), artifacts)),
            CaseSchema::Participants { .. } => Execution::Native,
        };
        built.push(BuiltCase { plan, execution });
    }
    if let RunnerArtifacts::Overrides(entries) = artifacts {
        let runners: Vec<_> = built.iter().filter_map(BuiltCase::runner).collect();
        ImageOverrides::new(entries).validate(&runners)?;
    }
    Ok(built)
}

pub(crate) mod runner_artifacts;
pub(crate) mod typed_case;

#[cfg(test)]
mod coverage_inventory;

#[cfg(test)]
pub(crate) mod tests {
    use panoramic_core::{
        artifact::ArtifactRole,
        case::{CaseLoader, Registries},
        layout::{Realization, RealizationKind},
        participant::ParticipantType,
    };
    use panoramic_participants::AgentAdp;

    use super::typed_case::LoadedCase;
    use super::*;
    use crate::correctness::case::CorrectnessTestCase;
    use crate::image_override::ImageOverride;

    #[test]
    fn load_planner_returns_manifest_and_role_errors_without_logging_them() {
        let dir = tempfile::tempdir().unwrap();
        let log = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(log.reopen().unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert!(load_planner(None).is_ok());
            for (file, content, expected) in [
                ("missing.toml", None, "failed to read artifact manifest"),
                ("malformed.toml", Some("not toml"), "TOML parse error"),
                (
                    "unknown.toml",
                    Some("schema = 1\n[roles]\nunknown = 'x'"),
                    "Invalid artifact roles:",
                ),
            ] {
                let path = dir.path().join(file);
                if let Some(content) = content {
                    std::fs::write(&path, content).unwrap();
                }
                let error = load_planner(Some(&path)).err().expect("invalid manifest must fail");
                assert!(error.contains(expected), "{error}");
                assert!(error.contains(path.to_str().unwrap()), "{error}");
            }
        });
        assert!(std::fs::read_to_string(log.path()).unwrap().is_empty());
    }

    pub(crate) const CORRECTNESS_FIELDS: &str =
        "runtime: docker\nanalysis_mode: metrics\nbaseline: {image: 'base:file'}\ncomparison: {image: 'comp:file'}\n";

    /// A one-step procedure for participant cases whose procedure isn't under test.
    macro_rules! minimal_procedure {
        () => {
            "procedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]"
        };
    }
    pub(crate) use minimal_procedure;

    pub(crate) const PARTICIPANT_FIELDS: &str = concat!(
        "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  adp: {type: agent-adp, subject: true}\n",
        minimal_procedure!(),
        "\n"
    );
    pub(crate) const NO_PARTICIPANT_FIELDS: &str = concat!(
        "timeout: 1s\nsubject_os: [linux]\nparticipants: {}\n",
        minimal_procedure!(),
        "\n"
    );

    pub(crate) const LOGGING_PARTICIPANT: &str = r#"description: Portable logging assertions
timeout: 120s
subject_os: [linux, macos, windows]
participants:
  adp:
    type: agent-adp
    subject: true
    config:
      env:
        DD_API_KEY: "0000"
        DD_LOG_FILE: /tmp/core.log
procedure:
  - step: log_contains
    on: adp
    pattern: Initial configuration received.
    timeout: 30s
  - parallel:
      - step: log_not_contains
        on: adp
        pattern: panic|PANIC
        regex: true
        during: 10s
      - step: file_contains
        on: adp
        path: /tmp/core.log
        pattern: CORE
        timeout: 20s
  - step: file_contains
    on: adp
    path: 'C:\adp\adp.log'
    pattern: DATAPLANE.*ready
    regex: true
    timeout: 60s
"#;

    /// Creates a test planner with the binary's registries and local artifact defaults.
    pub(crate) fn planner() -> Planner<'static> {
        Planner::with_run(crate::REGISTRIES, "inventry")
    }

    /// Loads and compiles one case for tests.
    pub(crate) fn plan(yaml: &str, os: Os) -> LoadedPlan {
        let case = crate::CASE_LOADER
            .load_content(yaml, "case", Path::new("/cases/case/config.yaml"))
            .unwrap()
            .remove(0);
        planner().compile(case, os).unwrap().into_plan().unwrap()
    }

    pub(crate) struct Plain;

    impl ParticipantType for Plain {
        fn name(&self) -> &'static str {
            "plain"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer),
            ]
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(crate) fn plan_with_plain(yaml: &str, name: &str, os: Os) -> LoadedPlan {
        let registries = Registries {
            participant_types: &[&AgentAdp, &Plain],
            ..crate::REGISTRIES
        };
        let source = format!("/cases/{name}/config.yaml");
        let case = CaseLoader::new(registries, typed_case::TypedConfigLoader, "integration")
            .load_content(yaml, name, Path::new(&source))
            .unwrap()
            .remove(0);
        Planner::new(registries, ArtifactManifest::default())
            .unwrap()
            .compile(case, os)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[test]
    fn typed_cases_build_their_runner_on_every_runtime() {
        let correctness = format!("type: correctness\n{CORRECTNESS_FIELDS}");
        let pod = correctness.replace("runtime: docker", "runtime: kubernetes_in_docker");
        for (yaml, os, runtime) in [
            (correctness.as_str(), Os::Linux, "docker"),
            (pod.as_str(), Os::Linux, "kubernetes_in_docker"),
        ] {
            let built = build_runners([plan(yaml, os)], &RunnerArtifacts::Overrides(&[])).unwrap();
            assert_eq!(built.len(), 1);
            assert!(matches!(built[0].execution, Execution::Runner(_)));
            assert_eq!(built[0].runner().unwrap().runtime(), runtime);
        }
    }

    #[test]
    fn participant_cases_build_native_executions_on_every_subject_os() {
        let single = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]");
        let multiple = single.replace("  adp:", "  other: {type: agent-adp}\n  adp:");
        let plain = single.replace("agent-adp", "plain");
        let mixed = single.replace("  adp:", "  other: {type: plain, os: linux}\n  adp:");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plans = [
                plan_with_plain(&single, "single", os),
                plan_with_plain(&multiple, "multiple", os),
                plan_with_plain(&plain, "plain", os),
                plan_with_plain(&mixed, "mixed", os),
            ];
            let built = build_runners(&plans, &RunnerArtifacts::Overrides(&[])).unwrap();
            assert_eq!(built.len(), 4);
            for (built, name) in built.iter().zip(["single", "multiple", "plain", "mixed"]) {
                assert_eq!(built.plan.case().name(), name);
                assert!(matches!(built.execution, Execution::Native));
                assert!(built.runner().is_none());
            }
        }
    }

    fn load_case<T, P>(path: P) -> Result<T, GenericError>
    where
        T: LoadedFrom + DeserializeOwned,
        P: AsRef<Path>,
    {
        let source = path.as_ref().canonicalize()?;
        let content = std::fs::read_to_string(&source)?;
        parse_case(&content, &source)
    }

    #[test]
    fn every_subject_os_maps_to_its_runtime_and_back() {
        for (os, runtime) in [(Os::Linux, "linux"), (Os::MacOs, "mac"), (Os::Windows, "windows")] {
            assert_eq!(runtime_for_os(os), runtime);
            assert_eq!(os_for_runtime(runtime), Some(os));
        }
        for runtime in ["docker", "kubernetes_in_docker", "macos", ""] {
            assert_eq!(os_for_runtime(runtime), None, "{runtime}");
        }
    }

    pub(crate) fn build_case_runners(
        cases: impl IntoIterator<Item = LoadedCase>, os: Os, overrides: &[ImageOverride],
    ) -> Result<Vec<BuiltCase>, GenericError> {
        let plans = cases
            .into_iter()
            .map(|case| planner().compile(case, os))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|plan| {
                plan.into_plan()
                    .expect("the test case declares the selected subject OS")
            })
            .collect::<Vec<_>>();
        build_runners(plans, &RunnerArtifacts::Overrides(overrides))
    }

    fn discover_runners(
        dirs: &[PathBuf], runtime: &str, image_overrides: &[ImageOverride],
    ) -> Result<Vec<BuiltCase>, GenericError> {
        let os = os_for_runtime(runtime).unwrap();
        let plans = discover_in_scope(&mut planner(), dirs, os)?.plans;
        build_runners(plans, &RunnerArtifacts::Overrides(image_overrides))
    }

    #[test]
    fn discovery_reports_excluded_cases_in_name_order_beside_the_plans() {
        let windows = create_test_case_dir(
            "windows-only",
            "name: windows-only\ntimeout: 1s\nsubject_os: [windows]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        let disabled = create_test_case_dir(
            "disabled",
            "type: correctness\ndisabled: true\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
        );
        let matrix = create_test_case_dir(
            "matrix",
            "type: correctness_matrix\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\nvariants: [{name: second}, {name: first}]\n",
        );
        let cases = crate::CASE_LOADER
            .discover_cases(&[
                windows.path().to_path_buf(),
                matrix.path().to_path_buf(),
                disabled.path().to_path_buf(),
            ])
            .unwrap();
        assert_eq!(
            cases.iter().map(LoadedCase::name).collect::<Vec<_>>(),
            ["disabled", "matrix/first", "matrix/second", "windows-only"]
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let discovered = discover_in_scope(
                &mut planner(),
                &[
                    windows.path().to_path_buf(),
                    matrix.path().to_path_buf(),
                    disabled.path().to_path_buf(),
                ],
                os,
            )
            .unwrap();
            let plans = discovered
                .plans
                .iter()
                .map(|plan| plan.case().name())
                .collect::<Vec<_>>();
            let excluded = discovered.excluded.iter().map(LoadedCase::name).collect::<Vec<_>>();
            match os {
                Os::Linux => {
                    assert_eq!(plans, ["matrix/first", "matrix/second"]);
                    assert_eq!(excluded, ["windows-only"]);
                }
                Os::MacOs => {
                    assert!(plans.is_empty());
                    assert_eq!(excluded, ["matrix/first", "matrix/second", "windows-only"]);
                }
                Os::Windows => {
                    assert_eq!(plans, ["windows-only"]);
                    assert_eq!(excluded, ["matrix/first", "matrix/second"]);
                }
            }
        }
    }

    #[test]
    fn discovery_reports_every_malformed_file_sorted_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = write_case(dir.path(), "b-unknown-type", "type: bogus\n");
        let invalid = write_case(dir.path(), "a-invalid-yaml", "type: [");
        let good = write_case(
            dir.path(),
            "good",
            "name: good\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );

        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("malformed files must fail discovery");
        let message = format!("{error:#}");
        assert!(message.starts_with("Failed to load test cases:\n  - "), "{message}");
        assert!(message.lines().skip(1).all(|line| line.starts_with("  ")), "{message}");
        let invalid_position = message
            .find(&invalid.canonicalize().unwrap().display().to_string())
            .unwrap();
        let unknown_position = message
            .find(&unknown.canonicalize().unwrap().display().to_string())
            .unwrap();
        assert!(invalid_position < unknown_position, "{message}");
        assert!(message.contains("Failed to parse config file as YAML"), "{message}");
        assert!(message.contains("Unknown test type 'bogus'"), "{message}");
        assert!(
            !message.contains(&good.canonicalize().unwrap().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn discovery_rejects_a_case_file_named_config_yml() {
        for with_config_yaml in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let yaml = write_case(
                dir.path(),
                "stray",
                "name: stray\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
            );
            let yml = yaml.with_extension("yml");
            if with_config_yaml {
                std::fs::copy(&yaml, &yml).unwrap();
            } else {
                std::fs::rename(&yaml, &yml).unwrap();
            }
            write_case(
                dir.path(),
                "good",
                "name: good\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
            );

            let error = crate::CASE_LOADER
                .discover_cases(&[dir.path().to_path_buf()])
                .err()
                .expect("config.yml must fail discovery");
            let message = format!("{error:#}");
            assert_eq!(
                message,
                format!(
                    "Failed to load test cases:\n  - Case file {} is named config.yml; panoramic loads only config.yaml, so rename it",
                    yml.canonicalize().unwrap().display()
                ),
                "with_config_yaml = {with_config_yaml}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_a_dangling_config_yml_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("stray");
        std::fs::create_dir(&case_dir).unwrap();
        let stray_path = case_dir.join("config.yml");
        std::os::unix::fs::symlink("missing.yaml", &stray_path).unwrap();

        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("a dangling config.yml symlink must fail discovery");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "Failed to load test cases:\n  - Case file {} is named config.yml; panoramic loads only config.yaml, so rename it",
                stray_path.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_a_dangling_config_yaml_symlink() {
        // A `config.yaml` whose symlink target is missing still names a case, so discovery must report it.
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("stray");
        std::fs::create_dir(&case_dir).unwrap();
        let stray_path = case_dir.join("config.yaml");
        std::os::unix::fs::symlink("missing.yaml", &stray_path).unwrap();
        let good = write_case(
            dir.path(),
            "good",
            "name: good\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );

        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("a dangling config.yaml symlink must fail discovery");
        let message = format!("{error:#}");
        assert!(
            message.starts_with(&format!(
                "Failed to load test cases:\n  - Failed to resolve configuration file: {}",
                stray_path.display()
            )),
            "{message}"
        );
        assert!(
            !message.contains(&good.canonicalize().unwrap().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn discovery_ignores_a_directory_without_a_case_file() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("support");
        std::fs::create_dir(&case_dir).unwrap();
        std::fs::write(case_dir.join("datadog.yaml"), "api_key: test\n").unwrap();
        std::fs::write(dir.path().join("loose.yaml"), "type: bogus\n").unwrap();
        let good = write_case(
            dir.path(),
            "good",
            "name: good\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );

        let cases = crate::CASE_LOADER.discover_cases(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(cases.iter().map(LoadedCase::name).collect::<Vec<_>>(), ["good"]);
        assert_eq!(cases[0].source(), good.canonicalize().unwrap());
    }

    #[test]
    fn a_stray_config_yml_is_reported_with_the_other_problems_in_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = write_case(dir.path(), "invalid", "type: [");
        let yml = yaml.with_extension("yml");
        std::fs::write(&yml, "type: correctness\n").unwrap();

        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("both case file problems must fail discovery");
        let message = format!("{error:#}");
        let problems: Vec<_> = message.split("\n  - ").collect();
        assert_eq!(problems.len(), 3, "{message}");
        assert_eq!(problems[0], "Failed to load test cases:");
        assert!(
            problems[1].starts_with(&format!(
                "Failed to parse config file as YAML: {}",
                yaml.canonicalize().unwrap().display()
            )),
            "{message}"
        );
        assert_eq!(
            problems[2],
            format!(
                "Case file {} is named config.yml; panoramic loads only config.yaml, so rename it",
                yml.canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn discovery_rejects_a_name_declared_in_two_directories_and_names_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut roots = [dir.path().join("windows"), dir.path().join("disabled")];
        let mut sources = [
            write_case(
                &roots[0],
                "shared",
                "name: shared\ntimeout: 1s\nsubject_os: [windows]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
            ).canonicalize().unwrap(),
            write_case(
                &roots[1],
                "shared",
                "type: correctness\ndisabled: true\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
            ).canonicalize().unwrap(),
        ];
        sources.sort();
        let expected = format!(
            "Failed to load test cases:\n  - Case name 'shared' is declared more than once: {}, {}",
            sources[0].display(),
            sources[1].display()
        );
        for _ in 0..2 {
            let error = crate::CASE_LOADER
                .discover_cases(&roots)
                .err()
                .expect("duplicate names must fail discovery");
            assert_eq!(format!("{error:#}"), expected);
            let error = discover_in_scope(&mut planner(), &roots, Os::Linux)
                .err()
                .expect("runtime and disabled filtering must not hide duplicate names");
            assert_eq!(format!("{error:#}"), expected);
            roots.reverse();
        }
    }

    #[test]
    fn discovery_rejects_a_matrix_variant_name_declared_twice() {
        let dir = create_test_case_dir(
            "matrix",
            "type: correctness_matrix\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\nvariants: [{name: x}, {name: x}]\n",
        );
        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("duplicate variant names must fail discovery");
        let source = dir.path().join("matrix/config.yaml").canonicalize().unwrap();
        assert_eq!(
            format!("{error:#}"),
            format!(
                "Failed to load test cases:\n  - Case name 'matrix/x' is declared more than once: {0}, {0}",
                source.display()
            )
        );
    }

    #[test]
    fn discovery_reports_load_failures_and_duplicate_names_together() {
        let dir = tempfile::tempdir().unwrap();
        let roots = [dir.path().join("first"), dir.path().join("second")];
        let malformed = write_case(&roots[0], "malformed", "type: bogus\n");
        for root in &roots {
            for name in ["z-shared", "a-shared"] {
                write_case(
                    root,
                    name,
                    &format!("name: {name}\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {{adp: {{type: agent-adp, subject: true}}}}\nprocedure: [{{step: log_contains, on: adp, pattern: ready, timeout: 5s}}]\n"),
                );
            }
        }
        let error = crate::CASE_LOADER
            .discover_cases(&roots)
            .err()
            .expect("all problems must fail discovery");
        let message = format!("{error:#}");
        let malformed_position = message
            .find(&malformed.canonicalize().unwrap().display().to_string())
            .unwrap();
        let first_duplicate = message.find("Case name 'a-shared' is declared more than once").unwrap();
        let second_duplicate = message.find("Case name 'z-shared' is declared more than once").unwrap();
        assert!(malformed_position < first_duplicate, "{message}");
        assert!(first_duplicate < second_duplicate, "{message}");
    }

    #[test]
    fn windows_participant_cases_accept_explicit_realizations_and_env_references() {
        let dir = tempfile::tempdir().unwrap();
        let mut plans = Vec::new();
        for (name, yaml) in [
            (
                "second",
                PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: windows-container}"),
            ),
            (
                "integration",
                "name: integration\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n".to_string(),
            ),
            (
                "first",
                PARTICIPANT_FIELDS.replace(
                    "subject: true",
                    "subject: true, config: {env: {KEY: '{{ self.facts.default_log_file }}'}}",
                ),
            ),
        ] {
            let yaml = yaml.replace("[linux]", "[windows]");
            let path = write_case(dir.path(), name, &yaml);
            let case = crate::CASE_LOADER.load_file(&path).unwrap().remove(0);
            plans.push(planner().compile(case, Os::Windows).unwrap().into_plan().unwrap());
        }
        let built = build_runners(&plans, &RunnerArtifacts::Overrides(&[])).unwrap();
        assert_eq!(
            built.iter().map(|case| case.plan.case().name()).collect::<Vec<_>>(),
            ["second", "integration", "first"]
        );
        assert!(matches!(built[0].execution, Execution::Native));
        assert!(matches!(built[1].execution, Execution::Native));
        assert!(matches!(built[2].execution, Execution::Native));
        let overrides = ["unmatched=image:override".parse().unwrap()];
        let error = build_runners(&plans, &RunnerArtifacts::Overrides(&overrides))
            .err()
            .expect("unmatched overrides must still fail");
        assert_eq!(
            error.to_string(),
            "No test in this run uses a container named 'unmatched'. Available: no test in this run declares a container image."
        );
    }

    #[test]
    fn discovery_aggregates_participant_schema_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut sources = Vec::new();
        for (name, yaml, expected) in [
            ("a-os", PARTICIPANT_FIELDS.replace("[linux]", "[]"), "empty subject_os"),
            (
                "b-participants",
                NO_PARTICIPANT_FIELDS.to_string(),
                "declares no participants",
            ),
            (
                "c-duplicate",
                PARTICIPANT_FIELDS.replace(
                    "  adp: {type: agent-adp, subject: true}",
                    "  adp: {type: agent-adp}\n  adp: {type: agent-adp}",
                ),
                "duplicate",
            ),
            (
                "d-connection",
                PARTICIPANT_FIELDS.replace("subject: true", "connect: {intake: invalid}"),
                "connection 'invalid'",
            ),
            (
                "e-procedure",
                PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{parallel: []}]"),
                "at least one step",
            ),
            (
                "e-procedure-empty",
                PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: []"),
                "a procedure must contain at least one step",
            ),
            (
                "f-subject",
                PARTICIPANT_FIELDS.replace(
                    "  adp: {type: agent-adp, subject: true}",
                    "  adp: {type: agent-adp, subject: true}\n  other: {type: agent-adp, subject: true}",
                ),
                "are both marked 'subject: true'",
            ),
            (
                "g-step",
                PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{step: wait_ready}]"),
                "unknown step 'wait_ready'",
            ),
        ] {
            sources.push((write_case(dir.path(), name, &yaml).canonicalize().unwrap(), expected));
        }
        let error = crate::CASE_LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("invalid cases must fail discovery");
        let message = format!("{error:#}");
        let errors: Vec<_> = message.split("\n  - ").collect();
        assert_eq!(errors[0], "Failed to load test cases:");
        assert_eq!(errors.len(), sources.len() + 1);
        for (error, (source, expected)) in errors[1..].iter().zip(sources) {
            assert!(error.contains(&source.display().to_string()), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn runners_follow_the_order_of_the_cases_they_are_built_from() {
        let integration = create_test_case_dir(
            "integration",
            "name: integration\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        let correctness = create_test_case_dir(
            "correctness",
            "type: correctness\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
        );
        let mut cases = crate::CASE_LOADER
            .discover_cases(&[integration.path().to_path_buf(), correctness.path().to_path_buf()])
            .unwrap();
        cases.reverse();
        let entries = ["baseline=base:override".parse().unwrap()];
        let plans: Vec<_> = cases
            .into_iter()
            .map(|case| planner().compile(case, Os::Linux).unwrap().into_plan().unwrap())
            .collect();
        let runners = build_runners(&plans, &RunnerArtifacts::Overrides(&entries)).unwrap();
        assert_eq!(runners.len(), plans.len());
        assert_eq!(
            plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
            ["integration", "correctness"]
        );
        assert_eq!(
            runners.iter().map(|built| built.plan.case().name()).collect::<Vec<_>>(),
            ["integration", "correctness"]
        );
        assert!(matches!(runners[0].execution, Execution::Native));
        assert_eq!(runners[1].runner().unwrap().images()["baseline"], "base:override");
        assert_eq!(runners[1].runner().unwrap().images()["comparison"], "comp");
    }

    #[test]
    fn matrix_variants_report_the_case_directory_they_expanded_from() {
        let base_dir = create_test_case_dir(
            "dsd-matrix",
            r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
variants:
  - name: first
    env:
      DD_EXAMPLE: "1"
  - name: second
    env:
      DD_EXAMPLE: "2"
"#,
        );

        let runners = discover_runners(&[base_dir.path().to_path_buf()], "linux", &[]).unwrap();

        assert_eq!(runners.len(), 2);
        // Canonicalized, because the loader canonicalizes the config path it derives this from.
        let case_dir = base_dir.path().join("dsd-matrix").canonicalize().unwrap();
        for built in &runners {
            assert_eq!(
                built.plan.case().source().parent().unwrap(),
                case_dir,
                "case path for '{}'",
                built.plan.case().name()
            );
        }
    }

    #[test]
    fn correctness_target_env_becomes_key_value_assignments_ordered_by_name() {
        let base_dir = create_test_case_dir(
            "dsd-env",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_TAGS: "a=1,b=2"
    DD_API_KEY: correctness-test
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DATA_PLANE_ENABLED: "true"
"#,
        );
        let config_path = base_dir.path().join("dsd-env").join("config.yaml");

        let config: CorrectnessConfig = load_case(&config_path).expect("case should parse");

        // What the Docker adapter hands to Airlock: one assignment per variable, ordered by name,
        // with a value containing '=' left intact.
        assert_eq!(
            crate::correctness::case::env_assignments(&config.baseline.env),
            vec!["DD_API_KEY=correctness-test".to_string(), "DD_TAGS=a=1,b=2".to_string()]
        );

        // Each target owns its own environment: the comparison-only variable does not leak into the
        // baseline.
        assert_eq!(
            crate::correctness::case::env_assignments(&config.comparison.env),
            vec![
                "DD_API_KEY=correctness-test".to_string(),
                "DD_DATA_PLANE_ENABLED=true".to_string()
            ]
        );
    }

    #[test]
    fn correctness_target_env_rejects_an_unquoted_boolean_value() {
        // An unquoted `true` is a YAML boolean, not the string the process would receive, so the
        // case must fail to load rather than guess at a value.
        let base_dir = create_test_case_dir(
            "dsd-bare-bool",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_DATA_PLANE_ENABLED: true
"#,
        );
        let config_path = base_dir.path().join("dsd-bare-bool").join("config.yaml");

        let error = match load_case::<CorrectnessConfig, _>(&config_path) {
            Ok(_) => panic!("an unquoted boolean env value should be rejected"),
            Err(e) => format!("{e:?}"),
        };

        assert!(error.contains("DD_DATA_PLANE_ENABLED"), "unexpected error: {error}");
    }

    #[test]
    fn a_case_relative_path_anchors_at_the_case_directory() {
        let base_dir = create_test_case_dir(
            "dsd-paths",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
millstone:
  config_path: millstone.yaml
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
"#,
        );
        let config_path = base_dir.path().join("dsd-paths").join("config.yaml");

        let config: CorrectnessConfig = load_case(&config_path).expect("case should parse");

        // Canonicalized, because the loader canonicalizes the config path it derives the case
        // directory from. On macOS that turns the temp directory's `/tmp` into `/private/tmp`.
        let case_dir = base_dir.path().join("dsd-paths").canonicalize().unwrap();
        assert_eq!(
            CorrectnessTestCase::new("paths".to_string(), config.clone())
                .millstone_config()
                .config_path,
            case_dir.join("millstone.yaml")
        );

        // An absolute path is the case's own choice and passes through untouched. Pick one that is
        // absolute on the platform running the test: a Unix-style root is not absolute on Windows.
        let absolute_path = if cfg!(windows) {
            PathBuf::from(r"C:\etc\datadog-agent\datadog.yaml")
        } else {
            PathBuf::from("/etc/datadog-agent/datadog.yaml")
        };
        assert_eq!(
            crate::test::resolve_case_path(&config.loaded_from, &absolute_path),
            absolute_path
        );
    }

    #[test]
    fn matrix_variant_env_overlays_both_targets_and_wins_on_a_shared_variable() {
        let base_dir = create_test_case_dir(
            "dsd-matrix",
            r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DOGSTATSD_TAG_CARDINALITY: low
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DOGSTATSD_TAG_CARDINALITY: low
    DD_DATA_PLANE_ENABLED: "true"
variants:
  - name: high-cardinality
    env:
      DD_DOGSTATSD_TAG_CARDINALITY: high
      DD_ORIGIN_DETECTION_UNIFIED: "true"
"#,
        );
        let config_path = base_dir.path().join("dsd-matrix").join("config.yaml");

        let base: CorrectnessConfig = load_case(&config_path).unwrap();
        let matrix: MatrixVariants = serde_saphyr::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let expanded = matrix.expand(&base, "dsd-matrix");

        assert_eq!(expanded.len(), 1);
        let variant = &expanded[0].1;
        for (side, target) in [("baseline", &variant.baseline), ("comparison", &variant.comparison)] {
            // The variant wins on the shared variable, adds its own, and leaves the rest of the base
            // environment alone.
            assert_eq!(
                target.env.get("DD_DOGSTATSD_TAG_CARDINALITY").map(String::as_str),
                Some("high"),
                "{side} should take the variant's cardinality"
            );
            assert_eq!(
                target.env.get("DD_ORIGIN_DETECTION_UNIFIED").map(String::as_str),
                Some("true"),
                "{side} should gain the variant's own variable"
            );
            assert_eq!(
                target.env.get("DD_API_KEY").map(String::as_str),
                Some("correctness-test"),
                "{side} should keep base variables the variant does not name"
            );
        }

        // The variant overlay does not merge the two targets: comparison-only settings stay there.
        assert_eq!(
            variant.comparison.env.get("DD_DATA_PLANE_ENABLED").map(String::as_str),
            Some("true")
        );
        assert!(!variant.baseline.env.contains_key("DD_DATA_PLANE_ENABLED"));
    }

    #[test]
    fn matrix_expansion_preserves_file_data_and_keeps_variants_independent() {
        // Matrix expansion used to reconstruct fields and resolve paths itself. Both forms now
        // retain file data and use the same case preparation and path resolution.
        let dir = create_test_case_dir(
            "matrix",
            r#"
type: correctness_matrix
runtime: kubernetes_in_docker
analysis_mode: traces
millstone:
  image: tools:custom
  binary_path: /custom/millstone
  config_path: nested/millstone.yaml
datadog_intake:
  image: intake:custom
  binary_path: /custom/intake
baseline:
  image: baseline:custom
  entrypoint: [baseline]
  command: [run]
  files: ["nested/agent.yaml:/etc/agent.yaml"]
  env: {SHARED: base, BASELINE_ONLY: baseline}
comparison:
  image: comparison:custom
  entrypoint: [comparison]
  command: [start]
  files: ["nested/comparison.yaml:/etc/agent.yaml"]
  env: {SHARED: base, COMPARISON_ONLY: comparison}
otlp_direct_analysis_mode: true
additional_span_ignore_fields: [metrics.example]
require_dogstatsd_forwarded_packets: true
variants:
  - name: first
    env: {SHARED: first, FIRST_ONLY: first}
  - name: second
    env: {SHARED: second}
"#,
        );
        let path = dir.path().join("matrix/config.yaml");
        let ordinary: CorrectnessConfig = load_case(&path).unwrap();
        let matrix: MatrixVariants = serde_saphyr::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let expanded = matrix.expand(&ordinary, "matrix");
        assert_eq!(
            expanded.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["matrix/first", "matrix/second"]
        );
        let ordinary_case = CorrectnessTestCase::new("ordinary".to_string(), ordinary.clone());
        for (name, config) in expanded {
            assert_eq!(config.loaded_from, path.canonicalize().unwrap());
            assert_eq!(config.millstone.config_path, PathBuf::from("nested/millstone.yaml"));
            assert_eq!(config.baseline.files, ordinary.baseline.files);
            assert_eq!(config.comparison.files, ordinary.comparison.files);
            assert_eq!(config.baseline.entrypoint, ordinary.baseline.entrypoint);
            assert_eq!(config.comparison.command, ordinary.comparison.command);
            assert!(config.otlp_direct_analysis_mode);
            assert_eq!(config.additional_span_ignore_fields, ["metrics.example"]);
            assert!(config.require_dogstatsd_forwarded_packets);
            let variant = name.strip_prefix("matrix/").unwrap();
            for target in [&config.baseline, &config.comparison] {
                assert_eq!(target.env["SHARED"], variant);
                assert_eq!(target.env.contains_key("FIRST_ONLY"), variant == "first");
            }
            assert_eq!(config.baseline.env["BASELINE_ONLY"], "baseline");
            assert!(!config.baseline.env.contains_key("COMPARISON_ONLY"));
            assert_eq!(config.comparison.env["COMPARISON_ONLY"], "comparison");
            assert!(!config.comparison.env.contains_key("BASELINE_ONLY"));
            let case = CorrectnessTestCase::new(name.clone(), config);
            assert_eq!(case.runtime(), "kubernetes_in_docker");
            assert_eq!(case.images(), ordinary_case.images());
            assert_eq!(
                case.millstone_config().config_path,
                ordinary_case.millstone_config().config_path
            );
            assert_eq!(
                case.millstone_config().binary_path.as_deref(),
                Some("/custom/millstone")
            );
            assert_eq!(
                case.datadog_intake_config().binary_path.as_deref(),
                Some("/custom/intake")
            );
        }
        assert_eq!(ordinary.baseline.env["SHARED"], "base");
        assert!(!ordinary.comparison.env.contains_key("FIRST_ONLY"));
    }

    #[tokio::test]
    async fn ordinary_and_matrix_targets_prepare_overridden_images_env_and_relative_mounts() {
        let container_root = if cfg!(windows) { "C:/etc" } else { "/etc" };
        let dir = create_test_case_dir(
            "prepared",
            &format!(
                r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
millstone:
  config_path: nested/millstone.yaml
baseline:
  image: baseline:file
  entrypoint: [baseline, --init]
  command: [sleep, 10]
  env: {{BASELINE_ONLY: baseline, SHARED: base, DD_TAGS: "a=1,b=2"}}
  files: ["nested/baseline.yaml:{container_root}/baseline.yaml", "nested/common.yaml:{container_root}/common.yaml"]
comparison:
  image: comparison:file
  entrypoint: [comparison, --init]
  command: [wait, 20]
  env: {{COMPARISON_ONLY: comparison, SHARED: base, DD_TAGS: "a=1,b=2"}}
  files: ["nested/comparison.yaml:{container_root}/comparison.yaml", "nested/common.yaml:{container_root}/common.yaml"]
variants:
  - name: first
    env: {{SHARED: first, FIRST_ONLY: "true"}}
  - name: second
    env: {{SHARED: second}}
"#,
            ),
        );
        let path = dir.path().join("prepared/config.yaml");
        let base: CorrectnessConfig = load_case(&path).unwrap();
        let matrix: MatrixVariants = serde_saphyr::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut configs = vec![("prepared/base".to_string(), base.clone())];
        configs.extend(matrix.expand(&base, "prepared"));
        let entries = [
            "baseline=baseline:custom",
            "comparison=comparison:custom",
            "datadog-intake=intake:custom",
            "millstone=millstone:custom",
        ]
        .map(|entry| entry.parse().unwrap());
        let overrides = ImageOverrides::new(&entries);
        let case_dir = path.parent().unwrap().canonicalize().unwrap();
        for (name, mut config) in configs {
            let variant = name.strip_prefix("prepared/").unwrap().to_string();
            overrides.apply_correctness(&mut config);
            let case = CorrectnessTestCase::new(name, config);
            for entry in &entries {
                assert_eq!(case.images()[entry.name.as_str()], entry.image);
            }
            assert_eq!(case.datadog_intake_config().image, "intake:custom");
            assert_eq!(case.millstone_config().image, "millstone:custom");
            assert_eq!(
                case.millstone_config().config_path,
                case_dir.join("nested/millstone.yaml")
            );
            for (side, target, command) in [
                ("baseline", &case.config.baseline, ["sleep", "10"]),
                ("comparison", &case.config.comparison, ["wait", "20"]),
            ] {
                let (prepared, mounts) = case.target_config(target).unwrap();
                assert_eq!(prepared.image, format!("{side}:custom"));
                assert_eq!(prepared.entrypoint, [side, "--init"]);
                assert_eq!(prepared.command, command);
                assert_eq!(prepared.container_os, airlock::driver::ContainerOs::Linux);
                assert!(!prepared.host_cgroup_namespace);
                let mut expected_env = vec![
                    format!("{}_ONLY={side}", side.to_uppercase()),
                    "DD_TAGS=a=1,b=2".to_string(),
                ];
                if variant == "first" {
                    expected_env.push("FIRST_ONLY=true".to_string());
                }
                expected_env.push(format!("SHARED={variant}"));
                assert_eq!(prepared.additional_env_vars, expected_env);
                assert_eq!(
                    mounts,
                    vec![
                        (
                            case_dir.join(format!("nested/{side}.yaml")),
                            PathBuf::from(format!("{container_root}/{side}.yaml"))
                        ),
                        (
                            case_dir.join("nested/common.yaml"),
                            PathBuf::from(format!("{container_root}/common.yaml"))
                        ),
                    ]
                );
                // DriverConfig is opaque; assert its prepared inputs above and exercise construction here.
                case.target_driver_config(target).await.expect("valid target driver");
                assert_eq!(
                    target.files[0],
                    format!("nested/{side}.yaml:{container_root}/{side}.yaml")
                );
            }
        }
        assert_eq!(base.baseline.image, "baseline:file");
        assert_eq!(base.comparison.env["SHARED"], "base");
        assert!(!base.baseline.env.contains_key("FIRST_ONLY"));
    }

    #[test]
    fn ordinary_and_matrix_targets_preserve_yaml_scalar_spellings() {
        // Flattening the matrix base buffers YAML scalars as Serde Content, rejecting numeric
        // commands. Parse the original YAML directly so String fields also retain spellings like 1e3.
        let dir = create_test_case_dir(
            "scalars",
            r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
baseline:
  image: base
  command: [sleep, 10, 01, 1e3, true, 1.50]
  entrypoint: [env, 20, 02, 2e3, false, 2.50]
  env: {BOOL: "true", NUMBER: "8125", SCIENTIFIC: "1e3", LEADING_ZERO: "01"}
comparison:
  image: comp
  command: [wait, 30, 03, 3e3, false, 3.50]
  entrypoint: [exec, 40, 04, 4e3, true, 4.50]
  env: {BOOL: "false", NUMBER: "8126", SCIENTIFIC: "2e3", LEADING_ZERO: "02"}
variants: [{name: one, env: {QUOTED: "true"}}]
"#,
        );
        let path = dir.path().join("scalars/config.yaml");
        let ordinary: CorrectnessConfig = load_case(&path).unwrap();
        let matrix: MatrixVariants = serde_saphyr::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let expanded = matrix.expand(&ordinary, "scalars");
        let runners = discover_runners(&[dir.path().to_path_buf()], LINUX_RUNTIME, &[]).unwrap();
        assert_eq!(runners.len(), 1);
        assert_eq!(runners[0].plan.case().name(), "scalars/one");
        for config in [&ordinary, &expanded[0].1] {
            assert_eq!(config.baseline.command, ["sleep", "10", "01", "1e3", "true", "1.50"]);
            assert_eq!(config.baseline.entrypoint, ["env", "20", "02", "2e3", "false", "2.50"]);
            assert_eq!(config.comparison.command, ["wait", "30", "03", "3e3", "false", "3.50"]);
            assert_eq!(
                config.comparison.entrypoint,
                ["exec", "40", "04", "4e3", "true", "4.50"]
            );
            assert_eq!(config.baseline.env["BOOL"], "true");
            assert_eq!(config.baseline.env["NUMBER"], "8125");
            assert_eq!(config.baseline.env["SCIENTIFIC"], "1e3");
            assert_eq!(config.baseline.env["LEADING_ZERO"], "01");
            assert_eq!(config.comparison.env["BOOL"], "false");
            assert_eq!(config.comparison.env["NUMBER"], "8126");
            assert_eq!(config.comparison.env["SCIENTIFIC"], "2e3");
            assert_eq!(config.comparison.env["LEADING_ZERO"], "02");
        }
        assert!(!ordinary.baseline.env.contains_key("QUOTED"));
        assert_eq!(expanded[0].1.baseline.env["QUOTED"], "true");
        assert_eq!(expanded[0].1.comparison.env["QUOTED"], "true");
    }

    #[test]
    fn all_environment_maps_reject_non_string_yaml_values() {
        for value in [
            "true",
            "false",
            "8125",
            "01",
            "1e3",
            "1.5",
            "null",
            "[text]",
            "{key: text}",
        ] {
            for (kind, section) in [
                ("correctness", "baseline"),
                ("correctness", "comparison"),
                ("correctness_matrix", "baseline"),
                ("correctness_matrix", "comparison"),
                ("correctness_matrix", "variant"),
            ] {
                let env = format!("env: {{INVALID: {value}}}");
                let baseline_env = if section == "baseline" { &env } else { "" };
                let comparison_env = if section == "comparison" { &env } else { "" };
                let variant_env = if section == "variant" { &env } else { "" };
                let yaml = format!(
                    r#"
type: {kind}
runtime: docker
analysis_mode: metrics
baseline:
  image: base
  {baseline_env}
comparison:
  image: comp
  {comparison_env}
variants:
  - name: one
    {variant_env}
"#
                );
                let dir = create_test_case_dir("strict", &yaml);
                let case_dir = dir.path().join("strict");
                let error = crate::CASE_LOADER
                    .load_file(&case_dir.join("config.yaml"))
                    .err()
                    .expect("non-string env value must fail");
                let error = format!("{error:?}");
                assert!(
                    error.contains("environment variable 'INVALID' must be a YAML string"),
                    "{kind} {section} {value}: {error}"
                );
            }
        }
    }

    #[test]
    fn correctness_cases_are_discovered_and_eligible_unless_disabled() {
        for (kind, variants, case_count) in [
            ("correctness", "", 1),
            ("correctness_matrix", "variants: [{name: first}, {name: second}]", 2),
        ] {
            for (setting, disabled) in [("", false), ("disabled: false", false), ("disabled: true", true)] {
                let dir = create_test_case_dir(
                    "case",
                    &format!(
                        r#"
type: {kind}
{setting}
runtime: docker
analysis_mode: metrics
baseline: {{image: base}}
comparison: {{image: comp}}
{variants}
"#
                    ),
                );
                let config: CorrectnessConfig = load_case(dir.path().join("case/config.yaml")).unwrap();
                assert_eq!(config.disabled, disabled, "{kind} with {setting}");

                let cases = crate::CASE_LOADER.discover_cases(&[dir.path().to_path_buf()]).unwrap();
                assert_eq!(cases.len(), case_count, "{kind} with {setting}");
                assert_eq!(
                    discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
                        .unwrap()
                        .plans
                        .len(),
                    if disabled { 0 } else { case_count },
                    "{kind} with {setting}"
                );
            }
        }
    }

    #[test]
    fn runners_apply_correctness_overrides_to_ordinary_and_matrix_cases() {
        let entries = [
            "baseline=base:override",
            "comparison=comp:override",
            "millstone=tools:override",
            "datadog-intake=intake:override",
        ]
        .map(|entry| entry.parse().unwrap());
        for (kind, variants, count) in [
            ("correctness", "", 1),
            ("correctness_matrix", "variants: [{name: first}, {name: second}]", 2),
        ] {
            let dir = create_test_case_dir(
                "case",
                &format!(
                    r#"
type: {kind}
runtime: docker
analysis_mode: metrics
baseline: {{image: base}}
comparison: {{image: comp}}
{variants}
"#
                ),
            );
            let runners = discover_runners(&[dir.path().to_path_buf()], LINUX_RUNTIME, &entries).unwrap();
            assert_eq!(runners.len(), count);
            for built in runners {
                let images = built.runner().unwrap().images();
                for entry in &entries {
                    assert_eq!(images[entry.name.as_str()], entry.image);
                }
            }
        }
    }

    #[test]
    fn parser_records_its_source_without_resolving_declared_paths() {
        let source = Path::new("/cases/relative/config.yaml");
        let config: CorrectnessConfig = parse_case(
            r#"
runtime: docker
analysis_mode: metrics
loaded_from: ignored
baseline: {image: base}
comparison: {image: comp}
"#,
            source,
        )
        .unwrap();
        assert_eq!(config.loaded_from, source);
        assert_eq!(config.millstone.config_path, PathBuf::from("millstone.yaml"));
        let case = CorrectnessTestCase::new("relative".to_string(), config);
        assert_eq!(
            case.millstone_config().config_path,
            Path::new("/cases/relative").join("millstone.yaml")
        );
    }

    struct TestCaseDir {
        path: PathBuf,
    }

    impl TestCaseDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestCaseDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn create_test_case_dir(case_name: &str, config: &str) -> TestCaseDir {
        let unique = format!(
            "panoramic-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base_dir = std::env::temp_dir().join(unique);
        write_case(&base_dir, case_name, config);

        TestCaseDir { path: base_dir }
    }

    pub(crate) fn write_case(root: &Path, name: &str, yaml: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }
}

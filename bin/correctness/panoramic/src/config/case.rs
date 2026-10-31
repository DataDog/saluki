//! A [`CaseConfig`] is one case loaded from a `config.yaml`.
//!
//! It records the name, description, suite, source file, timeout and declared subject operating systems.
//! The parsed integration, correctness or participant data stays private. [`CaseConfig::build_test`] builds runners
//! from copies, so image overrides leave the loaded case unchanged; a participant case builds an integration runner.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

use super::{
    case_file::{CaseFile, ProcedureNode},
    deserialize_case, parse_case,
    participant_check::check_participants,
    step_check::build_steps,
    IntegrationConfig, MatrixVariants, Step, LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME,
};
use crate::{
    correctness::{case::CorrectnessTestCase, config::Config as CorrectnessConfig},
    image_override::ImageOverrides,
    integration::{IntegrationSettings, IntegrationTestCase},
    participants::REGISTERED_TYPES,
    steps::REGISTERED_STEPS,
    test::{Test, TestSuite},
};

/// An operating system a case's processes run on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Os {
    Linux,
    #[allow(
        clippy::enum_variant_names,
        reason = "macOS is one word; the Os suffix is part of its name."
    )]
    MacOs,
    Windows,
}

impl Os {
    /// Returns the OS spelling used in case files.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
        }
    }

    /// Returns the integration runtime identifier for the OS.
    pub(crate) fn runtime(self) -> &'static str {
        match self {
            Self::Linux => LINUX_RUNTIME,
            Self::MacOs => MAC_RUNTIME,
            Self::Windows => WINDOWS_RUNTIME,
        }
    }

    /// Maps an integration runtime identifier to its OS.
    pub(crate) fn from_runtime(runtime: &str) -> Option<Self> {
        match runtime {
            LINUX_RUNTIME => Some(Self::Linux),
            MAC_RUNTIME => Some(Self::MacOs),
            WINDOWS_RUNTIME => Some(Self::Windows),
            _ => None,
        }
    }
}

/// Time budget for correctness cases, which run two isolation groups of several containers each.
pub(crate) const CORRECTNESS_TIMEOUT: Duration = Duration::from_mins(20);

/// Immutable case metadata and the file data used to build its runners.
pub(crate) struct CaseConfig {
    name: String,
    description: Option<String>,
    suite: TestSuite,
    source: PathBuf,
    timeout: Duration,
    subject_os: BTreeSet<Os>,
    disabled: bool,
    schema: CaseSchema,
}

/// Parsed case data, with each correctness matrix variant expanded into its own case.
pub(super) enum CaseSchema {
    Integration(IntegrationConfig),
    Correctness(CorrectnessConfig),
    Participants {
        file: CaseFile,
        procedure: Vec<ProcedureNode<Box<dyn Step>>>,
    },
}

impl CaseConfig {
    /// Loads every case a file declares, including disabled cases and individual matrix variants.
    ///
    /// # Errors
    ///
    /// Returns path resolution and read errors, an error if the directory has no name,
    /// and every error of [`Self::load_content`].
    pub(crate) fn load_file(config_path: &Path) -> Result<Vec<Self>, GenericError> {
        let canonical_path = config_path.canonicalize().error_context(format!(
            "Failed to resolve configuration file: {}",
            config_path.display()
        ))?;
        let dir_name = config_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                generic_error!(
                    "Cannot name the case in {}: its directory has no name",
                    canonical_path.display()
                )
            })?;
        let config_path = canonical_path.as_path();
        let content = std::fs::read_to_string(config_path)
            .error_context(format!("Failed to read config file: {}", config_path.display()))?;

        Self::load_content(&content, dir_name, config_path)
    }

    /// Loads `content` as the `config.yaml` recorded at `source`, in directory `directory`.
    ///
    /// # Errors
    ///
    /// Files without a top-level `type` declare participants. Returns an error if the content contains
    /// invalid YAML, has a non-string or unknown `type`, or fails schema deserialization.
    /// A declared case name must match the directory name.
    /// Integration runtimes must be nonempty and recognized; a correctness matrix must have at least
    /// one variant. Participant cases must declare at least one participant and a nonempty, unique
    /// `subject_os` list using `linux`, `macos` or `windows`. Participant names must be DNS labels starting
    /// with a letter, other than `self`; exactly one subject is required, without an `os` override.
    /// Types must be registered, configuration names declared by the type, and OS overlays reachable.
    /// Connections must join a declared consumed endpoint to another participant's declared served endpoint.
    /// Each step must name a registered step and decode its fields strictly. Only `file_contains.path`
    /// accepts template references, to participant facts declared on every OS the participant may run on.
    /// Participant and process names must be declared.
    pub(super) fn load_content(content: &str, directory: &str, source: &Path) -> Result<Vec<Self>, GenericError> {
        let peek: serde_json::Value = serde_saphyr::from_str(content)
            .error_context(format!("Failed to parse config file as YAML: {}", source.display()))?;

        let test_type = match peek.get("type") {
            None => {
                let file: CaseFile = deserialize_case(content, source)?;
                let name = case_name(file.name.as_deref(), directory, source)?;
                let subject_os = file.check(&name, source)?;
                check_participants(&file, &subject_os, REGISTERED_TYPES, &name, source)?;
                let procedure = build_steps(&file, &subject_os, REGISTERED_STEPS, REGISTERED_TYPES, &name, source)?;
                return Ok(vec![Self {
                    name,
                    description: file.description.clone(),
                    // TODO: derive the suite from the case's directory.
                    suite: TestSuite::Integration,
                    source: source.to_path_buf(),
                    timeout: file.timeout.0,
                    subject_os,
                    disabled: false,
                    schema: CaseSchema::Participants { file, procedure },
                }]);
            }
            Some(serde_json::Value::String(test_type)) => test_type.as_str(),
            Some(_) => {
                return Err(generic_error!(
                    "'type' in {} must be a string: 'integration', 'correctness' or 'correctness_matrix'; \
                     a case that declares participants has no top-level 'type'",
                    source.display()
                ));
            }
        };

        match test_type {
            "integration" => {
                let config: IntegrationConfig = parse_case(content, source)?;
                let name = case_name(Some(&config.name), directory, source)?;
                if config.runtimes.is_empty() {
                    return Err(generic_error!(
                        "integration test '{}' in {} has empty runtimes list",
                        config.name,
                        source.display()
                    ));
                }
                let subject_os = config
                    .runtimes
                    .iter()
                    .map(|runtime| {
                        Os::from_runtime(runtime).ok_or_else(|| {
                            generic_error!(
                                "integration test '{}' in {} declares unknown runtime '{}' (expected '{}', '{}', or '{}')",
                                config.name,
                                source.display(),
                                runtime,
                                LINUX_RUNTIME,
                                MAC_RUNTIME,
                                WINDOWS_RUNTIME
                            )
                        })
                    })
                    .collect::<Result<BTreeSet<_>, _>>()?;
                Ok(vec![Self {
                    name,
                    description: config.description.clone(),
                    suite: TestSuite::Integration,
                    source: config.loaded_from.clone(),
                    timeout: config.timeout.0,
                    subject_os,
                    disabled: false,
                    schema: CaseSchema::Integration(config),
                }])
            }
            "correctness" => {
                let config = parse_case(content, source)?;
                let name = case_name(None, directory, source)?;
                Ok(vec![Self::from_correctness(name, config)])
            }
            "correctness_matrix" => {
                // Parse each schema from YAML text; flattening or a Value round-trip changes scalar coercion.
                let base: CorrectnessConfig = parse_case(content, source)?;
                let matrix: MatrixVariants = deserialize_case(content, source)?;
                let base_name = case_name(None, directory, source)?;
                if matrix.variants.is_empty() {
                    return Err(generic_error!(
                        "correctness_matrix '{}' in {} has no variants defined",
                        base_name,
                        source.display()
                    ));
                }
                Ok(matrix
                    .expand(&base, &base_name)
                    .into_iter()
                    .map(|(name, config)| Self::from_correctness(name, config))
                    .collect())
            }
            other => Err(generic_error!(
                "Unknown test type '{}' in {} (expected 'integration', 'correctness', or 'correctness_matrix'; \
                 a case that declares participants has no top-level 'type')",
                other,
                source.display()
            )),
        }
    }

    fn from_correctness(name: String, config: CorrectnessConfig) -> Self {
        Self {
            name,
            description: None,
            suite: TestSuite::Correctness,
            source: config.loaded_from.clone(),
            timeout: CORRECTNESS_TIMEOUT,
            subject_os: BTreeSet::from([Os::Linux]),
            disabled: config.disabled,
            schema: CaseSchema::Correctness(config),
        }
    }

    /// Scopes integration and participant cases to their declared OSes;
    /// enabled correctness cases accept every runtime.
    pub(crate) fn is_eligible_for(&self, runtime: &str) -> bool {
        if self.disabled {
            return false;
        }
        match self.schema {
            CaseSchema::Integration(_) | CaseSchema::Participants { .. } => {
                Os::from_runtime(runtime).is_some_and(|os| self.subject_os.contains(&os))
            }
            CaseSchema::Correctness(_) => true,
        }
    }

    /// Translates a participant case into the configuration its integration runner receives.
    ///
    /// # Errors
    ///
    /// Returns an error naming the case and source if it uses another schema or suite, or has unsupported
    /// participant settings or steps, or does not declare the requested subject OS; see [`CaseFile::integration_config`].
    pub(super) fn integration_config(&self, os: Os) -> Result<IntegrationConfig, GenericError> {
        let CaseSchema::Participants { file, procedure } = &self.schema else {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: integration translation requires a case that declares participants",
                self.name,
                self.source.display()
            ));
        };
        if self.suite != TestSuite::Integration {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: the integration runner runs only integration-suite cases",
                self.name,
                self.source.display()
            ));
        }
        if !self.subject_os.contains(&os) {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: it does not declare subject OS '{}'",
                self.name,
                self.source.display(),
                os.name()
            ));
        }
        file.integration_config(&self.name, &self.source, procedure, os)
            .map_err(|reason| {
                generic_error!(
                    "Cannot run case '{}' from {}: {}",
                    self.name,
                    self.source.display(),
                    reason
                )
            })
    }

    /// Builds a runner from a copy of the case data, applying image choices to that copy.
    ///
    /// # Errors
    ///
    /// Participant cases require a recognized subject OS runtime and propagate [`Self::integration_config`] errors.
    pub(super) fn build_test(
        &self, integration_runtime: &str, settings: &IntegrationSettings, overrides: &ImageOverrides<'_>,
    ) -> Result<Box<dyn Test>, GenericError> {
        match &self.schema {
            CaseSchema::Integration(config) => Ok(Box::new(IntegrationTestCase::new(config.clone(), settings))),
            CaseSchema::Correctness(config) => {
                let mut config = config.clone();
                overrides.apply_correctness(&mut config);
                Ok(Box::new(CorrectnessTestCase::new(self.name.clone(), config)))
            }
            CaseSchema::Participants { .. } => {
                let os = Os::from_runtime(integration_runtime).ok_or_else(|| {
                    generic_error!(
                        "Cannot run case '{}' from {}: runtime '{}' is not a subject OS",
                        self.name,
                        self.source.display(),
                        integration_runtime
                    )
                })?;
                Ok(Box::new(IntegrationTestCase::new(
                    self.integration_config(os)?,
                    settings,
                )))
            }
        }
    }
}

impl CaseConfig {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub(crate) fn suite(&self) -> TestSuite {
        self.suite
    }

    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
impl CaseConfig {
    /// Subject OSes used to check that inventory translation is OS-independent.
    pub(super) fn subject_os(&self) -> &BTreeSet<Os> {
        &self.subject_os
    }

    /// The parsed file data, for tests that inspect what the loader produced.
    pub(super) fn schema(&self) -> &CaseSchema {
        &self.schema
    }
}

/// Names a case after its directory. A declared name must equal the directory name.
fn case_name(declared: Option<&str>, directory: &str, source: &Path) -> Result<String, GenericError> {
    match declared {
        Some(name) if name != directory => Err(generic_error!(
            "case name '{}' in {} does not match its directory '{}'; rename the directory or the name so they agree",
            name,
            source.display(),
            directory
        )),
        Some(name) => Ok(name.to_string()),
        None => Ok(directory.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{any::Any, fs};

    use super::*;
    use crate::config::{
        build_runners, default_host_runtime,
        tests::{write_case, NO_PARTICIPANT_FIELDS, PARTICIPANT_FIELDS},
        Need,
    };
    use crate::steps::{
        file_contains::FileContains,
        log_contains::{LogContains, LogNotContains},
    };

    const CORRECTNESS_FIELDS: &str =
        "runtime: docker\nanalysis_mode: metrics\nbaseline: {image: 'base:file'}\ncomparison: {image: 'comp:file'}\n";

    #[test]
    fn integration_case_records_its_declared_name_description_timeout_and_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            "type: integration\nname: alpha\ndescription: A portable case\ntimeout: 7s\nruntimes: [linux, mac]\nprocedure: []\n",
        );
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "alpha");
        assert_eq!(case.description(), Some("A portable case"));
        assert_eq!(case.timeout(), Duration::from_secs(7));
        assert_eq!(case.suite(), TestSuite::Integration);
        assert_eq!(case.subject_os, BTreeSet::from([Os::Linux, Os::MacOs]));
        assert!(!case.disabled);
        let canonical = path.canonicalize().unwrap();
        assert_eq!(case.source(), canonical);
        assert_eq!(case.source.parent().unwrap(), canonical.parent().unwrap());
    }

    #[test]
    fn a_case_without_a_type_loads_as_a_participant_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            &format!(
                "description: A portable case\n{}",
                PARTICIPANT_FIELDS
                    .replace("1s", "7s")
                    .replace("[linux]", "[linux, macos]")
            ),
        );
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "alpha");
        assert_eq!(case.description(), Some("A portable case"));
        assert_eq!(case.timeout(), Duration::from_secs(7));
        assert_eq!(case.subject_os, BTreeSet::from([Os::Linux, Os::MacOs]));
        assert_eq!(case.suite(), TestSuite::Integration);
        assert!(!case.disabled);
        assert_eq!(case.source(), path.canonicalize().unwrap());
        let CaseSchema::Participants { file, .. } = case.schema() else {
            panic!("expected participant data");
        };
        assert_eq!(file.participants["adp"].participant_type, "agent-adp");
        assert!(file.procedure.is_empty());
    }

    #[test]
    fn load_content_names_the_case_after_the_given_directory() {
        let source = Path::new("alpha/config.yaml");
        let cases = CaseConfig::load_content(PARTICIPANT_FIELDS, "alpha", source).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name(), "alpha");
        assert_eq!(cases[0].source(), source);

        let error = CaseConfig::load_content(&format!("name: beta\n{PARTICIPANT_FIELDS}"), "alpha", source)
            .err()
            .expect("a declared name must match the given directory");
        assert_eq!(
            error.to_string(),
            "case name 'beta' in alpha/config.yaml does not match its directory 'alpha'; \
             rename the directory or the name so they agree"
        );
    }

    #[test]
    fn integration_translation_requires_a_participant_case() {
        for yaml in [
            "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n".to_string(),
            format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        ] {
            let cases = CaseConfig::load_content(&yaml, "alpha", Path::new("alpha/config.yaml")).unwrap();
            assert_eq!(cases.len(), 1);
            assert_eq!(
                cases[0].integration_config(Os::Linux).unwrap_err().to_string(),
                "Cannot run case 'alpha' from alpha/config.yaml: \
                 integration translation requires a case that declares participants"
            );
        }
    }

    #[test]
    fn a_participant_case_in_another_suite_cannot_build_an_integration_runner() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "alpha", PARTICIPANT_FIELDS);
        let mut cases = CaseConfig::load_file(&path).unwrap();
        cases[0].suite = TestSuite::Correctness;
        let error = build_runners(&cases, LINUX_RUNTIME, &[])
            .err()
            .expect("the case and runner suites must agree");
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to build test runners:\n  - Cannot run case 'alpha' from {}: \
                 the integration runner runs only integration-suite cases",
                path.canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn a_participant_case_stores_its_built_procedure() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            r#"timeout: 120s
subject_os: [linux]
participants:
  adp: {type: agent-adp, subject: true, config: {env: {DD_API_KEY: "0000"}}}
procedure:
  - step: log_contains
    on: adp
    pattern: "Initial configuration received."
    timeout: 30s
  - parallel:
      - step: log_not_contains
        on: adp
        process: adp
        pattern: "panic|PANIC"
        regex: true
        during: 10s
  - step: file_contains
    on: adp
    path: /var/log/datadog/agent-data-plane.log
    pattern: DATAPLANE
    timeout: 30s
"#,
        );
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let CaseSchema::Participants { file, procedure } = cases[0].schema() else {
            panic!("expected participant data");
        };
        assert_eq!(file.participants["adp"].config.env["DD_API_KEY"], "0000");
        assert_eq!(file.procedure.len(), 3);
        let [ProcedureNode::Step(log), ProcedureNode::Parallel(members), ProcedureNode::Step(file_step)] =
            procedure.as_slice()
        else {
            panic!("expected step, parallel, step");
        };
        assert_eq!(members.len(), 1);
        let log = (log.as_ref() as &dyn Any).downcast_ref::<LogContains>().unwrap();
        assert_eq!(log.on, "adp");
        assert_eq!(log.process, None);
        assert_eq!(log.pattern, "Initial configuration received.");
        assert!(!log.regex);
        assert_eq!(log.timeout.0, Duration::from_secs(30));
        let absent = (members[0].as_ref() as &dyn Any)
            .downcast_ref::<LogNotContains>()
            .unwrap();
        assert_eq!(absent.on, "adp");
        assert_eq!(absent.process.as_deref(), Some("adp"));
        assert_eq!(absent.pattern, "panic|PANIC");
        assert!(absent.regex);
        assert_eq!(absent.during.0, Duration::from_secs(10));
        let file_step = (file_step.as_ref() as &dyn Any).downcast_ref::<FileContains>().unwrap();
        assert_eq!(file_step.on, "adp");
        assert_eq!(file_step.path, "/var/log/datadog/agent-data-plane.log".parse().unwrap());
        assert_eq!(file_step.pattern, "DATAPLANE");
        assert!(!file_step.regex);
        assert_eq!(file_step.timeout.0, Duration::from_secs(30));
    }

    #[test]
    fn default_log_file_references_load_on_linux_and_windows_but_not_macos() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
        for oses in ["linux", "windows", "linux, windows"] {
            let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
            let cases = CaseConfig::load_file(&path).unwrap();
            let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                panic!("expected participant data")
            };
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step")
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("adp".into()),
                    Need::Fact("adp".into(), "default_log_file".into())
                ]
            );
        }
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", "[linux, macos, windows]"));
        let message = CaseConfig::load_file(&path)
            .err()
            .expect("macOS has no declared default log file")
            .to_string();
        assert_eq!(message, format!(
            "case 'alpha' in {}: step 1 'file_contains' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide on macos; it provides it on linux, windows",
            path.canonicalize().unwrap().display()
        ));
    }

    #[test]
    fn participant_translation_requires_a_declared_subject_os() {
        let cases = CaseConfig::load_content(PARTICIPANT_FIELDS, "alpha", Path::new("alpha/config.yaml")).unwrap();
        assert_eq!(
            cases[0].integration_config(Os::Windows).unwrap_err().to_string(),
            "Cannot run case 'alpha' from alpha/config.yaml: it does not declare subject OS 'windows'"
        );
        let message = build_runners(&cases, WINDOWS_RUNTIME, &[])
            .err()
            .expect("runner construction must translate for the requested OS")
            .to_string();
        assert_eq!(message, "Failed to build test runners:\n  - Cannot run case 'alpha' from alpha/config.yaml: it does not declare subject OS 'windows'");
    }

    #[test]
    fn participant_runners_require_a_subject_os_runtime() {
        let cases = CaseConfig::load_content(PARTICIPANT_FIELDS, "alpha", Path::new("alpha/config.yaml")).unwrap();
        for runtime in ["docker", "macos", "unknown"] {
            let message = build_runners(&cases, runtime, &[])
                .err()
                .expect("invalid subject OS runtime")
                .to_string();
            assert_eq!(message, format!("Failed to build test runners:\n  - Cannot run case 'alpha' from alpha/config.yaml: runtime '{runtime}' is not a subject OS"));
        }
    }

    #[test]
    fn agent_adp_log_steps_may_name_the_adp_and_agent_processes_only() {
        let dir = tempfile::tempdir().unwrap();
        for (step, duration) in [("log_contains", "timeout"), ("log_not_contains", "during")] {
            for process in ["adp", "agent", "core"] {
                let yaml = PARTICIPANT_FIELDS.replace(
                    "procedure: []",
                    &format!(
                        "procedure: [{{step: {step}, on: adp, process: {process}, pattern: ready, {duration}: 1s}}]"
                    ),
                );
                let path = write_case(dir.path(), "alpha", &yaml);
                if process == "core" {
                    let error = CaseConfig::load_file(&path)
                        .err()
                        .expect("undeclared process must fail");
                    assert_eq!(
                        error.to_string(),
                        format!(
                            "case 'alpha' in {}: step 1 '{step}' names process 'core' of participant 'adp', which type 'agent-adp' does not run; it runs adp, agent",
                            path.canonicalize().unwrap().display()
                        )
                    );
                } else {
                    let cases = CaseConfig::load_file(&path).unwrap();
                    assert_eq!(cases.len(), 1);
                    let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                        panic!("expected participant data");
                    };
                    let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                        panic!("expected one built step");
                    };
                    assert_eq!(step.needs(), vec![Need::Process("adp".into(), process.into())]);
                }
            }
        }
    }

    #[test]
    fn participant_step_errors_name_the_canonical_file_and_step() {
        let dir = tempfile::tempdir().unwrap();
        for (fields, expected) in [
            (
                "step: wait_ready",
                "names unknown step 'wait_ready'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: adp_cli, on: adp, args: [status]",
                "names unknown step 'adp_cli'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: port_listening, on: adp, port: 58125, protocol: udp, timeout: 1s",
                "names unknown step 'port_listening'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: null, timeout: 1s",
                "'file_contains': pattern: invalid type: null, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 1s, unknown: x",
                "'log_contains': unknown: unknown field `unknown`",
            ),
            ("step: log_contains, on: adp, timeout: 1s", "missing field `pattern`"),
            (
                "step: log_contains, on: adp, pattern: true, timeout: 1s",
                "'log_contains': pattern: invalid type: boolean `true`, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 30",
                "'log_contains': timeout: invalid type: integer `30`, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: '(', regex: true, timeout: 1s",
                "'log_contains': pattern '(' is not a valid regex",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: '{{ adp.facts.default_log_file }}', timeout: 1s",
                "'file_contains': field 'pattern' contains a template reference",
            ),
            (
                "step: file_contains, on: adp, path: '{{ self.facts.default_log_file }}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{ self.facts.default_log_file }}': a step has no 'self'; name the participant",
            ),
            (
                "step: file_contains, on: adp, path: '{{ adp.endpoints.api.url }}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{ adp.endpoints.api.url }}' is not a fact reference",
            ),
            (
                "step: file_contains, on: adp, path: '{{PANORAMIC_DYNAMIC_PATH}}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{PANORAMIC_DYNAMIC_PATH}}' is not a fact reference",
            ),
            (
                "step: log_contains, on: missing, pattern: ready, timeout: 1s",
                "'log_contains' runs on unknown participant 'missing'; participants: adp",
            ),
            (
                "step: log_contains, on: agent, pattern: ready, timeout: 1s",
                "'log_contains' runs on unknown participant 'agent'; participants: adp",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 1s, stream: stdout",
                "'log_contains': stream: unknown field `stream`",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: ready, timeout: 1s, process: adp",
                "unknown field `process`",
            ),
        ] {
            let path = write_case(
                dir.path(),
                "alpha",
                &PARTICIPANT_FIELDS.replace("procedure: []", &format!("procedure: [{{{fields}}}]")),
            );
            let error = CaseConfig::load_file(&path).err().expect("invalid step must fail");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!(
                    "case 'alpha' in {}: step 1 ",
                    path.canonicalize().unwrap().display()
                )),
                "{message}"
            );
            assert!(message.contains(expected), "expected {expected}: {message}");
        }
    }

    #[test]
    fn an_unknown_field_in_a_parallel_step_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace(
            "procedure: []",
            "procedure: [{parallel: [{step: log_contains, on: adp, pattern: ready, timeout: 1s, unknown: x}]}]",
        );
        let path = write_case(dir.path(), "alpha", &yaml);
        let message = CaseConfig::load_file(&path)
            .err()
            .expect("unknown parallel step field must fail")
            .to_string();
        assert!(
            message.starts_with(&format!(
                "case 'alpha' in {}: step 1.1 ",
                path.canonicalize().unwrap().display()
            )),
            "{message}"
        );
        assert!(message.contains("unknown field `unknown`"), "{message}");
    }

    #[test]
    fn agent_adp_participants_accept_environment_variables_and_os_overlays_only() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace(
            "subject: true",
            "subject: true, config: {env: {ANY_KEY: arbitrary}, os: {linux: {env: {ANY_KEY: linux}}}}",
        );
        let path = write_case(dir.path(), "alpha", &yaml);
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let CaseSchema::Participants { file, .. } = cases[0].schema() else {
            panic!("expected participant data");
        };
        let config = &file.participants["adp"].config;
        assert_eq!(config.env["ANY_KEY"], "arbitrary");
        assert_eq!(config.os[&Os::Linux].env["ANY_KEY"], "linux");

        for (fields, expected) in [
            ("config: {files: {settings.yaml: missing.yaml}}", "it declares none"),
            ("config: {endpoints: {api: {port: 80}}}", "it serves none"),
            ("config: {params: {mode: fast}}", "it accepts none"),
            ("connect: {upstream: other.api}", "it consumes none"),
            (
                "config: {os: {linux: {files: {settings.yaml: missing.yaml}}}}",
                "it declares none",
            ),
            ("config: {os: {linux: {params: {mode: fast}}}}", "it accepts none"),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("subject: true", &format!("subject: true, {fields}"));
            let path = write_case(dir.path(), "alpha", &yaml);
            let error = CaseConfig::load_file(&path).err().expect("undeclared names must fail");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("case 'alpha' in {}: ", path.canonicalize().unwrap().display())),
                "{message}"
            );
            assert!(message.contains("which type 'agent-adp' does not"), "{message}");
            assert!(message.ends_with(expected), "{message}");
        }
    }

    #[test]
    fn a_participant_type_outside_the_registry_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            &PARTICIPANT_FIELDS.replace("agent-adp", "datadog-intake"),
        );
        let error = CaseConfig::load_file(&path).err().expect("unregistered type must fail");
        let message = error.to_string();
        assert!(
            message.contains("participant 'adp' has unknown type 'datadog-intake'; registered types: agent-adp"),
            "{message}"
        );
        assert!(
            message.starts_with(&format!("case 'alpha' in {}: ", path.canonicalize().unwrap().display())),
            "{message}"
        );
        let path = write_case(dir.path(), "alpha", PARTICIPANT_FIELDS);
        assert_eq!(CaseConfig::load_file(&path).unwrap().len(), 1);
    }

    #[test]
    fn participant_cases_are_eligible_only_for_their_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            &PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]"),
        );
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        for (runtime, eligible) in [
            (LINUX_RUNTIME, true),
            (MAC_RUNTIME, false),
            (WINDOWS_RUNTIME, true),
            ("unknown", false),
        ] {
            assert_eq!(cases[0].is_eligible_for(runtime), eligible, "{runtime}");
        }
    }

    #[test]
    fn correctness_case_takes_its_name_from_the_directory_and_the_fixed_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "beta", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "beta");
        assert_eq!(case.description(), None);
        assert_eq!(case.timeout(), CORRECTNESS_TIMEOUT);
        assert_eq!(case.timeout(), Duration::from_mins(20));
        assert_eq!(case.suite(), TestSuite::Correctness);
        assert_eq!(case.subject_os, BTreeSet::from([Os::Linux]));
        assert!(!case.disabled);
    }

    #[test]
    fn matrix_file_yields_one_case_per_variant_in_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "matrix",
            &format!(
                "type: correctness_matrix\n{CORRECTNESS_FIELDS}\nvariants:\n  - name: second\n    env: {{VARIANT: second}}\n  - name: first\n    env: {{VARIANT: first}}\n"
            ),
        );
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(
            cases.iter().map(CaseConfig::name).collect::<Vec<_>>(),
            ["matrix/second", "matrix/first"]
        );
        let source = path.canonicalize().unwrap();
        let runners = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        for ((case, runner), variant) in cases.iter().zip(&runners).zip(["second", "first"]) {
            assert_eq!(case.source(), source);
            assert_eq!(case.timeout(), CORRECTNESS_TIMEOUT);
            assert_eq!(case.suite(), TestSuite::Correctness);
            assert_eq!(case.description(), None);
            assert_eq!(case.subject_os, BTreeSet::from([Os::Linux]));
            assert!(!case.disabled);
            assert_eq!(runner.name(), format!("matrix/{variant}"));
            assert_eq!(runner.case_path(), source.parent().unwrap());

            // Check the variant data build_test consumes; CorrectnessTestCase exposes its target environment.
            let CaseSchema::Correctness(config) = &case.schema else {
                panic!("expected correctness data");
            };
            let runner = CorrectnessTestCase::new(case.name().to_string(), config.clone());
            for target in [&runner.config.baseline, &runner.config.comparison] {
                let (target, _) = runner.target_config(target).unwrap();
                assert_eq!(target.additional_env_vars, [format!("VARIANT={variant}")]);
            }
        }
    }

    #[test]
    fn disabled_correctness_cases_load_but_are_never_eligible() {
        let dir = tempfile::tempdir().unwrap();
        for (kind, variants, names) in [
            ("correctness", "", vec!["disabled"]),
            (
                "correctness_matrix",
                "variants: [{name: second}, {name: first}]\n",
                vec!["disabled/second", "disabled/first"],
            ),
        ] {
            let path = write_case(
                dir.path(),
                "disabled",
                &format!("type: {kind}\n{CORRECTNESS_FIELDS}disabled: true\n{variants}"),
            );
            let cases = CaseConfig::load_file(&path).unwrap();
            assert_eq!(cases.iter().map(CaseConfig::name).collect::<Vec<_>>(), names);
            for case in cases {
                assert!(case.disabled);
                for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME, "unknown"] {
                    assert!(!case.is_eligible_for(runtime), "{kind} under {runtime}");
                }
            }
        }
    }

    #[test]
    fn eligibility_scopes_integration_cases_to_their_runtimes_and_not_correctness_cases() {
        let dir = tempfile::tempdir().unwrap();
        let integration = write_case(
            dir.path(),
            "portable",
            "type: integration\nname: portable\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        let correctness = write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let integration = CaseConfig::load_file(&integration).unwrap();
        let correctness = CaseConfig::load_file(&correctness).unwrap();
        assert_eq!(integration.len(), 1);
        assert_eq!(correctness.len(), 1);
        assert_eq!(integration[0].subject_os, BTreeSet::from([Os::Linux, Os::Windows]));
        for (runtime, eligible) in [
            (LINUX_RUNTIME, true),
            (MAC_RUNTIME, false),
            (WINDOWS_RUNTIME, true),
            ("unknown", false),
        ] {
            assert_eq!(integration[0].is_eligible_for(runtime), eligible, "{runtime}");
            assert!(correctness[0].is_eligible_for(runtime), "{runtime}");
        }
    }

    #[test]
    fn load_file_rejects_unknown_and_non_string_types_and_empty_matrices() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, expected) in [
            ("type: 3\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: null\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: true\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: []\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: {}\n".to_string(), vec!["'type' in", "must be a string"]),
            (
                "type: bogus\n".to_string(),
                vec!["Unknown test type 'bogus'", "has no top-level 'type'"],
            ),
            (
                format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: []\n"),
                vec!["has no variants defined"],
            ),
        ] {
            let path = write_case(dir.path(), "invalid", &yaml);
            let error = CaseConfig::load_file(&path).err().expect("invalid case must fail");
            for expected in expected {
                assert!(error.to_string().contains(expected), "{error}");
            }
        }
    }

    #[test]
    fn every_load_error_names_the_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, reason) in [
            ("type: [".to_string(), "Failed to parse config file as YAML"),
            ("type: 3\n".to_string(), "must be a string"),
            (
                format!("{PARTICIPANT_FIELDS}unknown: true\n"),
                "unknown field `unknown`",
            ),
            (
                format!("{PARTICIPANT_FIELDS}container: {{files: ['settings.yaml:/etc/settings.yaml']}}\n"),
                "unknown field `container`",
            ),
            (
                format!("{PARTICIPANT_FIELDS}intake: {{enabled: true}}\n"),
                "unknown field `intake`",
            ),
            (
                PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{action: adp_cli, args: [status]}]"),
                "a procedure node must have 'step' or 'parallel'",
            ),
            (PARTICIPANT_FIELDS.replace("1s", "1nope"), "unknown duration unit"),
            (PARTICIPANT_FIELDS.replace("[linux]", "[]"), "empty subject_os list"),
            (
                PARTICIPANT_FIELDS.replace("[linux]", "[linux, linux]"),
                "lists subject OS 'linux' more than once",
            ),
            (NO_PARTICIPANT_FIELDS.to_string(), "declares no participants"),
            (
                format!("name: other\n{PARTICIPANT_FIELDS}"),
                "does not match its directory",
            ),
            ("type: bogus\n".to_string(), "Unknown test type 'bogus'"),
            (
                "type: integration\nname: alpha\ntimeout: nope\nprocedure: []\n".to_string(),
                "Failed to parse configuration file",
            ),
            (
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: []\nprocedure: []\n".to_string(),
                "empty runtimes list",
            ),
            (
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [typo]\nprocedure: []\n".to_string(),
                "unknown runtime 'typo'",
            ),
            (
                format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: []\n"),
                "has no variants defined",
            ),
            (
                "type: integration\nname: mismatch\ntimeout: 1s\nprocedure: []\n".to_string(),
                "does not match its directory",
            ),
        ] {
            let path = write_case(dir.path(), "alpha", &yaml);
            let error = CaseConfig::load_file(&path).err().expect("invalid case must fail");
            let message = format!("{error:#}");
            assert!(message.contains(reason), "{message}");
            assert!(
                message.contains(&path.canonicalize().unwrap().display().to_string()),
                "{message}"
            );
        }
    }

    #[test]
    fn a_declared_case_name_must_match_its_directory_and_an_omitted_one_defaults_to_it() {
        let source = Path::new("cases/dir/config.yaml");
        assert_eq!(case_name(None, "dir", source).unwrap(), "dir");
        assert_eq!(case_name(Some("dir"), "dir", source).unwrap(), "dir");

        let message = case_name(Some("other"), "dir", source).unwrap_err().to_string();
        assert!(message.contains("case name 'other'"), "{message}");
        assert!(message.contains(&source.display().to_string()), "{message}");
        assert!(message.contains("does not match its directory 'dir'"), "{message}");
    }

    #[test]
    fn an_integration_name_must_match_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n";
        let path = write_case(dir.path(), "directory", yaml);
        let error = CaseConfig::load_file(&path).err().expect("mismatched name must fail");
        let message = error.to_string();
        assert!(message.contains("name 'alpha'"), "{message}");
        assert!(
            message.contains("does not match its directory 'directory'"),
            "{message}"
        );

        let path = write_case(dir.path(), "alpha", yaml);
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.iter().map(CaseConfig::name).collect::<Vec<_>>(), ["alpha"]);
    }

    #[test]
    fn a_case_file_without_a_named_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        )
        .unwrap();
        let path = dir.path().join("sub/../config.yaml");
        let error = CaseConfig::load_file(&path)
            .err()
            .expect("a parent ending in '..' has no directory name");
        assert_eq!(
            error.to_string(),
            format!(
                "Cannot name the case in {}: its directory has no name",
                path.canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn source_is_canonical_when_the_file_is_named_through_a_non_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "case", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = CaseConfig::load_file(&dir.path().join("case/../case/config.yaml")).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.source(), path.canonicalize().unwrap());
        assert_eq!(case.name(), "case");
        let runners = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        assert_eq!(runners[0].case_path(), path.canonicalize().unwrap().parent().unwrap());
    }

    #[test]
    fn image_overrides_change_the_built_runner_but_not_the_loaded_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "case", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = CaseConfig::load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let entries = ["baseline=base:override".parse().unwrap()];
        let overridden = build_runners(&cases, LINUX_RUNTIME, &entries).unwrap();
        let declared = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        assert_eq!(overridden[0].images()["baseline"], "base:override");
        assert_eq!(declared[0].images()["baseline"], "base:file");
    }

    #[test]
    fn facade_metadata_matches_the_built_runner_for_every_checked_in_case() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        for suite in ["integration", "correctness"] {
            let mut loaded = 0;
            let mut built = 0;
            for entry in fs::read_dir(root.join("test").join(suite).join("cases")).unwrap() {
                let dir = entry.unwrap().path();
                let path = dir.join("config.yaml");
                if !dir.is_dir() || !path.is_file() {
                    continue;
                }
                let cases = CaseConfig::load_file(&path).unwrap();
                assert!(!cases.is_empty(), "{}", path.display());
                loaded += cases.len();
                let yaml: serde_json::Value = serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                for case in cases {
                    if yaml["type"] == "integration" {
                        let declared_os: BTreeSet<_> = match yaml.get("runtimes") {
                            Some(runtimes) => runtimes
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|runtime| Os::from_runtime(runtime.as_str().unwrap()).unwrap())
                                .collect(),
                            None => BTreeSet::from([Os::from_runtime(default_host_runtime()).unwrap()]),
                        };
                        assert_eq!(case.subject_os, declared_os, "{}", path.display());
                    }
                    for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME] {
                        if !case.is_eligible_for(runtime) {
                            continue;
                        }
                        let runners = build_runners(std::slice::from_ref(&case), runtime, &[]).unwrap();
                        let runner = &runners[0];
                        assert_eq!(case.name(), runner.name(), "{} under {runtime}", path.display());
                        assert_eq!(case.suite(), runner.suite(), "{} under {runtime}", path.display());
                        assert_eq!(
                            case.source.parent().unwrap(),
                            runner.case_path(),
                            "{} under {runtime}",
                            path.display()
                        );
                        assert_eq!(case.timeout(), runner.timeout(), "{} under {runtime}", path.display());
                        built += 1;
                    }
                }
            }
            assert!(loaded > 0, "{suite} must contain cases");
            assert!(built > 0, "{suite} must contain eligible cases");
        }
    }

    #[test]
    fn disabled_correctness_cases_still_reject_invalid_environments() {
        let dir = tempfile::tempdir().unwrap();
        for (kind, fields, variants) in [
            (
                "correctness",
                CORRECTNESS_FIELDS.replace(
                    "baseline: {image: 'base:file'}",
                    "baseline: {image: 'base:file', env: {INVALID: true}}",
                ),
                "",
            ),
            (
                "correctness_matrix",
                CORRECTNESS_FIELDS.to_string(),
                "variants: [{name: invalid, env: {INVALID: true}}]\n",
            ),
        ] {
            let path = write_case(
                dir.path(),
                "invalid",
                &format!("type: {kind}\n{fields}disabled: true\n{variants}"),
            );
            let error = CaseConfig::load_file(&path)
                .err()
                .expect("disabled invalid case must fail");
            assert!(
                format!("{error:?}").contains("environment variable 'INVALID' must be a YAML string"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn load_file_reports_read_yaml_and_schema_failures() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing/config.yaml");
        let error = CaseConfig::load_file(&missing).err().expect("missing file must fail");
        assert_eq!(
            error.to_string(),
            format!("Failed to resolve configuration file: {}", missing.display())
        );
        let directory = dir.path().join("unreadable/config.yaml");
        fs::create_dir_all(&directory).unwrap();
        let error = CaseConfig::load_file(&directory)
            .err()
            .expect("a directory cannot be read as a file");
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to read config file: {}",
                directory.canonicalize().unwrap().display()
            )
        );
        for (yaml, context) in [
            ("type: [", "Failed to parse config file as YAML"),
            ("type: integration\n", "Failed to parse configuration file"),
        ] {
            let path = write_case(dir.path(), "invalid", yaml).canonicalize().unwrap();
            let error = CaseConfig::load_file(&path).err().expect("invalid file must fail");
            assert_eq!(error.to_string(), format!("{context}: {}", path.display()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn correctness_names_use_the_supplied_directory_even_when_it_is_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "original",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        std::os::unix::fs::symlink(dir.path().join("original"), dir.path().join("alias")).unwrap();
        let cases = CaseConfig::load_file(&dir.path().join("alias/config.yaml")).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name(), "alias");
        assert_eq!(cases[0].source(), path.canonicalize().unwrap());
    }
}

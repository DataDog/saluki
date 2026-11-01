//! Loads participant case files and hands typed files to a supplied loader.
//!
//! [`CaseConfig`] records each case's metadata and parsed data.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{de::DeserializeOwned, Deserialize};

mod duration;
mod file;
mod participant_check;
mod step_check;
mod template;

#[cfg(test)]
mod fixtures;

pub use self::{
    duration::CaseDuration,
    file::{deserialize_env_map, CaseFile, ParticipantConfig, ParticipantEntry, ProcedureNode, StepNode},
    participant_check::{FactDeclaration, ParticipantType},
    step_check::{decode_fields, Need, Step, StepFactory, StepFields},
    template::Template,
};
use self::{participant_check::check_participants, step_check::build_steps};

/// An operating system a case's processes run on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    /// Linux.
    Linux,
    /// macOS.
    #[allow(
        clippy::enum_variant_names,
        reason = "macOS is one word; the Os suffix is part of its name."
    )]
    MacOs,
    /// Windows.
    Windows,
}

impl Os {
    /// Returns the OS spelling used in case files.
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
        }
    }
}

/// The participant types and steps a case file may name.
#[derive(Clone, Copy)]
pub struct Registries<'a> {
    /// Participant types registered as [`ParticipantType`] values, available to case files.
    pub participant_types: &'a [&'a dyn ParticipantType],
    /// Steps available to case procedures.
    pub steps: &'a [&'a dyn StepFactory],
}

/// Loads case files that select their schema with a top-level `type` key.
pub trait TypedCaseLoader {
    /// Parsed data of one typed case.
    type Config;

    /// Loads every case `content` declares. `case_type` is the file's `type` value as written,
    /// which need not be a string.
    fn load(
        &self, case_type: &serde_json::Value, content: &str, directory: &str, source: &Path,
    ) -> Result<Vec<TypedCase<Self::Config>>, GenericError>;
}

/// One case a typed loader loaded, with the metadata [`CaseConfig`] records for it.
pub struct TypedCase<C> {
    /// Case name used for selection and reporting.
    pub name: String,
    /// Optional reporting description.
    pub description: Option<String>,
    /// Suite used to group the case in reports.
    pub suite: String,
    /// Maximum execution time for the case.
    pub timeout: Duration,
    /// Operating systems the subject supports.
    pub subject_os: BTreeSet<Os>,
    /// Whether the case is excluded from execution.
    pub disabled: bool,
    /// Parsed data supplied by the typed loader.
    pub config: C,
}

/// Loads and discovers cases with fixed registries and typed schemas.
pub struct CaseLoader<'a, L> {
    registries: Registries<'a>,
    typed: L,
    participant_suite: &'a str,
}

impl<'a, L> CaseLoader<'a, L> {
    /// Creates a loader with registries, a typed loader and the suite assigned to participant cases.
    pub const fn new(registries: Registries<'a>, typed: L, participant_suite: &'a str) -> Self {
        Self {
            registries,
            typed,
            participant_suite,
        }
    }
}

impl<L> CaseLoader<'_, L>
where
    L: TypedCaseLoader,
{
    /// Discovers every case in direct subdirectories, sorted by name and source, including disabled cases.
    ///
    /// A `config.yaml` declares participants or selects a schema through the typed loader.
    /// A `config.yml` is rejected even alongside a `config.yaml`; discovery loads the latter to report every problem.
    /// A `config.yaml` entry that cannot be resolved or read is a load error, including a dangling symlink.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory is missing or unreadable, a case cannot be loaded,
    /// a directory contains `config.yml`, or multiple cases share a name.
    pub fn discover_cases(&self, dirs: &[PathBuf]) -> Result<Vec<CaseConfig<L::Config>>, GenericError> {
        let mut cases = Vec::new();
        let mut load_errors = Vec::new();

        for base_path in dirs {
            if !base_path.is_dir() {
                return Err(generic_error!("Test directory does not exist: {}", base_path.display()));
            }

            let entries = std::fs::read_dir(base_path)
                .error_context(format!("Failed to read test directory: {}", base_path.display()))?;

            for entry in entries {
                let entry = entry.error_context("Failed to read directory entry")?;
                let path = entry.path();

                if path.is_dir() {
                    let config_path = path.join("config.yaml");
                    if std::fs::symlink_metadata(&config_path).is_ok() {
                        match self.load_file(&config_path) {
                            Ok(loaded) => cases.extend(loaded),
                            // Continue loading so one report names every problem. Reject the whole list on error:
                            // a dropped case could make an incomplete run look like a passing run.
                            Err(e) => load_errors.push((config_path, e)),
                        }
                    }
                    // Treat config.yml as an error so a misspelled file cannot silently drop a case from every run.
                    let stray_path = path.join("config.yml");
                    if std::fs::symlink_metadata(&stray_path).is_ok() {
                        let source = stray_path.canonicalize().unwrap_or_else(|_| stray_path.clone());
                        load_errors.push((
                            stray_path,
                            generic_error!(
                                "Case file {} is named config.yml; panoramic loads only config.yaml, so rename it",
                                source.display()
                            ),
                        ));
                    }
                }
            }
        }

        load_errors.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut problems: Vec<_> = load_errors.into_iter().map(|(_, error)| format!("{error:#}")).collect();
        cases.sort_by(|a, b| (a.name(), a.source()).cmp(&(b.name(), b.source())));
        for group in cases.chunk_by(|a, b| a.name() == b.name()) {
            if group.len() > 1 {
                let sources: Vec<_> = group.iter().map(|case| case.source().display().to_string()).collect();
                problems.push(format!(
                    "Case name '{}' is declared more than once: {}",
                    group[0].name(),
                    sources.join(", ")
                ));
            }
        }
        if !problems.is_empty() {
            let problems: Vec<_> = problems
                .into_iter()
                .map(|problem| problem.replace('\n', "\n    "))
                .collect();
            return Err(generic_error!(
                "Failed to load test cases:\n  - {}",
                problems.join("\n  - ")
            ));
        }

        Ok(cases)
    }

    /// Loads every case a file declares, including disabled cases.
    ///
    /// # Errors
    ///
    /// Returns path resolution and read errors, an error if the directory has no name,
    /// and every error of [`Self::load_content`].
    pub fn load_file(&self, config_path: &Path) -> Result<Vec<CaseConfig<L::Config>>, GenericError> {
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

        self.load_content(&content, dir_name, config_path)
    }

    /// Loads `content` as the `config.yaml` recorded at `source`, in directory `directory`.
    ///
    /// Files without a top-level `type` declare participants; other files go to the typed loader.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid YAML or a typed file that declares no cases. Propagates typed-loader errors unchanged.
    /// A declared name must match the directory. Participant cases require at least one participant
    /// and a nonempty, unique `subject_os` list using `linux`, `macos` or `windows`.
    /// Participant names must be DNS labels starting with a letter, other than `self`;
    /// exactly one subject is required, without an `os` override.
    /// Types and steps must be registered, their fields valid, and their references declared.
    /// Connections must join a consumed endpoint to another participant's served endpoint.
    /// OS overlays must be reachable, and referenced facts must exist on every possible participant OS.
    pub fn load_content(
        &self, content: &str, directory: &str, source: &Path,
    ) -> Result<Vec<CaseConfig<L::Config>>, GenericError> {
        let peek: serde_json::Value = serde_saphyr::from_str(content)
            .error_context(format!("Failed to parse config file as YAML: {}", source.display()))?;
        let Some(case_type) = peek.get("type") else {
            let file: CaseFile = deserialize_case(content, source)?;
            let name = case_name(file.name.as_deref(), directory, source)?;
            let subject_os = file.check(&name, source)?;
            check_participants(&file, &subject_os, self.registries.participant_types, &name, source)?;
            let procedure = build_steps(
                &file,
                &subject_os,
                self.registries.steps,
                self.registries.participant_types,
                &name,
                source,
            )?;
            return Ok(vec![CaseConfig {
                name,
                description: file.description.clone(),
                // TODO: derive the suite from the case's directory.
                suite: self.participant_suite.to_string(),
                source: source.to_path_buf(),
                timeout: file.timeout.into(),
                subject_os,
                disabled: false,
                schema: CaseSchema::Participants { file, procedure },
            }]);
        };
        let cases = self.typed.load(case_type, content, directory, source)?;
        if cases.is_empty() {
            return Err(generic_error!("{} declares no cases", source.display()));
        }
        Ok(cases
            .into_iter()
            .map(|case| CaseConfig {
                name: case.name,
                description: case.description,
                suite: case.suite,
                source: source.to_path_buf(),
                timeout: case.timeout,
                subject_os: case.subject_os,
                disabled: case.disabled,
                schema: CaseSchema::Typed(case.config),
            })
            .collect())
    }
}

/// Immutable case metadata and parsed data.
pub struct CaseConfig<C> {
    name: String,
    description: Option<String>,
    suite: String,
    source: PathBuf,
    timeout: Duration,
    subject_os: BTreeSet<Os>,
    disabled: bool,
    schema: CaseSchema<C>,
}

/// A participant case or data supplied by the typed loader.
pub enum CaseSchema<C> {
    /// A participant case with its validated procedure.
    Participants {
        /// Parsed case file, including the original procedure fields.
        file: CaseFile,
        /// Steps built from their registered declarations.
        procedure: Vec<ProcedureNode<Box<dyn Step>>>,
    },
    /// Parsed data supplied by the typed loader.
    Typed(C),
}

impl<C> CaseConfig<C> {
    /// Returns the case name used for selection and reporting.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the optional reporting description.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the suite used to group the case in reports.
    pub fn suite(&self) -> &str {
        &self.suite
    }

    /// Returns the case file path recorded during loading.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Returns the maximum execution time for the case.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the operating systems the subject supports.
    pub fn subject_os(&self) -> &BTreeSet<Os> {
        &self.subject_os
    }

    /// Returns whether the case is excluded from execution.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Returns the parsed case data and built procedure.
    pub fn schema(&self) -> &CaseSchema<C> {
        &self.schema
    }
}

/// Deserializes case data and includes its source path in parse errors.
pub fn deserialize_case<T>(content: &str, source: &Path) -> Result<T, GenericError>
where
    T: DeserializeOwned,
{
    serde_saphyr::from_str(content).error_context(format!("Failed to parse configuration file: {}", source.display()))
}

/// Names a case after its directory. A declared name must equal the directory name.
pub fn case_name(declared: Option<&str>, directory: &str, source: &Path) -> Result<String, GenericError> {
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
    use std::fs;

    use super::fixtures::{write_case, NO_PARTICIPANT_FIELDS, PARTICIPANT_FIELDS};
    use super::*;

    #[test]
    fn a_file_with_a_top_level_type_is_handed_to_the_typed_loader_with_its_text_directory_and_source() {
        let content = "# Preserve the text, including comments.\ntype: fixture\nvalue: 'true'\n";
        let source = Path::new("original/config.yaml");
        let cases = fixtures::LOADER.load_content(content, "alias", source).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "alias");
        assert_eq!(case.description(), Some("alias fixture"));
        assert_eq!(case.suite(), "fixture-typed");
        assert_eq!(case.source(), source);
        assert_eq!(case.timeout(), Duration::from_secs(3));
        assert_eq!(*case.subject_os(), BTreeSet::from([Os::MacOs, Os::Windows]));
        assert!(case.is_disabled());
        let CaseSchema::Typed(config) = case.schema() else {
            panic!("expected typed data");
        };
        assert_eq!(config.case_type, serde_json::json!("fixture"));
        assert_eq!(config.content, content);
        assert_eq!(config.directory, "alias");
        assert_eq!(config.source, source);
    }

    #[test]
    fn a_non_string_type_is_handed_to_the_typed_loader_as_written() {
        for (yaml, received) in [
            ("null", serde_json::json!(null)),
            ("true", serde_json::json!(true)),
            ("3", serde_json::json!(3)),
            ("[fixture, 3]", serde_json::json!(["fixture", 3])),
            ("{schema: fixture}", serde_json::json!({"schema": "fixture"})),
        ] {
            let error = fixtures::LOADER
                .load_content(&format!("type: {yaml}\n"), "alpha", Path::new("config.yaml"))
                .err()
                .expect("the fixture loader rejects non-string types");
            assert_eq!(error.to_string(), format!("Unsupported fixture type: {received}"));
        }
    }

    #[test]
    fn typed_loader_errors_are_returned_unchanged() {
        let content = "type: unknown\n";
        let source = Path::new("alpha/config.yaml");
        let expected = fixtures::FixtureTypes
            .load(&serde_json::json!("unknown"), content, "alpha", source)
            .err()
            .expect("the fixture loader rejects unknown types");
        let error = fixtures::LOADER
            .load_content(content, "alpha", source)
            .err()
            .expect("the case loader propagates typed errors");
        assert_eq!(error.to_string(), "Unsupported fixture type: \"unknown\"");
        assert_eq!(format!("{error:#}"), format!("{expected:#}"));
    }

    #[test]
    fn a_typed_file_that_declares_no_cases_fails_to_load_and_discover() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "empty", "type: empty\n");
        write_case(dir.path(), "participant", PARTICIPANT_FIELDS);
        let expected = format!("{} declares no cases", path.canonicalize().unwrap().display());
        let error = fixtures::LOADER
            .load_file(&path)
            .err()
            .expect("a case file must produce at least one case");
        assert_eq!(error.to_string(), expected);
        let error = fixtures::LOADER
            .discover_cases(&[dir.path().to_path_buf()])
            .err()
            .expect("discovery must report an empty file alongside valid cases");
        assert_eq!(error.to_string(), format!("Failed to load test cases:\n  - {expected}"));
    }

    #[test]
    fn participant_types_and_steps_come_from_the_supplied_registries() {
        let source = Path::new("alpha/config.yaml");
        let error = fixtures::LOADER
            .load_content(&PARTICIPANT_FIELDS.replace("server", "agent-adp"), "alpha", source)
            .err()
            .expect("only the supplied participant types are accepted");
        assert_eq!(error.to_string(), "case 'alpha' in alpha/config.yaml: participant 'subject' has unknown type 'agent-adp'; registered types: client, server");

        let content = PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{step: probe, on: subject}]");
        let cases = fixtures::LOADER.load_content(&content, "alpha", source).unwrap();
        let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
            panic!("expected participant data");
        };
        let [ProcedureNode::Step(step)] = procedure.as_slice() else {
            panic!("expected one built step");
        };
        let expected = fixtures::Probe {
            on: "subject".into(),
            process: None,
            values: vec![],
            path: None,
        };
        assert_eq!(format!("{step:?}"), format!("{expected:?}"));

        let loader = CaseLoader::new(
            Registries {
                steps: &[],
                ..fixtures::REGISTRIES
            },
            fixtures::FixtureTypes,
            "fixture-participants",
        );
        let error = loader
            .load_content(&content, "alpha", source)
            .err()
            .expect("only the supplied steps are accepted");
        assert_eq!(
            error.to_string(),
            "case 'alpha' in alpha/config.yaml: step 1 names unknown step 'probe'; registered steps: none"
        );
    }

    #[test]
    fn discovery_sorts_typed_and_participant_cases_together_and_rejects_a_name_they_share() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let typed = write_case(&first, "alpha", "type: fixture\n");
        write_case(&first, "gamma", "type: fixture\n");
        write_case(&second, "beta", PARTICIPANT_FIELDS);
        for roots in [[first.clone(), second.clone()], [second.clone(), first.clone()]] {
            let cases = fixtures::LOADER.discover_cases(&roots).unwrap();
            assert_eq!(
                cases.iter().map(CaseConfig::name).collect::<Vec<_>>(),
                ["alpha", "beta", "gamma"]
            );
            assert!(matches!(cases[0].schema(), CaseSchema::Typed(_)));
            assert!(matches!(cases[1].schema(), CaseSchema::Participants { .. }));
            assert!(matches!(cases[2].schema(), CaseSchema::Typed(_)));
        }
        let participant = write_case(&second, "alpha", PARTICIPANT_FIELDS);
        let expected = format!(
            "Failed to load test cases:\n  - Case name 'alpha' is declared more than once: {}, {}",
            typed.canonicalize().unwrap().display(),
            participant.canonicalize().unwrap().display()
        );
        for roots in [[first.clone(), second.clone()], [second, first]] {
            let error = fixtures::LOADER
                .discover_cases(&roots)
                .err()
                .expect("names must be unique across schemas");
            assert_eq!(error.to_string(), expected);
        }
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
        let cases = fixtures::LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "alpha");
        assert_eq!(case.description(), Some("A portable case"));
        assert_eq!(case.timeout(), Duration::from_secs(7));
        assert_eq!(*case.subject_os(), BTreeSet::from([Os::Linux, Os::MacOs]));
        assert_eq!(case.suite(), "fixture-participants");
        assert!(!case.is_disabled());
        assert_eq!(case.source(), path.canonicalize().unwrap());
        let CaseSchema::Participants { file, .. } = case.schema() else {
            panic!("expected participant data");
        };
        assert_eq!(file.participants["subject"].participant_type, "server");
        assert!(file.procedure.is_empty());
    }

    #[test]
    fn a_participant_case_reports_the_suite_its_loader_names() {
        let suite = String::from("another-suite");
        let other = CaseLoader::new(fixtures::REGISTRIES, fixtures::FixtureTypes, &suite);
        for (loader, suite) in [(&fixtures::LOADER, "fixture-participants"), (&other, "another-suite")] {
            let cases = loader
                .load_content(PARTICIPANT_FIELDS, "alpha", Path::new("alpha/config.yaml"))
                .unwrap();
            assert_eq!(cases.len(), 1);
            assert_eq!(cases[0].suite(), suite);
        }
    }

    #[test]
    fn load_content_names_the_case_after_the_given_directory() {
        let source = Path::new("alpha/config.yaml");
        let cases = fixtures::LOADER
            .load_content(PARTICIPANT_FIELDS, "alpha", source)
            .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name(), "alpha");
        assert_eq!(cases[0].source(), source);

        let error = fixtures::LOADER
            .load_content(&format!("name: beta\n{PARTICIPANT_FIELDS}"), "alpha", source)
            .err()
            .expect("a declared name must match the given directory");
        assert_eq!(
            error.to_string(),
            "case name 'beta' in alpha/config.yaml does not match its directory 'alpha'; \
             rename the directory or the name so they agree"
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
  subject: {type: server, subject: true, config: {env: {KEY: "0000"}}}
procedure:
  - step: probe
    on: subject
    values: [first]
  - parallel:
      - step: marker
        on: subject
  - step: probe
    on: subject
    process: worker
    path: '{{ subject.facts.file }}'
    values: [last]
"#,
        );
        let cases = fixtures::LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let CaseSchema::Participants { file, procedure } = cases[0].schema() else {
            panic!("expected participant data");
        };
        assert_eq!(file.participants["subject"].config.env["KEY"], "0000");
        assert_eq!(file.procedure.len(), 3);
        let [ProcedureNode::Step(first), ProcedureNode::Parallel(members), ProcedureNode::Step(last)] =
            procedure.as_slice()
        else {
            panic!("expected step, parallel, step");
        };
        assert_eq!(members.len(), 1);
        let marker = fixtures::Marker { on: "subject".into() };
        assert_eq!(format!("{:?}", members[0]), format!("{marker:?}"));
        for (step, marker, process, path) in [
            (first, "first", None, None),
            (last, "last", Some("worker"), Some("{{ subject.facts.file }}")),
        ] {
            let expected = fixtures::Probe {
                on: "subject".into(),
                process: process.map(String::from),
                values: vec![serde_json::json!(marker)],
                path: path.map(|path| path.parse().unwrap()),
            };
            assert_eq!(format!("{step:?}"), format!("{expected:?}"));
        }
    }

    #[test]
    fn an_unknown_field_in_a_parallel_step_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace(
            "procedure: []",
            "procedure: [{parallel: [{step: probe, on: subject, unknown: x}]}]",
        );
        let path = write_case(dir.path(), "alpha", &yaml);
        let message = fixtures::LOADER
            .load_file(&path)
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
    fn a_participant_type_outside_the_registry_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "alpha", &PARTICIPANT_FIELDS.replace("server", "missing"));
        let error = fixtures::LOADER
            .load_file(&path)
            .err()
            .expect("unregistered type must fail");
        let message = error.to_string();
        assert!(
            message.contains("participant 'subject' has unknown type 'missing'; registered types: client, server"),
            "{message}"
        );
        assert!(
            message.starts_with(&format!("case 'alpha' in {}: ", path.canonicalize().unwrap().display())),
            "{message}"
        );
        let path = write_case(dir.path(), "alpha", PARTICIPANT_FIELDS);
        assert_eq!(fixtures::LOADER.load_file(&path).unwrap().len(), 1);
    }

    #[test]
    fn every_load_error_names_the_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, reason) in [
            (
                format!("{PARTICIPANT_FIELDS}unknown: true\n"),
                "unknown field `unknown`",
            ),
            (
                PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{action: probe}]"),
                "a procedure node must have 'step' or 'parallel'",
            ),
            (PARTICIPANT_FIELDS.replace("1s", "1nope"), "unknown duration unit"),
            (PARTICIPANT_FIELDS.replace("1s", "'1'"), "has no unit"),
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
        ] {
            let path = write_case(dir.path(), "alpha", &yaml);
            let error = fixtures::LOADER.load_file(&path).err().expect("invalid case must fail");
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
        let cases = fixtures::LOADER
            .load_content(PARTICIPANT_FIELDS, "dir", source)
            .unwrap();
        assert_eq!(cases[0].name(), "dir");
        let cases = fixtures::LOADER
            .load_content(&format!("name: dir\n{PARTICIPANT_FIELDS}"), "dir", source)
            .unwrap();
        assert_eq!(cases[0].name(), "dir");
        let message = fixtures::LOADER
            .load_content(&format!("name: other\n{PARTICIPANT_FIELDS}"), "dir", source)
            .err()
            .expect("a declared name must match its directory")
            .to_string();
        assert!(message.contains("case name 'other'"), "{message}");
        assert!(message.contains(&source.display().to_string()), "{message}");
        assert!(message.contains("does not match its directory 'dir'"), "{message}");
    }

    #[test]
    fn a_case_file_without_a_named_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("config.yaml"), "type: fixture\n").unwrap();
        let path = dir.path().join("sub/../config.yaml");
        let error = fixtures::LOADER
            .load_file(&path)
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
        let path = write_case(dir.path(), "case", "type: fixture\n");
        let cases = fixtures::LOADER
            .load_file(&dir.path().join("case/../case/config.yaml"))
            .unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.source(), path.canonicalize().unwrap());
        assert_eq!(case.name(), "case");
    }

    #[test]
    fn load_file_reports_read_yaml_and_schema_failures() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing/config.yaml");
        let error = fixtures::LOADER
            .load_file(&missing)
            .err()
            .expect("missing file must fail");
        assert_eq!(
            error.to_string(),
            format!("Failed to resolve configuration file: {}", missing.display())
        );
        let directory = dir.path().join("unreadable/config.yaml");
        fs::create_dir_all(&directory).unwrap();
        let error = fixtures::LOADER
            .load_file(&directory)
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
            ("timeout: 1s\n", "Failed to parse configuration file"),
        ] {
            let path = write_case(dir.path(), "invalid", yaml).canonicalize().unwrap();
            let error = fixtures::LOADER.load_file(&path).err().expect("invalid file must fail");
            assert_eq!(error.to_string(), format!("{context}: {}", path.display()));
        }
    }
}

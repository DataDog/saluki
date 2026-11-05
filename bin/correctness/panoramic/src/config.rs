use std::path::{Path, PathBuf};

use panoramic_core::case::CaseConfig;
use panoramic_core::{
    artifact::ArtifactManifest,
    layout::{Os, RealizationKind},
    plan::{CasePlan, Compiled, Planner},
};
use saluki_error::{generic_error, GenericError};

/// Directory containing the case's source file.
pub(crate) fn case_dir(case: &CaseConfig) -> PathBuf {
    case.source().parent().unwrap_or(Path::new("")).to_path_buf()
}

/// Whether any planned participant requires a Kubernetes pod.
pub(crate) fn needs_pod(plan: &CasePlan) -> bool {
    plan.layout()
        .iter()
        .any(|(_, realization)| realization.kind == RealizationKind::KubernetesPod)
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

/// Discovered cases partitioned by the selected subject OS, with each list sorted by name.
pub(crate) struct Discovered {
    /// Cases that declare the selected subject OS, planned for it.
    pub(crate) plans: Vec<CasePlan>,
    /// Cases whose `subject_os` omits the selected OS and whose compilation checks passed.
    pub(crate) excluded: Vec<CaseConfig>,
}

/// Discovers and compiles cases, sorted by name, using the caller's run planner.
pub(crate) fn discover_in_scope(
    planner: &mut Planner<'_>, dirs: &[PathBuf], subject_os: Os,
) -> Result<Discovered, GenericError> {
    let cases = crate::CASE_LOADER.discover_cases(dirs)?;
    let mut plans = Vec::new();
    let mut excluded = Vec::new();
    let mut errors = Vec::new();
    for case in cases {
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

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use panoramic_core::{
        artifact::ArtifactRole,
        case::{CaseLoader, Registries},
        endpoint::{Endpoint, Port},
        layout::Realization,
        participant::{EffectiveConfig, ParticipantType},
    };
    use panoramic_participants::AgentAdp;

    use super::*;

    #[test]
    fn planning_rejects_udp_listeners_with_the_case_step_and_endpoint() {
        // UDP addressability cannot prove a listener exists; reject the need before the step can report a pass.
        struct Server(Endpoint);
        impl ParticipantType for Server {
            fn name(&self) -> &'static str {
                "server"
            }
            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[(Os::Linux, RealizationKind::LinuxContainer)]
            }
            fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
                &[]
            }
            fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
                Ok(BTreeMap::from([("input".into(), self.0.clone())]))
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }

        for (endpoint, supported) in [
            (Endpoint::udp(Port::Fixed(1001)), false),
            (Endpoint::tcp(Port::Fixed(1001)), true),
        ] {
            let server = Server(endpoint);
            let registries = Registries {
                participant_types: &[&server],
                steps: crate::REGISTRIES.steps,
            };
            for (procedure, label) in [
                (
                    "[{step: port_listening, on: subject, endpoint: input, timeout: 1s}]",
                    "1",
                ),
                (
                    "[{parallel: [{step: port_listening, on: subject, endpoint: input, timeout: 1s}]}]",
                    "1.1",
                ),
            ] {
                let yaml = format!(
                    "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: server, subject: true}}\nprocedure: {procedure}"
                );
                let case = CaseLoader::new(registries)
                    .load_content("cases", &yaml, "listener", Path::new("cases/listener/config.yaml"))
                    .unwrap();
                let result = Planner::new(registries, Default::default())
                    .unwrap()
                    .compile(case, Os::Linux);
                if supported {
                    assert!(result.unwrap().into_plan().is_some());
                } else {
                    assert_eq!(
                        result.err().unwrap().to_string(),
                        format!("case 'listener' in cases/listener/config.yaml: on subject OS 'linux', step {label} 'port_listening' reaches endpoint 'input' of participant 'subject', which the runner host cannot reach: UDP probes cannot confirm a listener; use an output or metric assertion instead")
                    );
                }
            }
        }
    }

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

    /// Creates a test planner with the binary's registries and local artifact defaults.
    pub(crate) fn planner() -> Planner<'static> {
        Planner::new(crate::REGISTRIES, ArtifactManifest::default()).unwrap()
    }

    /// Loads and compiles one case for tests.
    pub(crate) fn plan(yaml: &str, os: Os) -> CasePlan {
        let case = crate::CASE_LOADER
            .load_content(
                "integration",
                yaml,
                "case",
                Path::new("/test/integration/cases/case/config.yaml"),
            )
            .unwrap();
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
                (Os::Linux, RealizationKind::KubernetesPod),
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

    pub(crate) fn plan_with_plain(yaml: &str, name: &str, os: Os) -> CasePlan {
        let registries = Registries {
            participant_types: &[&AgentAdp, &Plain],
            ..crate::REGISTRIES
        };
        let source = format!("/test/integration/cases/{name}/config.yaml");
        let case = CaseLoader::new(registries)
            .load_content("integration", yaml, name, Path::new(&source))
            .unwrap();
        Planner::new(registries, ArtifactManifest::default())
            .unwrap()
            .compile(case, os)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[test]
    fn discovery_reports_excluded_cases_in_name_order_beside_the_plans() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "windows-only",
            &PARTICIPANT_FIELDS.replace("[linux]", "[windows]"),
        );
        write_case(dir.path(), "second", PARTICIPANT_FIELDS);
        write_case(dir.path(), "first", PARTICIPANT_FIELDS);
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let discovered = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os).unwrap();
            let plans = discovered
                .plans
                .iter()
                .map(|plan| plan.case().name())
                .collect::<Vec<_>>();
            let excluded = discovered.excluded.iter().map(CaseConfig::name).collect::<Vec<_>>();
            match os {
                Os::Linux => {
                    assert_eq!(plans, ["first", "second"]);
                    assert_eq!(excluded, ["windows-only"]);
                }
                Os::MacOs => {
                    assert!(plans.is_empty());
                    assert_eq!(excluded, ["first", "second", "windows-only"]);
                }
                Os::Windows => {
                    assert_eq!(plans, ["windows-only"]);
                    assert_eq!(excluded, ["first", "second"]);
                }
            }
        }
    }

    #[test]
    fn discovery_reports_every_malformed_file_sorted_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = write_case(dir.path(), "b-unknown-type", "type: bogus\n");
        let invalid = write_case(dir.path(), "a-invalid-yaml", "participants: [");
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
        assert!(message.contains("Failed to parse configuration file"), "{message}");
        assert!(message.contains("unknown field `type`"), "{message}");
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
        assert_eq!(cases.iter().map(CaseConfig::name).collect::<Vec<_>>(), ["good"]);
        assert_eq!(cases[0].source(), good.canonicalize().unwrap());
    }

    #[test]
    fn a_stray_config_yml_is_reported_with_the_other_problems_in_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = write_case(dir.path(), "invalid", "participants: [");
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
                "Failed to parse configuration file: {}",
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
        let mut roots = [dir.path().join("windows"), dir.path().join("linux")];
        let mut sources = [
            write_case(
                &roots[0],
                "shared",
                "name: shared\ntimeout: 1s\nsubject_os: [windows]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
            ).canonicalize().unwrap(),
            write_case(
                &roots[1],
                "shared",
                PARTICIPANT_FIELDS,
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
                .expect("subject OS filtering must not hide duplicate names");
            assert_eq!(format!("{error:#}"), expected);
            roots.reverse();
        }
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
            let case = crate::CASE_LOADER.load_file(&path).unwrap();
            plans.push(planner().compile(case, Os::Windows).unwrap().into_plan().unwrap());
        }
        assert_eq!(
            plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
            ["second", "integration", "first"]
        );
        for plan in plans {
            assert_eq!(
                plan.layout().iter().collect::<Vec<_>>(),
                [(
                    "adp",
                    Realization {
                        os: Os::Windows,
                        kind: RealizationKind::WindowsContainer,
                    }
                )]
            );
        }
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
    fn case_directory_is_the_canonical_source_parent() {
        let root = tempfile::tempdir().unwrap();
        let path = write_case(root.path(), "relative", PARTICIPANT_FIELDS);
        let case = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(case_dir(&case), path.parent().unwrap().canonicalize().unwrap());
    }

    #[test]
    fn pod_requirements_include_non_subject_participants() {
        let native = plan(&PARTICIPANT_FIELDS.replace("[linux]", "[macos]"), Os::MacOs);
        assert!(!needs_pod(&native));
        let yaml = PARTICIPANT_FIELDS.replace(
            "participants:",
            "participants:\n  other: {type: plain, os: linux, realization: {kind: kubernetes-pod}}",
        );
        let mixed = plan_with_plain(&yaml, "mixed", Os::Linux);
        assert!(needs_pod(&mixed));
    }

    #[test]
    fn participant_environment_maps_require_yaml_strings_in_base_and_os_overlays() {
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
            for config in [
                format!("env: {{INVALID: {value}}}"),
                format!("os: {{linux: {{env: {{INVALID: {value}}}}}}}"),
            ] {
                let yaml = PARTICIPANT_FIELDS.replace("subject: true", &format!("subject: true, config: {{{config}}}"));
                let error = crate::CASE_LOADER
                    .load_content("integration", &yaml, "strict", Path::new("strict/config.yaml"))
                    .err()
                    .expect("non-string environment value must fail");
                assert!(
                    format!("{error:#}").contains("environment variable 'INVALID' must be a YAML string"),
                    "{config}: {error:#}"
                );
            }
        }
        let yaml = PARTICIPANT_FIELDS.replace(
            "subject: true",
            "subject: true, config: {env: {BOOL: 'true', NUMBER: '8125', SCIENTIFIC: '1e3', LEADING_ZERO: '01'}}",
        );
        let plan = plan(&yaml, Os::Linux);
        let env = &plan.effective_config("adp").unwrap().env;
        for (key, value) in [
            ("BOOL", "true"),
            ("NUMBER", "8125"),
            ("SCIENTIFIC", "1e3"),
            ("LEADING_ZERO", "01"),
        ] {
            assert_eq!(env[key], value);
        }
    }

    pub(crate) fn write_case(root: &Path, name: &str, yaml: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }
}

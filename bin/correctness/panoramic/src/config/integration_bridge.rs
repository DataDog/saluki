//! Translates a case that declares participants into an [`IntegrationConfig`] so the integration runner can execute it.
//!
//! Supports one `agent-adp` participant configured only by `config.env`, with `log_contains` and `log_not_contains`
//! (`process` omitted), translated to `stream: both`, and `file_contains`. File paths may reference the participant's
//! facts, resolved per subject OS. Log steps search captured output: Linux container logs (Agent and ADP under s6),
//! macOS stdout and stderr of the spawned Agent and ADP processes, or Windows container console output only.
//! The translated configuration has default container settings, with empty `container.exposed_ports`;
//! `runtimes` follow `subject_os` order.

use std::{any::Any, path::Path};

use super::{
    case::Os,
    case_file::{CaseFile, ParticipantConfig, ParticipantEntry, ProcedureNode, StepNode},
    AssertionConfig, AssertionStep, ContainerConfig, IntakeConfig, IntegrationConfig, LogStream, Step,
};
use crate::{
    dynamic_vars::ENV_PREFIX,
    participants::AGENT_ADP,
    record::step_label,
    steps::{
        file_contains::FileContains,
        log_contains::{LogContains, LogNotContains},
    },
};

impl CaseFile {
    /// Translates the file and its built procedure, preserving step order and time budgets.
    ///
    /// # Errors
    ///
    /// Returns the first unsupported participant setting or step, checking participants before steps,
    /// or a file path's first unresolvable fact. The caller supplies the case name and source in its error context.
    ///
    /// # Panics
    ///
    /// Panics if the built procedure differs in shape from the file's procedure.
    pub(super) fn integration_config(
        &self, name: &str, source: &Path, procedure: &[ProcedureNode<Box<dyn Step>>], os: Os,
    ) -> Result<IntegrationConfig, String> {
        let CaseFile {
            // The facade's canonical name is used instead.
            name: _,
            description,
            timeout,
            subject_os,
            participants,
            procedure: nodes,
        } = self;
        if participants.len() != 1 {
            return Err(format!(
                "it declares {} participants; the integration runner runs exactly one",
                participants.len()
            ));
        }
        let (participant, entry) = participants.first_key_value().unwrap();
        let ParticipantEntry {
            participant_type,
            subject: _,
            os: participant_os,
            realization,
            config,
            connect,
        } = entry;
        let ParticipantConfig {
            env,
            files,
            endpoints,
            params,
            os: overlays,
        } = config;
        if participant_type != AGENT_ADP.name {
            return Err(format!(
                "participant '{participant}' has type '{participant_type}'; the integration runner runs only '{}'",
                AGENT_ADP.name
            ));
        }
        for (field, is_set) in [
            ("os", participant_os.is_some()),
            ("realization", realization.is_some()),
            ("connect", !connect.is_empty()),
            ("config.files", !files.is_empty()),
            ("config.endpoints", !endpoints.is_empty()),
            ("config.params", !params.is_empty()),
            ("config.os", !overlays.is_empty()),
        ] {
            if is_set {
                return Err(format!(
                    "participant '{participant}' sets '{field}', which the integration runner does not support"
                ));
            }
        }
        for (key, value) in env {
            if key.contains("{{") || value.contains("{{") {
                return Err(format!(
                    "participant '{participant}' sets config.env '{key}' with a template reference ('{{{{'); \
                     panoramic does not resolve references in environment variables"
                ));
            }
            if key.starts_with(ENV_PREFIX) {
                return Err(format!(
                    "participant '{participant}' sets config.env '{key}'; names starting with {ENV_PREFIX} \
                     are reserved for cases with 'type: integration'"
                ));
            }
        }

        let translate = |node: &StepNode, step: &dyn Step, label: String| {
            let error = |reason: &str| format!("step {label} '{}' {reason}", node.name);
            let process_error = || {
                error(
                    "sets 'process', which the integration runner does not support: \
                       it searches the output of every process together",
                )
            };
            let check_on = |on: &str| {
                if on == participant {
                    Ok(())
                } else {
                    Err(error(&format!(
                        "runs on '{on}'; the integration runner runs only participant '{participant}'"
                    )))
                }
            };
            let step = step as &dyn Any;
            if let Some(step) = step.downcast_ref::<LogContains>() {
                let LogContains {
                    on,
                    process,
                    pattern,
                    regex,
                    timeout,
                } = step;
                check_on(on)?;
                if process.is_some() {
                    return Err(process_error());
                }
                Ok(AssertionConfig::LogContains {
                    pattern: pattern.clone(),
                    regex: *regex,
                    timeout: timeout.clone(),
                    stream: LogStream::Both,
                })
            } else if let Some(step) = step.downcast_ref::<LogNotContains>() {
                let LogNotContains {
                    on,
                    process,
                    pattern,
                    regex,
                    during,
                } = step;
                check_on(on)?;
                if process.is_some() {
                    return Err(process_error());
                }
                Ok(AssertionConfig::LogNotContains {
                    pattern: pattern.clone(),
                    regex: *regex,
                    during: during.clone(),
                    stream: LogStream::Both,
                })
            } else if let Some(step) = step.downcast_ref::<FileContains>() {
                let FileContains {
                    on,
                    path,
                    pattern,
                    regex,
                    timeout,
                } = step;
                check_on(on)?;
                Ok(AssertionConfig::FileContains {
                    path: path
                        .resolve(|name, fact| {
                            if name == participant {
                                AGENT_ADP.fact_value(fact, os)
                            } else {
                                None
                            }
                        })
                        .map_err(|reason| error(&format!("path: {reason}")))?,
                    pattern: Some(pattern.clone()),
                    regex: *regex,
                    timeout: timeout.clone(),
                })
            } else {
                Err(error("has no integration runner equivalent"))
            }
        };
        assert_eq!(
            nodes.len(),
            procedure.len(),
            "the built procedure must have the same shape as the case file"
        );
        let procedure = nodes
            .iter()
            .zip(procedure)
            .enumerate()
            .map(|(index, (node, built))| match (node, built) {
                (ProcedureNode::Step(node), ProcedureNode::Step(step)) => {
                    translate(node, step.as_ref(), step_label(index, None)).map(AssertionStep::Single)
                }
                (ProcedureNode::Parallel(nodes), ProcedureNode::Parallel(steps)) => {
                    assert_eq!(
                        nodes.len(),
                        steps.len(),
                        "the built procedure must have the same shape as the case file"
                    );
                    let parallel = nodes
                        .iter()
                        .zip(steps)
                        .enumerate()
                        .map(|(member, (node, step))| translate(node, step.as_ref(), step_label(index, Some(member))))
                        .collect::<Result<_, _>>()?;
                    Ok(AssertionStep::Parallel { parallel })
                }
                _ => unreachable!("the built procedure must have the same shape as the case file"),
            })
            .collect::<Result<_, _>>()?;

        Ok(IntegrationConfig {
            name: name.to_string(),
            description: description.clone(),
            timeout: timeout.clone(),
            container: ContainerConfig::default(),
            intake: IntakeConfig::default(),
            env: env.iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
            procedure,
            runtimes: subject_os.iter().map(|os| os.runtime().to_string()).collect(),
            loaded_from: source.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            build_runners,
            case::{CaseConfig, CaseSchema},
            tests::{write_case, LOGGING_INTEGRATION, LOGGING_PARTICIPANT, PARTICIPANT_FIELDS},
            Need, LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME,
        },
        steps::{tests::fields, REGISTERED_STEPS},
        test::TestSuite,
    };

    #[test]
    fn participant_cases_build_the_same_integration_config_as_their_integration_twins() {
        for (participant_yaml, integration_yaml) in [
            (
                PARTICIPANT_FIELDS,
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
            ),
            (LOGGING_PARTICIPANT, LOGGING_INTEGRATION),
        ] {
            let participant_root = tempfile::tempdir().unwrap();
            let integration_root = tempfile::tempdir().unwrap();
            let participant_path = write_case(participant_root.path(), "alpha", participant_yaml);
            let integration_path = write_case(integration_root.path(), "alpha", integration_yaml);
            let participant_cases = CaseConfig::load_file(&participant_path).unwrap();
            let integration_cases = CaseConfig::load_file(&integration_path).unwrap();
            assert_eq!(participant_cases.len(), 1);
            assert_eq!(integration_cases.len(), 1);
            assert_eq!(build_runners(&participant_cases, LINUX_RUNTIME, &[]).unwrap().len(), 1);
            assert_eq!(build_runners(&integration_cases, LINUX_RUNTIME, &[]).unwrap().len(), 1);
            let participant = &participant_cases[0];
            let CaseSchema::Participants { file, procedure } = participant.schema() else {
                panic!("expected participant data");
            };
            let config = file
                .integration_config(participant.name(), participant.source(), procedure, Os::Linux)
                .unwrap();
            let CaseSchema::Integration(integration) = integration_cases[0].schema() else {
                panic!("expected integration data");
            };
            assert_eq!(
                serde_json::to_value(&config).unwrap(),
                serde_json::to_value(integration).unwrap()
            );
            assert_eq!(config.loaded_from, participant_path.canonicalize().unwrap());
            assert_eq!(integration.loaded_from, integration_path.canonicalize().unwrap());
        }
    }

    #[test]
    fn participant_case_runners_match_their_integration_twins_on_every_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "alpha", LOGGING_PARTICIPANT);
        let participant_cases = CaseConfig::load_file(&path).unwrap();
        write_case(dir.path(), "alpha", LOGGING_INTEGRATION);
        let integration_cases = CaseConfig::load_file(&path).unwrap();
        let overrides = ["container=target:custom".parse().unwrap()];
        for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME] {
            let overrides = if runtime == MAC_RUNTIME {
                let errors = [&participant_cases, &integration_cases].map(|cases| {
                    build_runners(cases, runtime, &overrides)
                        .err()
                        .expect("host processes have no container image to override")
                        .to_string()
                });
                assert_eq!(errors[0], errors[1]);
                &[][..]
            } else {
                &overrides[..]
            };
            let participant_runners = build_runners(&participant_cases, runtime, overrides).unwrap();
            let integration_runners = build_runners(&integration_cases, runtime, overrides).unwrap();
            assert_eq!(participant_runners.len(), 1);
            assert_eq!(integration_runners.len(), 1);
            let participant = &participant_runners[0];
            let integration = &integration_runners[0];
            assert_eq!(participant.name(), integration.name());
            assert_eq!(participant.name(), "alpha");
            assert_eq!(participant.suite(), integration.suite());
            assert_eq!(participant.suite(), TestSuite::Integration);
            assert_eq!(participant.case_path(), integration.case_path());
            assert_eq!(participant.case_path(), path.canonicalize().unwrap().parent().unwrap());
            assert_eq!(participant.timeout(), integration.timeout());
            assert_eq!(participant.runtime(), integration.runtime());
            assert_eq!(participant.runtime(), runtime);
            assert_eq!(participant.images(), integration.images());
            if runtime == MAC_RUNTIME {
                assert!(participant.images().is_empty());
            } else {
                assert_eq!(participant.images()["container"], "target:custom");
            }
        }
    }

    #[test]
    fn default_log_file_references_translate_to_each_os_literal_path() {
        let dir = tempfile::tempdir().unwrap();
        for (oses, runtimes) in [
            ("linux, windows", "linux, windows"),
            ("windows, linux", "windows, linux"),
        ] {
            let participant_yaml = LOGGING_PARTICIPANT
                .replace("linux, macos, windows", oses)
                .replace(r"C:\adp\adp.log", "{{ adp.facts.default_log_file }}");
            let path = write_case(dir.path(), "alpha", &participant_yaml);
            let participant_cases = CaseConfig::load_file(&path).unwrap();
            for (os, literal) in [
                (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
                (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
            ] {
                let integration_yaml = LOGGING_INTEGRATION
                    .replace("linux, mac, windows", runtimes)
                    .replace(r"C:\adp\adp.log", literal);
                write_case(dir.path(), "alpha", &integration_yaml);
                let integration_cases = CaseConfig::load_file(&path).unwrap();
                let CaseSchema::Integration(integration) = integration_cases[0].schema() else {
                    panic!("expected integration data")
                };
                let translated = participant_cases[0].integration_config(os).unwrap();
                let Some(AssertionStep::Single(AssertionConfig::FileContains { path, .. })) =
                    translated.procedure.last()
                else {
                    panic!("expected a final file assertion");
                };
                assert_eq!(path, literal);
                assert_eq!(
                    serde_json::to_value(&translated).unwrap(),
                    serde_json::to_value(integration).unwrap()
                );
                assert!(!serde_json::to_string(&translated).unwrap().contains("{{"));
                assert_eq!(translated.runtimes, runtimes.split(", ").collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn translation_rejects_a_step_on_another_participant() {
        for declaration in REGISTERED_STEPS {
            for (parallel, label) in [(false, "1"), (true, "1.1")] {
                let mut file: CaseFile = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
                let mut fields = fields(declaration.name);
                fields.insert("on".into(), serde_json::json!("other"));
                let step = (declaration.build)(&fields).unwrap();
                let node = StepNode {
                    name: declaration.name.into(),
                    fields,
                };
                let procedure = if parallel {
                    file.procedure = vec![ProcedureNode::Parallel(vec![node])];
                    vec![ProcedureNode::Parallel(vec![step])]
                } else {
                    file.procedure = vec![ProcedureNode::Step(node)];
                    vec![ProcedureNode::Step(step)]
                };
                assert_eq!(
                    file.integration_config("alpha", Path::new("alpha/config.yaml"), &procedure, Os::Linux)
                        .unwrap_err(),
                    format!(
                        "step {label} '{}' runs on 'other'; the integration runner runs only participant 'adp'",
                        declaration.name
                    )
                );
            }
        }
    }

    #[test]
    fn every_registered_step_translates_to_a_log_or_file_assertion_on_every_subject_os() {
        for declaration in REGISTERED_STEPS {
            let yaml = PARTICIPANT_FIELDS
                .replace("[linux]", "[linux, macos, windows]")
                .replace("  adp:", "  subject:");
            let mut file: CaseFile = serde_saphyr::from_str(&yaml).unwrap();
            assert_eq!(file.subject_os, [Os::Linux, Os::MacOs, Os::Windows]);
            let fields = fields(declaration.name);
            let procedure = [ProcedureNode::Step((declaration.build)(&fields).unwrap())];
            file.procedure = vec![ProcedureNode::Step(StepNode {
                name: declaration.name.into(),
                fields,
            })];
            for os in &file.subject_os {
                let config = file
                    .integration_config("alpha", Path::new("alpha/config.yaml"), &procedure, *os)
                    .unwrap();
                let [AssertionStep::Single(assertion)] = config.procedure.as_slice() else {
                    panic!("expected one assertion for '{}' on {os:?}", declaration.name);
                };
                match assertion {
                    AssertionConfig::LogContains { stream, .. } | AssertionConfig::LogNotContains { stream, .. } => {
                        assert!(matches!(stream, LogStream::Both));
                    }
                    AssertionConfig::FileContains { pattern, .. } => assert!(pattern.is_some()),
                    AssertionConfig::ProcessStableFor { .. }
                    | AssertionConfig::AdpExitsWith { .. }
                    | AssertionConfig::PortListening { .. }
                    | AssertionConfig::HttpCheck { .. }
                    | AssertionConfig::AdpConfigKeyEquals { .. }
                    | AssertionConfig::IntakeHasMetric { .. } => panic!(
                        "step '{}' on {os:?} would be observed through container liveness, log-parsed exits or another old mechanism",
                        declaration.name
                    ),
                }
            }
        }
    }

    #[test]
    fn translation_reports_a_fact_that_cannot_resolve_for_the_single_participant() {
        for (reference, os) in [
            ("other.facts.default_log_file", Os::Linux),
            ("adp.facts.unknown", Os::Linux),
            ("adp.facts.default_log_file", Os::MacOs),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("procedure: []", &format!("procedure: [{{step: file_contains, on: adp, path: '{{{{ {reference} }}}}', pattern: ready, timeout: 1s}}]"));
            let file: CaseFile = serde_saphyr::from_str(&yaml).unwrap();
            let ProcedureNode::Step(node) = &file.procedure[0] else {
                panic!("expected one step")
            };
            let procedure = [ProcedureNode::Step(
                (crate::steps::file_contains::FILE_CONTAINS.build)(&node.fields).unwrap(),
            )];
            assert_eq!(
                file.integration_config("alpha", Path::new("alpha/config.yaml"), &procedure, os)
                    .unwrap_err(),
                format!("step 1 'file_contains' path: cannot resolve fact reference '{{{{ {reference} }}}}'")
            );
        }
    }

    #[test]
    fn the_integration_runner_rejects_participant_cases_it_cannot_run() {
        for (old, new, reason) in [
            (
                "procedure: []",
                "  other: {type: agent-adp}\nprocedure: []",
                "it declares 2 participants; the integration runner runs exactly one",
            ),
            (
                "subject: true",
                "subject: true, realization: {kind: linux-container}",
                "participant 'adp' sets 'realization', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {os: {linux: {env: {A: b}}}}",
                "participant 'adp' sets 'config.os', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {env: {DD_BIND_HOST: \"{{PANORAMIC_DYNAMIC_IP}}\"}}",
                "participant 'adp' sets config.env 'DD_BIND_HOST' with a template reference ('{{'); panoramic does not resolve references in environment variables",
            ),
            (
                "subject: true",
                "subject: true, config: {env: {\"{{name}}\": x}}",
                "participant 'adp' sets config.env '{{name}}' with a template reference ('{{'); panoramic does not resolve references in environment variables",
            ),
            (
                "subject: true",
                "subject: true, config: {env: {PANORAMIC_DYNAMIC_IP: \"hostname -i\"}}",
                "participant 'adp' sets config.env 'PANORAMIC_DYNAMIC_IP'; names starting with PANORAMIC_DYNAMIC_ are reserved for cases with 'type: integration'",
            ),
            (
                "procedure: []",
                "procedure: [{step: log_contains, on: adp, process: adp, pattern: ready, timeout: 1s}]",
                "step 1 'log_contains' sets 'process', which the integration runner does not support: it searches the output of every process together",
            ),
            (
                "procedure: []",
                "procedure:\n  - {step: log_contains, on: adp, pattern: ready, timeout: 1s}\n  - parallel:\n      - {step: log_not_contains, on: adp, process: agent, pattern: panic, during: 1s}",
                "step 2.1 'log_not_contains' sets 'process', which the integration runner does not support: it searches the output of every process together",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_case(dir.path(), "alpha", &PARTICIPANT_FIELDS.replace(old, new));
            let cases = CaseConfig::load_file(&path).unwrap();
            let error = build_runners(&cases, LINUX_RUNTIME, &[])
                .err()
                .expect("unsupported participant cases must fail runner construction");
            assert_eq!(
                error.to_string(),
                format!(
                    "Failed to build test runners:\n  - Cannot run case 'alpha' from {}: {reason}",
                    path.canonicalize().unwrap().display()
                )
            );
        }
    }

    #[test]
    fn translation_rejects_fields_the_loader_rejects_for_agent_adp() {
        for (old, new, reason) in [
            (
                "agent-adp",
                "datadog-intake",
                "participant 'adp' has type 'datadog-intake'; the integration runner runs only 'agent-adp'",
            ),
            (
                "subject: true",
                "subject: true, os: windows",
                "participant 'adp' sets 'os', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, connect: {intake: x.api}",
                "participant 'adp' sets 'connect', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {files: {datadog.yaml: settings.yaml}}",
                "participant 'adp' sets 'config.files', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {endpoints: {api: {port: 80}}}",
                "participant 'adp' sets 'config.endpoints', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {params: {mode: fast}}",
                "participant 'adp' sets 'config.params', which the integration runner does not support",
            ),
        ] {
            let file: CaseFile = serde_saphyr::from_str(&PARTICIPANT_FIELDS.replace(old, new)).unwrap();
            assert_eq!(
                file.integration_config("alpha", Path::new("alpha/config.yaml"), &[], Os::Linux)
                    .unwrap_err(),
                reason
            );
        }
    }

    #[test]
    fn translation_panics_when_the_file_and_built_procedure_have_different_shapes() {
        for nodes in ["[]", "[{parallel: [{step: log_contains}]}]", "[{step: log_contains}]"] {
            let file: CaseFile =
                serde_saphyr::from_str(&PARTICIPANT_FIELDS.replace("procedure: []", &format!("procedure: {nodes}")))
                    .unwrap();
            let procedure = [ProcedureNode::Parallel(vec![])];
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                file.integration_config("alpha", Path::new("alpha/config.yaml"), &procedure, Os::Linux)
            }))
            .expect_err("a shape mismatch must panic before zip can truncate assertions");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .expect("a shape mismatch must explain the broken invariant");
            assert!(
                message.contains("the built procedure must have the same shape as the case file"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_step_without_an_integration_runner_equivalent_fails_translation() {
        #[derive(Debug)]
        struct Probe;

        impl Step for Probe {
            fn needs(&self) -> Vec<Need> {
                vec![]
            }
        }

        let file: CaseFile =
            serde_saphyr::from_str(&PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{step: probe}]")).unwrap();
        let procedure = [ProcedureNode::Step(Box::new(Probe) as Box<dyn Step>)];
        assert_eq!(
            file.integration_config("alpha", Path::new("alpha/config.yaml"), &procedure, Os::Linux)
                .unwrap_err(),
            "step 1 'probe' has no integration runner equivalent"
        );
    }
}

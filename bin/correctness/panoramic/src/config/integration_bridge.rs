//! Translates a case that declares participants into an [`IntegrationConfig`] so the integration runner can execute it.
//!
//! Reads step fields from the case file and environment from the plan. Supports one `agent-adp` participant
//! configured through `config.env` and `config.os` env overlays, with `log_contains` and `log_not_contains`
//! (`process` omitted), translated to `stream: both`, and `file_contains`.
//! File paths may reference the participant's facts, resolved from the participant's planned realization;
//! the integration runner rejects `shared_dir` and late-fact references.
//! Log steps search captured output: Linux container logs (Agent and ADP under s6),
//! macOS stdout and stderr of the spawned Agent and ADP processes, or Windows container console output only.
//! The translated configuration has default container settings, with empty `container.exposed_ports`;
//! `runtimes` follow `subject_os` order.

use std::path::Path;

use panoramic_core::case::{CaseFile, ParticipantConfig, ParticipantEntry, ProcedureNode, StepNode};
use panoramic_core::participant::{EffectiveConfig, SHARED_DIR_FACT};
use panoramic_core::record::step_label;
use panoramic_core::step::{decode_fields, Need};
use panoramic_participants::AgentAdp;
use panoramic_steps::{FileContains, LogContains, LogNotContains};
use serde::de::DeserializeOwned;

use super::{
    runtime_for_os, AssertionConfig, AssertionStep, ContainerConfig, IntakeConfig, IntegrationConfig, LogStream,
};
use crate::dynamic_vars::ENV_PREFIX;

/// Translates the case file's step fields, preserving step order and time budgets.
///
/// # Errors
///
/// Returns the first unsupported participant setting, missing planned configuration, unsupported or invalid step,
/// or unresolvable file-path fact.
/// Checks participants before steps. The caller supplies the case name and source in its error context.
pub(super) fn integration_config<'a>(
    file: &CaseFile, name: &str, source: &Path, fact: impl Fn(&str, &str) -> Option<&'a str>,
    config: impl Fn(&str) -> Option<&'a EffectiveConfig>,
) -> Result<IntegrationConfig, String> {
    let CaseFile {
        // Use the name resolved by the case loader.
        name: _,
        description,
        timeout,
        subject_os,
        participants,
        procedure,
    } = file;
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
        config: case_config,
        connect,
    } = entry;
    let ParticipantConfig {
        env: case_env,
        files,
        endpoints,
        params,
        os: overlays,
    } = case_config;
    if participant_type != AgentAdp::NAME {
        return Err(format!(
            "participant '{participant}' has type '{participant_type}'; the integration runner runs only '{}'",
            AgentAdp::NAME
        ));
    }
    for (field, is_set) in [
        ("os", participant_os.is_some()),
        ("realization", realization.is_some()),
        ("connect", !connect.is_empty()),
    ] {
        if is_set {
            return Err(format!(
                "participant '{participant}' sets '{field}', which the integration runner does not support"
            ));
        }
    }
    let EffectiveConfig {
        env,
        files: _,
        params: _,
        endpoints: _,
    } = config(participant).ok_or_else(|| format!("participant '{participant}' has no planned configuration"))?;
    if !files.is_empty() || overlays.values().any(|overlay| !overlay.files.is_empty()) {
        return Err(format!(
            "participant '{participant}' sets 'config.files' or an overlay's 'files', which the integration runner does not support"
        ));
    }
    for (field, is_set) in [
        ("config.endpoints", !endpoints.is_empty()),
        ("config.params", !params.is_empty()),
    ] {
        if is_set {
            return Err(format!(
                "participant '{participant}' sets '{field}', which the integration runner does not support"
            ));
        }
    }
    for (os, overlay) in overlays {
        if !overlay.params.is_empty() {
            return Err(format!(
                "participant '{participant}' sets 'config.os.{}.params', which the integration runner does not support",
                os.name(),
            ));
        }
    }
    for (field, values) in std::iter::once(("config.env".to_string(), case_env)).chain(
        overlays
            .iter()
            .map(|(os, overlay)| (format!("config.os.{}.env", os.name()), &overlay.env)),
    ) {
        for (key, value) in values {
            if value.contains("{{") {
                return Err(format!(
                    "participant '{participant}' sets {field} '{key}' with a template reference ('{{{{'); \
                     the integration runner does not support references in environment variables"
                ));
            }
            if key.starts_with(ENV_PREFIX) {
                return Err(format!(
                    "participant '{participant}' sets {field} '{key}'; names starting with {ENV_PREFIX} \
                     are reserved for cases with 'type: integration'"
                ));
            }
        }
    }

    let translate = |node: &StepNode, label: String| {
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
        match node.name.as_str() {
            LogContains::NAME => {
                let LogContains {
                    on,
                    process,
                    pattern,
                    regex,
                    timeout,
                } = decode(node, &label)?;
                check_on(&on)?;
                if process.is_some() {
                    return Err(process_error());
                }
                Ok(AssertionConfig::LogContains {
                    pattern,
                    regex,
                    timeout: timeout.into(),
                    stream: LogStream::Both,
                })
            }
            LogNotContains::NAME => {
                let LogNotContains {
                    on,
                    process,
                    pattern,
                    regex,
                    during,
                } = decode(node, &label)?;
                check_on(&on)?;
                if process.is_some() {
                    return Err(process_error());
                }
                Ok(AssertionConfig::LogNotContains {
                    pattern,
                    regex,
                    during: during.into(),
                    stream: LogStream::Both,
                })
            }
            FileContains::NAME => {
                let FileContains {
                    on,
                    path,
                    pattern,
                    regex,
                    timeout,
                } = decode(node, &label)?;
                check_on(&on)?;
                if path
                    .needs()
                    .any(|need| matches!(need, Need::Fact(_, fact) if fact == SHARED_DIR_FACT))
                {
                    return Err(error(
                        "path: the integration runner does not create participant shared directories",
                    ));
                }
                Ok(AssertionConfig::FileContains {
                    path: path
                        .resolve(&fact)
                        .map_err(|reason| error(&format!("path: {reason}")))?,
                    pattern: Some(pattern),
                    regex,
                    timeout: timeout.into(),
                })
            }
            _ => Err(error("has no integration runner equivalent")),
        }
    };
    let procedure = procedure
        .iter()
        .enumerate()
        .map(|(index, node)| match node {
            ProcedureNode::Step(node) => translate(node, step_label(index, None)).map(AssertionStep::Single),
            ProcedureNode::Parallel(nodes) => {
                let parallel = nodes
                    .iter()
                    .enumerate()
                    .map(|(member, node)| translate(node, step_label(index, Some(member))))
                    .collect::<Result<_, _>>()?;
                Ok(AssertionStep::Parallel { parallel })
            }
        })
        .collect::<Result<_, _>>()?;

    Ok(IntegrationConfig {
        name: name.to_string(),
        description: description.clone(),
        timeout: (*timeout).into(),
        container: ContainerConfig::default(),
        intake: IntakeConfig::default(),
        env: env.iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
        procedure,
        runtimes: subject_os.iter().map(|os| runtime_for_os(*os).to_string()).collect(),
        loaded_from: source.to_path_buf(),
    })
}

/// Decodes a step's fields, naming the step in any error.
///
/// Only hand-built case files can fail here: the loader already decoded and validated these fields.
fn decode<T>(node: &StepNode, label: &str) -> Result<T, String>
where
    T: DeserializeOwned,
{
    decode_fields(&node.fields).map_err(|reason| {
        format!(
            "step {label} '{}' has fields the integration runner cannot read: {reason}",
            node.name
        )
    })
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        case::{CaseLoader, CaseSchema, Registries},
        layout::{Os, RealizationKind},
        participant::ParticipantType,
        plan::Planner,
    };
    use panoramic_steps::{test_support::valid_fields, REGISTERED_STEPS};

    use super::*;
    use crate::{
        config::{
            build_runners,
            tests::{
                build_case_runners, minimal_procedure, plan, write_case, LOGGING_INTEGRATION, LOGGING_PARTICIPANT,
                PARTICIPANT_FIELDS,
            },
            typed_case::{RunnableCase, TypedConfig, TypedConfigLoader},
            LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME,
        },
        test::TestSuite,
    };

    #[test]
    fn participant_cases_build_the_same_integration_config_as_their_integration_twins() {
        for (participant_yaml, integration_yaml) in [
            (
                PARTICIPANT_FIELDS,
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: [{assertion: log_contains, pattern: ready, timeout: 5s}]\n",
            ),
            (LOGGING_PARTICIPANT, LOGGING_INTEGRATION),
        ] {
            let participant_root = tempfile::tempdir().unwrap();
            let integration_root = tempfile::tempdir().unwrap();
            let participant_path = write_case(participant_root.path(), "alpha", participant_yaml);
            let integration_path = write_case(integration_root.path(), "alpha", integration_yaml);
            let participant_cases = crate::CASE_LOADER.load_file(&participant_path).unwrap();
            let integration_cases = crate::CASE_LOADER.load_file(&integration_path).unwrap();
            assert_eq!(participant_cases.len(), 1);
            assert_eq!(integration_cases.len(), 1);
            let plans: Vec<_> = participant_cases
                .into_iter()
                .map(|case| {
                    Planner::new(crate::REGISTRIES)
                        .compile(case, Os::Linux)
                        .unwrap()
                        .unwrap()
                })
                .collect();
            let config = plans[0].integration_config().unwrap();
            let CaseSchema::Typed(TypedConfig::Integration(integration)) = integration_cases[0].schema() else {
                panic!("expected integration data");
            };
            assert_eq!(
                serde_json::to_value(&config).unwrap(),
                serde_json::to_value(integration).unwrap()
            );
            assert_eq!(config.loaded_from, participant_path.canonicalize().unwrap());
            assert_eq!(integration.loaded_from, integration_path.canonicalize().unwrap());
            assert_eq!(build_runners(plans, Os::Linux, &[]).unwrap().len(), 1);
            assert_eq!(build_case_runners(integration_cases, Os::Linux, &[]).unwrap().len(), 1);
        }
    }

    #[test]
    fn participant_case_runners_match_their_integration_twins_on_every_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alpha/config.yaml");
        let overrides = ["container=target:custom".parse().unwrap()];
        for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME] {
            let os = crate::config::os_for_runtime(runtime).unwrap();
            let [participant_plans, integration_plans] = [LOGGING_PARTICIPANT, LOGGING_INTEGRATION].map(|yaml| {
                write_case(dir.path(), "alpha", yaml);
                crate::CASE_LOADER
                    .load_file(&path)
                    .unwrap()
                    .into_iter()
                    .map(|case| Planner::new(crate::REGISTRIES).compile(case, os).unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            let overrides = if runtime == MAC_RUNTIME {
                let errors = [&participant_plans, &integration_plans].map(|plans| {
                    build_runners(plans, os, &overrides)
                        .err()
                        .expect("host processes have no container image to override")
                        .to_string()
                });
                assert_eq!(errors[0], errors[1]);
                &[][..]
            } else {
                &overrides[..]
            };
            let participant_runners = build_runners(&participant_plans, os, overrides).unwrap();
            let integration_runners = build_runners(&integration_plans, os, overrides).unwrap();
            assert_eq!(participant_runners.len(), 1);
            assert_eq!(integration_runners.len(), 1);
            let participant = &participant_runners[0];
            let integration = &integration_runners[0];
            assert_eq!(participant.plan.case().name(), integration.plan.case().name());
            assert_eq!(participant.plan.case().name(), "alpha");
            assert_eq!(participant.plan.case().suite(), integration.plan.case().suite());
            assert_eq!(participant.plan.case().suite(), TestSuite::Integration.name());
            assert_eq!(
                participant.plan.case().source().parent().unwrap(),
                integration.plan.case().source().parent().unwrap()
            );
            assert_eq!(
                participant.plan.case().source().parent().unwrap(),
                path.canonicalize().unwrap().parent().unwrap()
            );
            assert_eq!(participant.plan.case().timeout(), integration.plan.case().timeout());
            assert_eq!(participant.runner.runtime(), integration.runner.runtime());
            assert_eq!(participant.runner.runtime(), runtime);
            assert_eq!(participant.runner.images(), integration.runner.images());
            if runtime == MAC_RUNTIME {
                assert!(participant.runner.images().is_empty());
            } else {
                assert_eq!(participant.runner.images()["container"], "target:custom");
            }
        }
    }

    #[test]
    fn an_env_overlay_changes_the_translated_env_only_on_its_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]").replace(
            "subject: true",
            "subject: true, config: {env: {A: base, B: base}, os: {windows: {env: {A: win, C: win}}}}",
        );
        for (os, expected) in [
            (Os::Linux, vec![("A", "base"), ("B", "base")]),
            (Os::Windows, vec![("A", "win"), ("B", "base"), ("C", "win")]),
        ] {
            let config = plan(&yaml, os).integration_config().unwrap();
            assert_eq!(
                config.env,
                expected.into_iter().map(|(k, v)| (k.into(), v.into())).collect()
            );
        }
    }

    #[test]
    fn overlay_env_values_with_template_references_or_reserved_names_are_rejected_on_every_subject_os() {
        for (env, reason) in [
            ("{KEY: '{{ adp.facts.default_log_file }}'}", "'KEY' with a template reference ('{{'); the integration runner does not support references in environment variables"),
            ("{PANORAMIC_DYNAMIC_IP: value}", "'PANORAMIC_DYNAMIC_IP'; names starting with PANORAMIC_DYNAMIC_ are reserved for cases with 'type: integration'"),
        ] {
            for overlay_os in ["linux", "windows"] {
                let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[windows, linux]").replace(
                    "subject: true",
                    &format!("subject: true, config: {{os: {{{overlay_os}: {{env: {env}}}}}}}"),
                );
                for os in [Os::Linux, Os::Windows] {
                    let plan = plan(&yaml, os);
                    assert_eq!(plan.integration_config().unwrap_err().to_string(), format!(
                        "Cannot run case 'case' from /cases/case/config.yaml: participant 'adp' sets config.os.{overlay_os}.env {reason}"
                    ));
                }
            }
        }
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[windows, linux]").replace(
            "subject: true",
            "subject: true, config: {os: {windows: {env: {PANORAMIC_DYNAMIC_IP: windows}}, linux: {env: {PANORAMIC_DYNAMIC_IP: linux}}}}",
        );
        for os in [Os::Linux, Os::Windows] {
            let message = plan(&yaml, os).integration_config().unwrap_err().to_string();
            assert!(message.contains("config.os.linux.env"), "{message}");
        }
    }

    #[test]
    fn the_integration_runner_rejects_resolvable_references_in_env_on_every_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]").replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ self.facts.default_log_file }}'}, os: {macos: {env: {KEY: '{{ self.late.ip }}'}}}}",
        );
        for (os, expected) in [
            (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
            (Os::MacOs, "127.0.0.1"),
            (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
        ] {
            let plan = plan(&yaml, os);
            assert_eq!(plan.effective_config("adp").unwrap().env["KEY"], expected);
            assert_eq!(plan.integration_config().unwrap_err().to_string(),
                "Cannot run case 'case' from /cases/case/config.yaml: participant 'adp' sets config.env 'KEY' with a template reference ('{{'); the integration runner does not support references in environment variables");
        }
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ self.late.ip }}'}}",
        );
        let plan = plan(&yaml, Os::MacOs);
        assert_eq!(plan.effective_config("adp").unwrap().env["KEY"], "127.0.0.1");
        assert_eq!(plan.integration_config().unwrap_err().to_string(),
            "Cannot run case 'case' from /cases/case/config.yaml: participant 'adp' sets config.env 'KEY' with a template reference ('{{'); the integration runner does not support references in environment variables");
    }

    #[test]
    fn the_integration_runner_rejects_overlay_files_and_params() {
        struct Configured;

        impl ParticipantType for Configured {
            fn name(&self) -> &'static str {
                AgentAdp::NAME
            }

            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                AgentAdp.supports()
            }

            fn config_files(&self) -> &'static [&'static str] {
                &["settings.yaml"]
            }

            fn params(&self) -> &'static [&'static str] {
                &["mode"]
            }

            fn processes(&self) -> &'static [&'static str] {
                AgentAdp.processes()
            }
        }

        let registries = Registries {
            participant_types: &[&Configured],
            steps: REGISTERED_STEPS,
        };
        let loader = CaseLoader::new(registries, TypedConfigLoader, TestSuite::Integration.name());
        let dir = tempfile::tempdir().unwrap();
        for (config, reason) in [
            (
                "{os: {windows: {files: {settings.yaml: windows.yaml}}}}",
                "'config.files' or an overlay's 'files'",
            ),
            ("{os: {windows: {params: {mode: fast}}}}", "'config.os.windows.params'"),
            (
                "{os: {windows: {params: {mode: fast}}, linux: {params: {mode: slow}}}}",
                "'config.os.linux.params'",
            ),
        ] {
            let yaml = PARTICIPANT_FIELDS
                .replace("[linux]", "[linux, windows]")
                .replace("subject: true", &format!("subject: true, config: {config}"));
            let path = write_case(dir.path(), "alpha", &yaml).canonicalize().unwrap();
            std::fs::write(path.parent().unwrap().join("windows.yaml"), "").unwrap();
            for os in [Os::Linux, Os::Windows] {
                let case = loader.load_file(&path).unwrap().remove(0);
                let plan = Planner::new(registries).compile(case, os).unwrap().unwrap();
                assert_eq!(
                    plan.effective_config("adp").unwrap().files.is_empty(),
                    os == Os::Linux || !config.contains("files:")
                );
                assert_eq!(plan.integration_config().unwrap_err().to_string(), format!(
                    "Cannot run case 'alpha' from {}: participant 'adp' sets {reason}, which the integration runner does not support",
                    path.display(),
                ));
            }
        }
    }

    #[test]
    fn translation_requires_the_participants_planned_configuration() {
        let file: CaseFile = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
        assert_eq!(
            integration_config(&file, "alpha", Path::new("alpha/config.yaml"), |_, _| None, |_| None,).unwrap_err(),
            "participant 'adp' has no planned configuration"
        );
    }

    #[test]
    fn translation_takes_environment_only_from_the_planned_configuration() {
        let file: CaseFile = serde_saphyr::from_str(
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, config: {env: {A: raw}}"),
        )
        .unwrap();
        let planned_config = EffectiveConfig {
            env: [("B".into(), "planned".into())].into(),
            ..EffectiveConfig::default()
        };
        let translated = integration_config(
            &file,
            "alpha",
            Path::new("alpha/config.yaml"),
            |_, _| None,
            |participant| {
                assert_eq!(participant, "adp");
                Some(&planned_config)
            },
        )
        .unwrap();
        assert_eq!(translated.env, [("B".into(), "planned".into())].into());
    }

    #[test]
    fn translation_resolves_file_paths_through_the_plan_on_every_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        for (oses, runtimes) in [
            ("linux, windows", "linux, windows"),
            ("windows, linux", "windows, linux"),
        ] {
            let participant_yaml = LOGGING_PARTICIPANT
                .replace("linux, macos, windows", oses)
                .replace(r"C:\adp\adp.log", "{{ adp.facts.default_log_file }}");
            let path = write_case(dir.path(), "alpha", &participant_yaml);
            for (os, literal) in [
                (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
                (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
            ] {
                let integration_yaml = LOGGING_INTEGRATION
                    .replace("linux, mac, windows", runtimes)
                    .replace(r"C:\adp\adp.log", literal);
                write_case(dir.path(), "alpha", &integration_yaml);
                let integration_cases = crate::CASE_LOADER.load_file(&path).unwrap();
                let CaseSchema::Typed(TypedConfig::Integration(integration)) = integration_cases[0].schema() else {
                    panic!("expected integration data")
                };
                let case = crate::CASE_LOADER
                    .load_content(&participant_yaml, "alpha", &path)
                    .unwrap()
                    .remove(0);
                let plan = Planner::new(crate::REGISTRIES).compile(case, os).unwrap().unwrap();
                let translated = plan.integration_config().unwrap();
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
        let planned_config = EffectiveConfig::default();
        for factory in REGISTERED_STEPS {
            for (parallel, label) in [(false, "1"), (true, "1.1")] {
                let mut file: CaseFile = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
                let mut fields = valid_fields(factory.name());
                fields.insert("on".into(), serde_json::json!("other"));
                let node = StepNode {
                    name: factory.name().into(),
                    fields,
                };
                file.procedure = if parallel {
                    vec![ProcedureNode::Parallel(vec![node])]
                } else {
                    vec![ProcedureNode::Step(node)]
                };
                assert_eq!(
                    integration_config(
                        &file,
                        "alpha",
                        Path::new("alpha/config.yaml"),
                        |_, _| None,
                        |_| Some(&planned_config)
                    )
                    .unwrap_err(),
                    format!(
                        "step {label} '{}' runs on 'other'; the integration runner runs only participant 'adp'",
                        factory.name()
                    )
                );
            }
        }
    }

    #[test]
    fn every_registered_step_translates_to_a_log_or_file_assertion_on_every_subject_os() {
        let planned_config = EffectiveConfig::default();
        for factory in REGISTERED_STEPS {
            let yaml = PARTICIPANT_FIELDS
                .replace("[linux]", "[linux, macos, windows]")
                .replace("  adp:", "  subject:");
            let mut file: CaseFile = serde_saphyr::from_str(&yaml).unwrap();
            assert_eq!(file.subject_os, [Os::Linux, Os::MacOs, Os::Windows]);
            let fields = valid_fields(factory.name());
            file.procedure = vec![ProcedureNode::Step(StepNode {
                name: factory.name().into(),
                fields,
            })];
            for os in &file.subject_os {
                let config = integration_config(
                    &file,
                    "alpha",
                    Path::new("alpha/config.yaml"),
                    |_, _| None,
                    |_| Some(&planned_config),
                )
                .unwrap();
                let [AssertionStep::Single(assertion)] = config.procedure.as_slice() else {
                    panic!("expected one assertion for '{}' on {os:?}", factory.name());
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
                        factory.name()
                    ),
                }
            }
        }
    }

    #[test]
    fn translation_resolves_file_path_facts_only_through_the_resolver() {
        let planned_config = EffectiveConfig::default();
        for (reference, expected) in [
            ("other.facts.default_log_file", None),
            ("adp.facts.unknown", None),
            ("adp.facts.default_log_file", Some("/x")),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace(minimal_procedure!(), &format!("procedure: [{{step: file_contains, on: adp, path: '{{{{ {reference} }}}}', pattern: ready, timeout: 1s}}]"));
            let file: CaseFile = serde_saphyr::from_str(&yaml).unwrap();
            let translated = integration_config(
                &file,
                "alpha",
                Path::new("alpha/config.yaml"),
                |participant, fact| (participant == "adp" && fact == "default_log_file").then_some("/x"),
                |_| Some(&planned_config),
            );
            if let Some(expected) = expected {
                let config = translated.unwrap();
                let [AssertionStep::Single(AssertionConfig::FileContains { path, .. })] = config.procedure.as_slice()
                else {
                    panic!("expected one file assertion");
                };
                assert_eq!(path, expected);
            } else {
                assert_eq!(
                    translated.unwrap_err(),
                    format!("step 1 'file_contains' path: cannot resolve fact reference '{{{{ {reference} }}}}'")
                );
            }
        }
    }

    #[test]
    fn translation_rejects_shared_dir_in_file_paths() {
        for path in [
            "{{ adp.facts.shared_dir }}",
            "{{ adp.facts.default_log_file }}/{{ adp.facts.shared_dir }}/socket",
        ] {
            let yaml = PARTICIPANT_FIELDS.replace(
                minimal_procedure!(),
                &format!("procedure: [{{step: file_contains, on: adp, path: '{path}', pattern: ready, timeout: 1s}}]"),
            );
            let case = crate::CASE_LOADER
                .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = Planner::new(crate::REGISTRIES)
                .compile(case, Os::Linux)
                .unwrap()
                .unwrap();
            let CaseSchema::Participants { file, .. } = plan.case().schema() else {
                panic!("expected participant data")
            };
            let error = integration_config(
                file,
                "alpha",
                Path::new("alpha/config.yaml"),
                |_, _| panic!("shared_dir must be rejected before resolving facts"),
                |participant| plan.effective_config(participant),
            )
            .unwrap_err();
            assert_eq!(
                error,
                "step 1 'file_contains' path: the integration runner does not create participant shared directories"
            );
        }
    }

    #[test]
    fn translation_rejects_late_fact_references_in_file_paths() {
        let yaml = PARTICIPANT_FIELDS.replace(
            minimal_procedure!(),
            "procedure: [{step: file_contains, on: adp, path: '{{ adp.late.ip }}', pattern: ready, timeout: 1s}]",
        );
        let case = crate::CASE_LOADER
            .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::new(crate::REGISTRIES)
            .compile(case, Os::Linux)
            .unwrap()
            .unwrap();
        let CaseSchema::Participants { file, .. } = plan.case().schema() else {
            panic!("expected participant data");
        };
        let error = integration_config(
            file,
            "alpha",
            Path::new("alpha/config.yaml"),
            |participant, fact| plan.fact(participant, fact),
            |participant| plan.effective_config(participant),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "step 1 'file_contains' path: cannot resolve late fact reference '{{ adp.late.ip }}' \
             before the participant runs"
        );
    }

    #[test]
    fn the_integration_runner_rejects_participant_cases_it_cannot_run() {
        for (old, new, reason) in [
            (
                minimal_procedure!(),
                concat!("  other: {type: agent-adp}\n", minimal_procedure!()),
                "it declares 2 participants; the integration runner runs exactly one",
            ),
            (
                "subject: true",
                "subject: true, realization: {kind: linux-container}",
                "participant 'adp' sets 'realization', which the integration runner does not support",
            ),
            (
                "subject: true",
                "subject: true, config: {env: {PANORAMIC_DYNAMIC_IP: \"hostname -i\"}}",
                "participant 'adp' sets config.env 'PANORAMIC_DYNAMIC_IP'; names starting with PANORAMIC_DYNAMIC_ are reserved for cases with 'type: integration'",
            ),
            (
                minimal_procedure!(),
                "procedure: [{step: log_contains, on: adp, process: adp, pattern: ready, timeout: 1s}]",
                "step 1 'log_contains' sets 'process', which the integration runner does not support: it searches the output of every process together",
            ),
            (
                minimal_procedure!(),
                "procedure:\n  - {step: log_contains, on: adp, pattern: ready, timeout: 1s}\n  - parallel:\n      - {step: log_not_contains, on: adp, process: agent, pattern: panic, during: 1s}",
                "step 2.1 'log_not_contains' sets 'process', which the integration runner does not support: it searches the output of every process together",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_case(dir.path(), "alpha", &PARTICIPANT_FIELDS.replace(old, new));
            let cases = crate::CASE_LOADER.load_file(&path).unwrap();
            let error = build_case_runners(cases, Os::Linux, &[])
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
                "participant 'adp' sets 'config.files' or an overlay's 'files', which the integration runner does not support",
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
            let planned_config = EffectiveConfig {
                files: file.participants["adp"].config.files.clone(),
                ..EffectiveConfig::default()
            };
            assert_eq!(
                integration_config(&file, "alpha", Path::new("alpha/config.yaml"), |_, _| None, |_| Some(&planned_config)).unwrap_err(),
                reason
            );
        }
    }

    #[test]
    fn a_step_whose_fields_do_not_decode_fails_translation_naming_the_step() {
        let planned_config = EffectiveConfig::default();
        for (nodes, label) in [
            ("[{step: log_contains, on: adp}]", "1"),
            ("[{parallel: [{step: log_contains, on: adp}]}]", "1.1"),
        ] {
            let file: CaseFile = serde_saphyr::from_str(
                &PARTICIPANT_FIELDS.replace(minimal_procedure!(), &format!("procedure: {nodes}")),
            )
            .unwrap();
            assert_eq!(
                integration_config(&file, "alpha", Path::new("alpha/config.yaml"), |_, _| None, |_| Some(&planned_config)).unwrap_err(),
                format!("step {label} 'log_contains' has fields the integration runner cannot read: missing field `pattern`")
            );
        }
    }

    #[test]
    fn translation_reads_parallel_members_from_the_case_file() {
        let yaml = PARTICIPANT_FIELDS.replace(
            minimal_procedure!(),
            "procedure:\n  - {step: log_contains, on: adp, pattern: first, timeout: 1s}\n  - parallel:\n      - {step: log_not_contains, on: adp, pattern: second, during: 2s}\n      - {step: file_contains, on: adp, path: /logs/output.log, pattern: third, timeout: 3s}",
        );
        let cases = crate::CASE_LOADER
            .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap();
        let plan = Planner::new(crate::REGISTRIES)
            .compile(cases.into_iter().next().unwrap(), Os::Linux)
            .unwrap()
            .unwrap();
        let config = plan.integration_config().unwrap();
        let [AssertionStep::Single(AssertionConfig::LogContains { pattern: first, .. }), AssertionStep::Parallel { parallel }] =
            config.procedure.as_slice()
        else {
            panic!("expected a log assertion followed by a parallel block");
        };
        let [AssertionConfig::LogNotContains { pattern: second, .. }, AssertionConfig::FileContains {
            pattern: Some(third), ..
        }] = parallel.as_slice()
        else {
            panic!("expected parallel log and file assertions in that order");
        };
        assert_eq!([first, second, third].map(String::as_str), ["first", "second", "third"]);
    }

    #[test]
    fn a_step_without_an_integration_runner_equivalent_fails_translation() {
        let planned_config = EffectiveConfig::default();
        let file: CaseFile =
            serde_saphyr::from_str(&PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{step: probe}]"))
                .unwrap();
        assert_eq!(
            integration_config(
                &file,
                "alpha",
                Path::new("alpha/config.yaml"),
                |_, _| None,
                |_| Some(&planned_config)
            )
            .unwrap_err(),
            "step 1 'probe' has no integration runner equivalent"
        );
    }
}

//! Compiles loaded cases for a selected subject operating system.

use std::collections::BTreeMap;

use saluki_error::{generic_error, GenericError};

use crate::{
    case::{CaseConfig, CaseFile, CaseSchema, ProcedureNode, Registries},
    layout::{describe_realizations, lay_out, Layout, Os, Realization},
    participant::{find_type, CaseValues, EffectiveConfig, FactDeclaration, ParticipantType},
    record::step_label,
    step::{Need, Step},
};

/// A loaded case with the layout selected for execution.
pub struct CasePlan<C> {
    case: CaseConfig<C>,
    subject_os: Os,
    layout: Option<Layout>,
    participants: BTreeMap<String, PlannedParticipant>,
}

struct PlannedParticipant {
    facts: &'static [FactDeclaration],
    config: EffectiveConfig,
}

impl<C> CasePlan<C> {
    /// Validates every declared subject OS, returning a plan only for a declared selection.
    /// Participant cases carry a layout; typed cases retain their loader-supplied data.
    ///
    /// # Errors
    ///
    /// Returns the first layout, configuration or fact-availability error, naming the case, source and failing
    /// subject OS, even when the selected OS is excluded by the case. On each OS, layout errors precede
    /// configuration errors in participant name order, then fact errors in procedure order with step labels.
    pub fn compile(
        case: CaseConfig<C>, subject_os: Os, registries: &Registries<'_>,
    ) -> Result<Option<Self>, GenericError> {
        let mut layout = None;
        let mut participants = BTreeMap::new();
        if let CaseSchema::Participants { file, procedure } = case.schema() {
            for &os in case.subject_os() {
                let error = |message: String| {
                    generic_error!(
                        "case '{}' in {}: on subject OS '{}', {}",
                        case.name(),
                        case.source().display(),
                        os.name(),
                        message
                    )
                };
                let candidate = lay_out(&file.participants, registries.participant_types, os).map_err(&error)?;
                let mut candidate_participants = BTreeMap::new();
                for (name, realization) in candidate.iter() {
                    let entry = &file.participants[name];
                    let participant_type = find_type(registries.participant_types, &entry.participant_type)
                        .expect("laid-out participants have registered types");
                    let values = CaseValues::for_os(&entry.config, realization.os);
                    participant_type
                        .validate_config(realization, &values)
                        .map_err(|message| {
                            error(format!(
                                "participant '{name}' of type '{}' as {} on {} has invalid configuration: {message}",
                                participant_type.name(),
                                realization.kind.name(),
                                realization.os.name(),
                            ))
                        })?;
                    let defaults = participant_type.default_env(realization, &values);
                    candidate_participants.insert(
                        name.to_string(),
                        PlannedParticipant {
                            facts: participant_type.facts(realization),
                            config: EffectiveConfig::assemble(defaults, values),
                        },
                    );
                }
                check_fact_needs(
                    file,
                    procedure,
                    &candidate,
                    registries.participant_types,
                    &candidate_participants,
                )
                .map_err(error)?;
                if os == subject_os {
                    layout = Some(candidate);
                    participants = candidate_participants;
                }
            }
        }
        if !case.subject_os().contains(&subject_os) {
            return Ok(None);
        }
        Ok(Some(Self {
            case,
            subject_os,
            layout,
            participants,
        }))
    }

    /// Returns the loaded case and its metadata.
    pub fn case(&self) -> &CaseConfig<C> {
        &self.case
    }

    /// Returns the subject OS selected for execution.
    pub fn subject_os(&self) -> Os {
        self.subject_os
    }

    /// Returns `Some(layout)` for participant cases and `None` for typed cases.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Returns a fact from the participant's planned realization.
    /// Returns `None` for unknown participants or facts, and for typed cases.
    pub fn fact(&self, participant: &str, fact: &str) -> Option<&str> {
        self.participants
            .get(participant)?
            .facts
            .iter()
            .find(|declaration| declaration.name == fact)
            .map(|declaration| declaration.value)
    }

    /// Returns the participant's effective configuration for its planned realization.
    /// Returns `None` for unknown participants and typed cases.
    pub fn effective_config(&self, participant: &str) -> Option<&EffectiveConfig> {
        self.participants
            .get(participant)
            .map(|participant| &participant.config)
    }
}

/// Rejects the first step needing an unavailable fact.
fn check_fact_needs(
    file: &CaseFile, procedure: &[ProcedureNode<Box<dyn Step>>], layout: &Layout, types: &[&dyn ParticipantType],
    participants: &BTreeMap<String, PlannedParticipant>,
) -> Result<(), String> {
    for (index, node) in procedure.iter().enumerate() {
        let (steps, parallel) = match node {
            ProcedureNode::Step(step) => (std::slice::from_ref(step), false),
            ProcedureNode::Parallel(steps) => (steps.as_slice(), true),
        };
        for (member, step) in steps.iter().enumerate() {
            for need in step.needs() {
                let Need::Fact(participant, fact) = need else {
                    continue;
                };
                if participants[&participant]
                    .facts
                    .iter()
                    .any(|declaration| declaration.name == fact)
                {
                    continue;
                }
                let realization = layout.get(&participant).expect("step needs name laid-out participants");
                let participant_type = find_type(types, &file.participants[&participant].participant_type)
                    .expect("laid-out participants have registered types");
                let provided =
                    describe_realizations(participant_type.supports().iter().copied().filter(|&(os, kind)| {
                        participant_type
                            .facts(Realization { os, kind })
                            .iter()
                            .any(|declaration| declaration.name == fact)
                    }));
                return Err(format!(
                    "step {} references fact '{fact}' of participant '{participant}', which type '{}' does not provide as {} on {}; it provides it as {provided}",
                    step_label(index, parallel.then_some(member)),
                    participant_type.name(),
                    realization.kind.name(),
                    realization.os.name(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        case::{
            fixtures::{
                write_case, Configured, FixtureConfig, FixtureTypes, PodServer, Server, LOADER, PARTICIPANT_FIELDS,
                REGISTRIES, STEPS,
            },
            CaseLoader, FileSource,
        },
        layout::{Realization, RealizationKind},
    };

    const FACT_CASE: &str = "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: probe, on: subject, path: '{{ subject.facts.file }}'}]\n";

    const CONFIG_REGISTRIES: Registries<'static> = Registries {
        participant_types: &[&Configured, &Server],
        steps: STEPS,
    };
    const CONFIG_LOADER: CaseLoader<'static, FixtureTypes> =
        CaseLoader::new(CONFIG_REGISTRIES, FixtureTypes, "fixture-participants");

    fn compile(yaml: &str, os: Os) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
        let case = CONFIG_LOADER
            .load_content(yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        CasePlan::compile(case, os, &CONFIG_REGISTRIES)
    }

    #[test]
    fn a_plan_holds_each_participants_effective_config_for_the_selected_subject_os() {
        let yaml = r#"timeout: 1s
subject_os: [windows, linux]
participants:
  subject:
    type: configured
    subject: true
    config:
      env: {LEVEL: debug, EXTRA: base}
      os: {windows: {env: {LEVEL: trace, EXTRA: windows}}}
  other: {type: configured, config: {env: {PORT: "2"}}}
procedure: []
"#;
        for (os, level, extra) in [(Os::Linux, "debug", "base"), (Os::Windows, "trace", "windows")] {
            let plan = compile(yaml, os).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([
                        ("LEVEL".into(), level.into()),
                        ("PORT".into(), "1".into()),
                        ("EXTRA".into(), extra.into()),
                    ]),
                    files: BTreeMap::new(),
                })
            );
            assert_eq!(
                plan.effective_config("other"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([("LEVEL".into(), "info".into()), ("PORT".into(), "2".into())]),
                    files: BTreeMap::new(),
                })
            );
            assert_eq!(plan.effective_config("unknown"), None);
        }
    }

    #[test]
    fn a_participant_with_its_own_os_gets_the_overlay_for_that_os() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "participants:",
            "participants:\n  other: {type: configured, os: linux, config: {env: {LEVEL: debug}, os: {linux: {env: {LEVEL: trace}}}}}",
        );
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("other").unwrap().env,
            BTreeMap::from([("LEVEL".into(), "trace".into()), ("PORT".into(), "1".into()),])
        );
        assert_eq!(plan.effective_config("subject"), Some(&EffectiveConfig::default()));
    }

    #[test]
    fn type_defaults_see_the_overlaid_case_values() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]").replace(
            "type: server, subject: true",
            "type: configured, subject: true, config: {os: {windows: {files: {settings.yaml: {from: windows.yaml, template: true}}}}}",
        );
        let source = write_case(dir.path(), "alpha", &yaml);
        std::fs::write(source.parent().unwrap().join("windows.yaml"), "port: 7").unwrap();
        for os in [Os::Linux, Os::Windows] {
            let case = CONFIG_LOADER.load_file(&source).unwrap().remove(0);
            let plan = CasePlan::compile(case, os, &CONFIG_REGISTRIES).unwrap().unwrap();
            let effective = plan.effective_config("subject").unwrap();
            assert_eq!(effective.env["LEVEL"], "info");
            if os == Os::Windows {
                assert_eq!(effective.env.get("PORT"), None);
                assert_eq!(
                    effective.files,
                    BTreeMap::from([(
                        "settings.yaml".into(),
                        FileSource {
                            from: "windows.yaml".into(),
                            template: true,
                        }
                    )])
                );
            } else {
                assert_eq!(effective.env.get("PORT").map(String::as_str), Some("1"));
                assert!(effective.files.is_empty());
            }
        }
    }

    #[test]
    fn a_config_its_type_rejects_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        for config in [
            "{params: {mode: broken}}",
            "{params: {mode: working}, os: {windows: {params: {mode: broken}}}}",
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[windows, linux]").replace(
                "type: server, subject: true",
                &format!("type: configured, subject: true, config: {config}"),
            );
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'subject' of type 'configured' as windows-container on windows has invalid configuration: mode 'broken' is unavailable on windows"
                );
            }
        }
    }

    #[test]
    fn configuration_errors_follow_layout_errors_and_precede_fact_errors() {
        let mut yaml = FACT_CASE
            .replace("[linux]", "[windows]")
            .replace(
                "participants:",
                r#"participants:
  z: {type: server, realization: {kind: linux-container}}
  b: {type: configured, config: {params: {mode: broken}}}
  a: {type: configured, config: {params: {mode: broken}}}
  other: {type: server, os: macos}"#,
            )
            .replace("subject.facts.file", "other.facts.file");
        for (bad, fix, expected) in [
            (
                ", realization: {kind: linux-container}",
                "",
                "participant 'z' of type 'server' cannot run as linux-container on windows",
            ),
            (
                "a: {type: configured, config: {params: {mode: broken}}}",
                "a: {type: configured}",
                "participant 'a' of type 'configured' as windows-container on windows has invalid configuration",
            ),
            (
                "b: {type: configured, config: {params: {mode: broken}}}",
                "b: {type: configured}",
                "participant 'b' of type 'configured' as windows-container on windows has invalid configuration",
            ),
            (
                "other.facts.file",
                "subject.facts.file",
                "step 1 references fact 'file' of participant 'other'",
            ),
        ] {
            let message = compile(&yaml, Os::Windows).err().unwrap().to_string();
            assert!(message.contains(expected), "{message}");
            assert!(yaml.contains(bad));
            yaml = yaml.replace(bad, fix);
        }
        assert_eq!(
            compile(&yaml, Os::Windows).unwrap().unwrap().fact("subject", "file"),
            Some(r"C:\logs\output.log")
        );
    }

    #[test]
    fn a_typed_plan_has_no_effective_configs() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject"), None);
    }

    #[test]
    fn facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        for declared in ["[windows, macos, linux]", "[macos]"] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let message = compile(&FACT_CASE.replace("[linux]", declared), os)
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(message,
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references fact 'file' of participant 'subject', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
                );
            }
        }
        assert!(compile(&FACT_CASE.replace("[linux]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_fact_provided_for_one_realization_is_missing_for_another_realization_on_the_same_os() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE.replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = CasePlan::compile(case, Os::Linux, &registries)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 references fact 'file' of participant 'subject', which type 'pod-server' does not provide as kubernetes-pod on linux; it provides it as linux-container on linux"
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(case, Os::Linux, &registries).unwrap().unwrap();
        assert_eq!(plan.fact("subject", "file"), Some("/logs/output.log"));
    }

    #[test]
    fn a_participant_with_its_own_os_needs_facts_only_from_its_realization_on_that_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace("participants:", "participants:\n  other: {type: server, os: linux}")
            .replace("subject.facts.file", "other.facts.file");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.fact("other", "file"), Some("/logs/output.log"));
        assert_eq!(plan.fact("subject", "file"), None);
        let message = compile(&yaml.replace(", os: linux", ""), Os::MacOs)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references fact 'file' of participant 'other', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
        );
    }

    #[test]
    fn a_plan_resolves_facts_from_each_participants_realization_on_the_selected_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, windows]")
            .replace("participants:", "participants:\n  other: {type: server, os: linux}");
        for (os, expected) in [(Os::Linux, "/logs/output.log"), (Os::Windows, r"C:\logs\output.log")] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.fact("subject", "file"), Some(expected));
            assert_eq!(plan.fact("other", "file"), Some("/logs/output.log"));
            assert_eq!(plan.fact("subject", "unknown"), None);
            assert_eq!(plan.fact("unknown", "file"), None);
        }
    }

    #[test]
    fn a_typed_plan_resolves_no_facts() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.fact("subject", "file"), None);
    }

    #[test]
    fn a_layout_error_on_a_subject_os_is_reported_before_its_fact_errors() {
        let yaml = FACT_CASE.replace("[linux]", "[macos]").replace(
            "participants:",
            "participants:\n  other: {type: server, realization: {kind: linux-container}}",
        );
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'other' of type 'server' cannot run as linux-container on macos; it supports linux-container on linux, host-process-group on macos, windows-container on windows"
        );
        assert!(
            compile(&yaml.replace(", realization: {kind: linux-container}", ""), Os::MacOs)
                .err()
                .unwrap()
                .to_string()
                .contains("step 1 references fact 'file'")
        );
    }

    #[test]
    fn fact_errors_follow_subject_os_order_then_procedure_order_including_parallel_members() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[macos, linux]")
            .replace("participants:", "participants:\n  other: {type: server, os: macos}")
            .replace("procedure: []", "procedure:\n  - {step: probe, on: subject, path: '{{ subject.facts.file }}'}\n  - parallel:\n    - {step: probe, on: subject}\n    - {step: probe, on: subject, path: '{{ other.facts.file }}'}\n  - {step: probe, on: subject, path: '{{ other.facts.file }}'}");
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert!(
            message.contains("on subject OS 'linux', step 2.2 references fact 'file' of participant 'other'"),
            "{message}"
        );
        let message = compile(&yaml.replace("{{ other.facts.file }}", "literal"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("on subject OS 'macos', step 1 references fact 'file' of participant 'subject'"),
            "{message}"
        );
    }

    #[test]
    fn a_participant_case_is_planned_only_for_a_subject_os_it_declares() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for (os, kind) in [
            (Os::Linux, Some(RealizationKind::LinuxContainer)),
            (Os::MacOs, None),
            (Os::Windows, Some(RealizationKind::WindowsContainer)),
        ] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
            if let Some(kind) = kind {
                let plan = plan.unwrap();
                assert_eq!(plan.case().name(), "alpha");
                assert_eq!(plan.subject_os(), os);
                assert_eq!(
                    plan.layout().unwrap().iter().collect::<Vec<_>>(),
                    [("subject", Realization { os, kind })]
                );
            } else {
                assert!(plan.is_none());
            }
        }
    }

    #[test]
    fn a_layout_error_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, windows]")
            .replace("subject: true", "subject: true, realization: {kind: linux-container}");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let message = CasePlan::compile(case, os, &REGISTRIES).err().unwrap().to_string();
            assert!(
                message.contains(
                    "on subject OS 'windows', participant 'subject' of type 'server' \
                     cannot run as linux-container on windows"
                ),
                "{message}"
            );
        }
    }

    #[test]
    fn a_typed_case_is_planned_without_a_layout_for_its_declared_subject_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content("type: fixture", "alpha", Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
            if os == Os::Linux {
                assert!(plan.is_none());
            } else {
                let plan = plan.unwrap();
                assert_eq!(plan.subject_os(), os);
                assert_eq!(plan.case().name(), "alpha");
                assert!(plan.layout().is_none());
            }
        }
    }

    #[test]
    fn compile_errors_name_the_case_source_and_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        let case = LOADER
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = CasePlan::compile(case, Os::Linux, &REGISTRIES)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', \
             participant 'subject' of type 'server' cannot run as kubernetes-pod on linux; \
             it supports linux-container on linux, host-process-group on macos, windows-container on windows"
        );
    }
}

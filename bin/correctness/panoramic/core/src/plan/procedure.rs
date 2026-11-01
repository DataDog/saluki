//! Checks a case's procedure steps against each subject OS's plan.

use std::collections::BTreeMap;

use super::{
    references::{late_fact_timing, planned_fact},
    PlannedParticipant,
};
use crate::{
    case::{CaseFile, ProcedureNode},
    layout::Layout,
    participant::{find_type, ParticipantType},
    record::step_label,
    step::{Need, Step},
};

/// Checks fact and late-fact needs against the layout.
///
/// Participant and process needs were checked at load and are independent of the layout.
///
/// # Errors
///
/// Rejects the first step needing an unavailable fact or late fact.
pub(super) fn check_step_references(
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
                let result = match need {
                    Need::Fact(participant, fact) => {
                        let realization = layout.get(&participant).expect("step needs name laid-out participants");
                        let participant_type = find_type(types, &file.participants[&participant].participant_type)
                            .expect("laid-out participants have registered types");
                        planned_fact(
                            &participant,
                            participant_type,
                            realization,
                            &participants[&participant].facts,
                            &fact,
                        )
                        .map(|_| ())
                    }
                    Need::Late(participant, fact) => {
                        let realization = layout.get(&participant).expect("step needs name laid-out participants");
                        late_fact_timing(&participant, realization.kind, fact).map(|_| ())
                    }
                    Need::Handle(_) | Need::Process(_, _) => continue,
                };
                result.map_err(|reason| format!("step {} {reason}", step_label(index, parallel.then_some(member))))?;
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
        backend,
        case::{
            fixtures::{minimal_procedure, FixtureTypes, PodServer, PARTICIPANT_FIELDS, STEPS},
            CaseLoader, CaseSchema, Registries,
        },
        layout::{Os, RealizationKind},
        plan::{
            fixtures::{compile, FACT_CASE},
            Planner,
        },
    };

    #[test]
    fn undeclared_step_facts_fail_at_plan_time() {
        for (participant, participant_type, fact) in [("subject", "server", "missing"), ("other", "client", "file")] {
            let yaml = FACT_CASE
                .replace(
                    "participants:",
                    "participants:\n  other: {type: client, connect: {upstream: subject.api}}",
                )
                .replace("subject.facts.file", &format!("{participant}.facts.{fact}"));
            let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
            assert_eq!(message, format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 references fact '{fact}' of participant '{participant}', which type '{participant_type}' does not provide as linux-container on linux; it provides it as none"
            ));
        }
    }

    #[test]
    fn a_step_and_a_configuration_reference_to_a_fact_the_realization_lacks_report_the_same_reason() {
        for fact in ["file", "missing"] {
            let step_yaml = FACT_CASE
                .replace("[linux]", "[macos]")
                .replace("subject.facts.file", &format!("subject.facts.{fact}"));
            let config_yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
                "subject: true",
                &format!("subject: true, config: {{env: {{KEY: '{{{{ subject.facts.{fact} }}}}'}}}}"),
            );
            let step_error = compile(&step_yaml, Os::MacOs).err().unwrap().to_string();
            let config_error = compile(&config_yaml, Os::MacOs).err().unwrap().to_string();
            let step_reason = step_error
                .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 ")
                .unwrap();
            let config_reason = config_error
                .strip_prefix(&format!("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'server' as host-process-group on macos environment variable 'KEY' '{{{{ subject.facts.{fact} }}}}' "))
                .unwrap();
            assert_eq!(step_reason, config_reason);
        }
    }

    #[test]
    fn a_step_and_a_configuration_reference_to_a_late_fact_the_realization_lacks_report_the_same_reason() {
        let step_yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace("subject.facts.file", "subject.late.container_id");
        let config_yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ subject.late.container_id }}'}}",
        );
        let step_error = compile(&step_yaml, Os::MacOs).err().unwrap().to_string();
        let config_error = compile(&config_yaml, Os::MacOs).err().unwrap().to_string();
        let step_reason = step_error
            .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 ")
            .unwrap();
        let config_reason = config_error
            .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'server' as host-process-group on macos environment variable 'KEY' '{{ subject.late.container_id }}' ")
            .unwrap();
        assert_eq!(step_reason, config_reason);
        assert!(step_reason.ends_with("; a host-process-group has ip"));
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
    fn late_facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos]")
            .replace("subject.facts.file", "subject.late.container_id");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references late fact 'container_id' of participant 'subject', which a host-process-group does not have; a host-process-group has ip"
            );
        }
        assert!(compile(&yaml.replace("[linux, macos]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_late_fact_every_realization_has_compiles_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos, windows]")
            .replace("subject.facts.file", "subject.late.ip");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.subject_os(), os);
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("expected participant data");
            };
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step");
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("subject".into()),
                    Need::Late("subject".into(), backend::LateFact::Ip)
                ]
            );
        }
    }

    #[test]
    fn pod_uid_is_available_only_to_a_kubernetes_pod() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
            ..Default::default()
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE
            .replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}")
            .replace("subject.facts.file", "subject.late.pod_uid");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(
            plan.layout().unwrap().get("subject").unwrap().kind,
            RealizationKind::KubernetesPod
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("cases/alpha/config.yaml"),
            )
            .unwrap()
            .remove(0);
        assert_eq!(Planner::with_run(registries, "testrun0").compile(case, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip"
        );
    }

    #[test]
    fn fact_and_late_fact_errors_follow_procedure_order_including_parallel_members() {
        let mut yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            minimal_procedure!(),
            r#"procedure:
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}{{ subject.facts.file }}'}
  - parallel:
    - {step: probe, on: subject, path: '{{ subject.late.ip }}'}
    - {step: probe, on: subject, path: '{{ subject.facts.file }}{{ subject.late.pod_uid }}'}
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}'}"#,
        );
        for (reference, label, kind, fact) in [
            ("{{ subject.late.container_id }}", "1", "late fact", "container_id"),
            ("{{ subject.facts.file }}", "1", "fact", "file"),
            ("{{ subject.facts.file }}", "2.2", "fact", "file"),
            ("{{ subject.late.pod_uid }}", "2.2", "late fact", "pod_uid"),
            ("{{ subject.late.container_id }}", "3", "late fact", "container_id"),
        ] {
            let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
            assert!(
                message.contains(&format!(
                    "on subject OS 'macos', step {label} references {kind} '{fact}' of participant 'subject'"
                )),
                "{message}"
            );
            yaml = yaml.replacen(reference, "literal", 1);
        }
        assert!(compile(&yaml, Os::MacOs).unwrap().is_some());
    }

    #[test]
    fn a_fact_provided_for_one_realization_is_missing_for_another_realization_on_the_same_os() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
            ..Default::default()
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE.replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
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
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(plan.fact("subject", "file"), Some("/logs/output.log"));
    }

    #[test]
    fn a_participant_with_its_own_os_needs_facts_only_from_its_realization_on_that_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace(
                "participants:",
                "participants:\n  other: {type: server, os: linux, config: {env: {PORT: '8081', METRICS: 'off'}}}",
            )
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
    fn fact_errors_follow_subject_os_order_then_procedure_order_including_parallel_members() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[macos, linux]")
            .replace("participants:", "participants:\n  other: {type: server, os: macos, config: {env: {PORT: '8081', METRICS: 'off'}}}")
            .replace(minimal_procedure!(), "procedure:\n  - {step: probe, on: subject, path: '{{ subject.facts.file }}'}\n  - parallel:\n    - {step: probe, on: subject}\n    - {step: probe, on: subject, path: '{{ other.facts.file }}'}\n  - {step: probe, on: subject, path: '{{ other.facts.file }}'}");
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
}

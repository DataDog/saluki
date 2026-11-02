//! Builds case procedure steps through their factories and checks what each step references.

use std::path::Path;

use saluki_error::{generic_error, GenericError};
use serde_json::Value;

use super::{
    file::{CaseFile, ProcedureNode, StepNode},
    participant_check::declared_names,
};
use crate::{
    participant::{find_type, ParticipantType},
    record::step_label,
    step::{Need, Step, StepFactory},
};

/// Builds every step of the procedure, keeping its order and parallel blocks.
///
/// # Errors
///
/// Returns the first error in procedure order, naming the case, source, step label and step.
/// Checks registration, field decoding, template references, then needs in their declared order.
pub(super) fn build_steps(
    file: &CaseFile, steps: &[&dyn StepFactory], types: &[&dyn ParticipantType], name: &str, source: &Path,
) -> Result<Vec<ProcedureNode<Box<dyn Step>>>, GenericError> {
    file.procedure
        .iter()
        .enumerate()
        .map(|(index, node)| match node {
            ProcedureNode::Step(node) => {
                build_step(node, &step_label(index, None), file, steps, types, name, source).map(ProcedureNode::Step)
            }
            ProcedureNode::Parallel(nodes) => nodes
                .iter()
                .enumerate()
                .map(|(member, node)| {
                    build_step(node, &step_label(index, Some(member)), file, steps, types, name, source)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(ProcedureNode::Parallel),
        })
        .collect()
}

fn build_step(
    node: &StepNode, label: &str, file: &CaseFile, steps: &[&dyn StepFactory], types: &[&dyn ParticipantType],
    name: &str, source: &Path,
) -> Result<Box<dyn Step>, GenericError> {
    let error = |message: String| generic_error!("case '{}' in {}: step {} {}", name, source.display(), label, message);
    let Some(factory) = steps.iter().find(|factory| factory.name() == node.name) else {
        return Err(error(format!(
            "names unknown step '{}'; registered steps: {}",
            node.name,
            declared_names(&steps.iter().map(|factory| factory.name()).collect::<Vec<_>>())
        )));
    };
    let step = factory
        .build(&node.fields)
        .map_err(|message| error(format!("'{}': {message}", node.name)))?;
    for (field, value) in &node.fields {
        if !factory.template_fields().contains(&field.as_str()) && contains_template_reference(value) {
            return Err(error(format!(
                "'{}': field '{field}' contains a template reference ('{{{{'); \
                 '{}' accepts references only in: {}",
                node.name,
                node.name,
                declared_names(factory.template_fields())
            )));
        }
    }
    for need in step.needs() {
        let participant = match &need {
            Need::Handle(participant)
            | Need::Process(participant, _)
            | Need::Fact(participant, _)
            | Need::Late(participant, _)
            | Need::Endpoint(participant, _) => participant,
        };
        let Some(entry) = file.participants.get(participant) else {
            let usage = match &need {
                Need::Fact(_, fact) => format!("references fact '{fact}' of"),
                Need::Late(_, fact) => format!("references late fact '{}' of", fact.name()),
                Need::Endpoint(_, endpoint) => format!("reaches endpoint '{endpoint}' of"),
                Need::Handle(_) | Need::Process(_, _) => "runs on".into(),
            };
            return Err(error(format!(
                "'{}' {usage} unknown participant '{participant}'; participants: {}",
                node.name,
                file.participants
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let Some(participant_type) = find_type(types, &entry.participant_type) else {
            return Err(error(format!(
                "'{}': participant '{participant}' has unknown type '{}'; registered types: {}",
                node.name,
                entry.participant_type,
                declared_names(
                    &types
                        .iter()
                        .map(|participant_type| participant_type.name())
                        .collect::<Vec<_>>()
                )
            )));
        };
        if let Need::Process(_, process) = &need {
            if !participant_type.processes().contains(&process.as_str()) {
                return Err(error(format!(
                    "'{}' names process '{process}' of participant '{participant}', \
                     which type '{}' does not run; it runs {}",
                    node.name,
                    participant_type.name(),
                    declared_names(participant_type.processes())
                )));
            }
        }
    }
    Ok(step)
}

fn contains_template_reference(value: &Value) -> bool {
    match value {
        Value::String(value) => value.contains("{{"),
        Value::Array(values) => values.iter().any(contains_template_reference),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains("{{") || contains_template_reference(value)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{Probe, Server, STEPS, TYPES};
    use super::*;
    use crate::{
        backend::LateFact,
        step::{decode_fields, StepFields},
    };

    fn case(procedure: &str) -> CaseFile {
        serde_saphyr::from_str(&format!(
            "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: server, subject: true}}\n  other: {{type: client}}\nprocedure:\n{procedure}\n"
        ))
        .unwrap()
    }

    fn build(procedure: &str) -> Result<Vec<ProcedureNode<Box<dyn Step>>>, GenericError> {
        build_steps(
            &case(procedure),
            STEPS,
            TYPES,
            "alpha",
            Path::new("cases/alpha/config.yaml"),
        )
    }

    #[track_caller]
    fn build_error(procedure: &str) -> String {
        let message = build(procedure).unwrap_err().to_string();
        assert!(
            message.starts_with("case 'alpha' in cases/alpha/config.yaml: step "),
            "{message}"
        );
        message
    }

    #[test]
    fn an_unregistered_step_fails_and_lists_the_registered_steps() {
        let message = build_error("- {step: missing, on: subject}");
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: step 1 names unknown step 'missing'; registered steps: marker, probe, noop, reach"
        );
    }

    #[test]
    fn step_errors_name_the_case_source_label_and_step_name() {
        for (procedure, label) in [
            ("- {step: probe, on: subject}\n- {step: probe}", "2"),
            (
                "- {step: probe, on: subject}\n- {step: probe, on: subject}\n- parallel: [{step: probe}]",
                "3.1",
            ),
        ] {
            let message = build_error(procedure);
            assert!(
                message.starts_with(&format!(
                    "case 'alpha' in cases/alpha/config.yaml: step {label} 'probe': "
                )),
                "{message}"
            );
            assert!(message.contains("missing field `on`"), "{message}");
        }
    }

    #[test]
    fn a_step_on_an_unknown_participant_fails_and_lists_the_participants() {
        for fields in ["on: missing", "on: missing, process: server"] {
            let message = build_error(&format!("- {{step: probe, {fields}}}"));
            assert!(
                message.ends_with("'probe' runs on unknown participant 'missing'; participants: other, subject"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_step_reaching_an_endpoint_of_an_unknown_participant_fails_loading() {
        assert_eq!(build_error("- {step: reach, endpoint: missing.api}"),
            "case 'alpha' in cases/alpha/config.yaml: step 1 'reach' reaches endpoint 'api' of unknown participant 'missing'; participants: other, subject");
        let procedure = build("- {step: reach, endpoint: subject.api}").unwrap();
        let [ProcedureNode::Step(step)] = procedure.as_slice() else {
            panic!("expected one built step");
        };
        assert_eq!(step.needs(), [Need::Endpoint("subject".into(), "api".into())]);
    }

    #[test]
    fn a_process_its_participant_type_does_not_run_fails_and_lists_its_processes() {
        for (participant, participant_type, processes) in
            [("subject", "server", "server, worker"), ("other", "client", "none")]
        {
            let message = build_error(&format!("- {{step: probe, on: {participant}, process: missing}}"));
            assert!(
                message.ends_with(&format!(
                    "'probe' names process 'missing' of participant '{participant}', which type '{participant_type}' does not run; it runs {processes}"
                )),
                "{message}"
            );
        }
        for process in Server.processes() {
            let procedure = build(&format!("- {{step: probe, on: subject, process: {process}}}")).unwrap();
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one built step");
            };
            assert_eq!(step.needs(), vec![Need::Process("subject".into(), (*process).into())]);
        }
    }

    #[test]
    fn a_template_reference_in_an_undeclared_field_fails_naming_the_field_and_accepted_fields() {
        for (fields, field) in [
            ("on: '{{ subject }}'", "on"),
            ("on: subject, process: '{{ process }}'", "process"),
            ("on: subject, values: ['prefix {{ value }} suffix']", "values"),
            ("on: subject, values: ['{{ subject.late.ip }}']", "values"),
            ("on: subject, values: [{'{{ x }}': 1}]", "values"),
            (
                "on: subject, values: [{nested: [false, 3, null, '{{ value }}']}]",
                "values",
            ),
        ] {
            let message = build_error(&format!("- {{step: probe, {fields}}}"));
            assert!(
                message.ends_with(&format!(
                    "'probe': field '{field}' contains a template reference ('{{{{'); 'probe' accepts references only in: path"
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn a_factory_without_template_fields_accepts_no_references() {
        let message = build_error("- {step: marker, on: '{{ subject }}'}");
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: step 1 'marker': field 'on' contains a template reference ('{{'); 'marker' accepts references only in: none"
        );
    }

    #[test]
    fn fact_references_in_declared_template_fields_build_and_report_needs() {
        let procedure = build("- {step: probe, on: subject, path: '{{ subject.facts.file }}'}").unwrap();
        let [ProcedureNode::Step(step)] = procedure.as_slice() else {
            panic!("expected one step")
        };
        assert_eq!(
            step.needs(),
            [
                Need::Handle("subject".into()),
                Need::Fact("subject".into(), "file".into())
            ]
        );
    }

    #[test]
    fn late_fact_references_in_declared_template_fields_build_and_report_needs() {
        for fact in [LateFact::ContainerId, LateFact::Ip, LateFact::PodUid] {
            let procedure = build(&format!(
                "- {{step: probe, on: subject, path: '{{{{ other.late.{} }}}}'}}",
                fact.name()
            ))
            .unwrap();
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step");
            };
            assert_eq!(
                step.needs(),
                [Need::Handle("subject".into()), Need::Late("other".into(), fact)]
            );
        }
    }

    #[test]
    fn late_fact_references_require_a_known_participant() {
        let message = build_error("- {step: probe, on: subject, path: '{{ missing.late.ip }}'}");
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: step 1 'probe' references late fact 'ip' of unknown participant 'missing'; participants: other, subject"
        );
    }

    #[test]
    fn fact_references_require_a_known_participant() {
        let message = build_error("- {step: probe, on: subject, path: '{{ missing.facts.file }}'}");
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: step 1 'probe' references fact 'file' of unknown participant 'missing'; participants: other, subject"
        );
    }

    #[test]
    fn the_first_failing_step_in_procedure_order_is_reported() {
        let mut procedure = "- {step: probe, on: missing}\n- parallel:\n  - {step: probe, on: subject}\n  - {step: probe, on: missing}\n  - {step: unknown}\n- {step: unknown}".to_string();
        for label in [
            "1 'probe'",
            "2.2 'probe'",
            "2.3 names unknown step 'unknown'",
            "3 names unknown step 'unknown'",
        ] {
            let message = build_error(&procedure);
            assert!(message.contains(&format!("step {label}")), "{message}");
            procedure = if procedure.contains("on: missing") {
                procedure.replacen("on: missing", "on: subject", 1)
            } else {
                procedure.replacen("step: unknown", "step: probe, on: subject", 1)
            };
        }
        assert_eq!(build(&procedure).unwrap().len(), 3);
    }

    #[test]
    fn checks_report_build_errors_before_template_references_and_needs() {
        let procedure = "- {step: probe, on: missing, values: ['{{ unresolved }}'], unknown: x}";
        let message = build_error(procedure);
        assert!(message.contains("unknown field `unknown`"), "{message}");
        let procedure = procedure.replace(", unknown: x", "");
        let message = build_error(&procedure);
        assert_eq!(message, "case 'alpha' in cases/alpha/config.yaml: step 1 'probe': field 'values' contains a template reference ('{{'); 'probe' accepts references only in: path");
        let message = build_error(&procedure.replace("{{ unresolved }}", "literal"));
        assert!(message.contains("runs on unknown participant 'missing'"), "{message}");
    }

    #[test]
    fn built_steps_keep_procedure_order_and_parallel_blocks() {
        let procedure = build(
            "- {step: probe, on: subject, values: [first]}\n- parallel:\n  - {step: probe, on: other, values: [second]}\n  - {step: probe, on: subject, process: worker, values: [third]}\n- {step: probe, on: other, values: [last, {nested: [false, 3, null, literal]}]}",
        )
        .unwrap();
        let [ProcedureNode::Step(first), ProcedureNode::Parallel(members), ProcedureNode::Step(last)] =
            procedure.as_slice()
        else {
            panic!("expected step, parallel, step");
        };
        assert_eq!(members.len(), 2);
        for (step, participant, process, values) in [
            (first, "subject", None, vec![serde_json::json!("first")]),
            (&members[0], "other", None, vec![serde_json::json!("second")]),
            (&members[1], "subject", Some("worker"), vec![serde_json::json!("third")]),
            (
                last,
                "other",
                None,
                vec![
                    serde_json::json!("last"),
                    serde_json::json!({"nested": [false, 3, null, "literal"]}),
                ],
            ),
        ] {
            let expected = Probe {
                on: participant.into(),
                process: process.map(String::from),
                values,
                path: None,
            };
            assert_eq!(format!("{step:?}"), format!("{expected:?}"));
        }
    }

    #[test]
    fn decode_fields_names_the_field_with_the_wrong_type() {
        let fields: StepFields = serde_saphyr::from_str("on: true").unwrap();
        let message = decode_fields::<Probe>(&fields).unwrap_err();
        assert_eq!(message, "on: invalid type: boolean `true`, expected a string");
    }

    #[test]
    fn a_need_for_a_participant_with_an_unknown_type_fails_without_panicking() {
        for fields in [
            "on: subject",
            "on: subject, process: server",
            "on: other, path: '{{ subject.facts.file }}'",
        ] {
            let mut file = case(&format!("- {{step: probe, {fields}}}"));
            file.participants.get_mut("subject").unwrap().participant_type = "unknown".into();
            let message = build_steps(&file, STEPS, TYPES, "alpha", Path::new("config.yaml"))
                .unwrap_err()
                .to_string();
            assert_eq!(
                message,
                "case 'alpha' in config.yaml: step 1 'probe': participant 'subject' has unknown type 'unknown'; registered types: client, server"
            );
        }
    }
}

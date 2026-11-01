//! Checks fact availability and configuration references, substituting facts available from the layout.

use std::collections::BTreeMap;

use super::{
    endpoints::ParticipantEndpoints,
    ports::RunnerHostPorts,
    routes::{route, Route},
    scope::check_consumed_socket,
};
use crate::{
    backend::{late_facts, writes_late_files, LateFact, LateFactTiming},
    case::{
        config_references::{walk_config_strings, walk_config_strings_mut, ConfigField, FileTemplates},
        declared_names,
        template::{AddressPart, Reference, Template},
        CaseFile,
    },
    endpoint::{EndpointKind, Listener},
    layout::{describe_participant, describe_realizations, Layout, Realization, RealizationKind},
    participant::{find_type, CaseValues, ParticipantType},
};

/// Returns `participant`'s planned value of early fact `fact`.
///
/// # Errors
///
/// If the fact is missing, names the realizations on which the participant's type provides it.
pub(super) fn planned_fact<'f>(
    participant: &str, participant_type: &dyn ParticipantType, realization: Realization,
    facts: &'f BTreeMap<String, String>, fact: &str,
) -> Result<&'f str, String> {
    if let Some(value) = facts.get(fact) {
        return Ok(value);
    }
    let provided = describe_realizations(participant_type.supports().iter().copied().filter(|&(os, kind)| {
        participant_type
            .facts(Realization { os, kind })
            .iter()
            .any(|value| value.name == fact)
    }));
    Err(format!(
        "references fact '{fact}' of participant '{participant}', which type '{}' does not provide as {} on {}; it provides it as {provided}",
        participant_type.name(), realization.kind.name(), realization.os.name(),
    ))
}

/// Returns when a participant realized as `kind` can supply late fact `fact`.
///
/// # Errors
///
/// If the fact is missing, lists the late facts the realization kind has.
pub(super) fn late_fact_timing(
    participant: &str, kind: RealizationKind, fact: LateFact,
) -> Result<LateFactTiming, String> {
    let facts = late_facts(kind);
    if let Some((_, timing)) = facts.iter().find(|(provided, _)| *provided == fact) {
        return Ok(*timing);
    }
    let kind = kind.name();
    Err(format!(
        "references late fact '{}' of participant '{participant}', which a {kind} does not have; a {kind} has {}",
        fact.name(),
        declared_names(&facts.iter().map(|(fact, _)| fact.name()).collect::<Vec<_>>()),
    ))
}

pub(super) fn resolve_config_references(
    file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType],
    facts: &BTreeMap<String, BTreeMap<String, String>>, name: &str, values: &mut CaseValues,
) -> Result<(), String> {
    let realization = layout.get(name).expect("loaded participants have a layout");
    let holder = describe_participant(name, &file.participants[name].participant_type, realization);
    walk_config_strings_mut(&mut values.env, &mut values.params, |field, text| {
        let template: Template = text.parse().expect("configuration templates were checked at load");
        *text = template
            .substitute(|written, reference| {
                resolve_reference(file, layout, types, facts, name, field, reference)
                    .map_err(|reason| format!("'{written}' {reason}"))
            })
            .map_err(|reason| format!("{holder} {field} {reason}"))?;
        Ok(())
    })
}

pub(super) fn check_file_references(
    file: &CaseFile, templates: &FileTemplates, layout: &Layout, types: &[&dyn ParticipantType],
    facts: &BTreeMap<String, BTreeMap<String, String>>, name: &str, values: &CaseValues,
) -> Result<(), String> {
    let realization = layout.get(name).expect("loaded participants have a layout");
    let holder = describe_participant(name, &file.participants[name].participant_type, realization);
    for (file_name, source) in values.files.iter().filter(|(_, source)| source.template) {
        let field = ConfigField::File(file_name);
        for (written, reference) in templates.get(&source.from).written_references() {
            resolve_reference(file, layout, types, facts, name, field, reference)
                .map_err(|reason| format!("{holder} {field} '{written}' {reason}"))?;
        }
    }
    Ok(())
}

fn resolve_reference(
    file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType],
    facts: &BTreeMap<String, BTreeMap<String, String>>, holder: &str, field: ConfigField<'_>, reference: &Reference,
) -> Result<Option<String>, String> {
    let (participant, fact) = match reference {
        Reference::Fact { participant, fact } => (participant, Some(fact)),
        Reference::Late { participant, .. } => (participant, None),
        Reference::Endpoint { .. } | Reference::Connection { .. } => return Ok(None),
    };
    let name = participant.name(holder);
    let realization = layout
        .get(name)
        .expect("configuration references name laid-out participants");
    let participant_type = find_type(types, &file.participants[name].participant_type)
        .expect("laid-out participants have registered types");
    if let Some(fact) = fact {
        return planned_fact(name, participant_type, realization, &facts[name], fact).map(|value| Some(value.into()));
    }
    let Reference::Late { fact, .. } = reference else {
        unreachable!("only late references remain");
    };
    let fact_name = fact.name();
    let kind = realization.kind.name();
    let timing = late_fact_timing(name, realization.kind, *fact)?;
    if let LateFactTiming::Fixed(value) = timing {
        let holder_realization = layout.get(holder).expect("loaded participants have a layout");
        if holder_realization != realization {
            return Err(format!(
                "references late fact '{fact_name}' of participant '{name}', which a {kind} on {} reports only to participants on its own host",
                realization.os.name(),
            ));
        }
        return Ok(Some(value.into()));
    }
    if matches!(field, ConfigField::File(_)) {
        let holder_kind = layout.get(holder).expect("loaded participants have a layout").kind;
        if writes_late_files(holder_kind) {
            return Ok(None);
        }
        if name == holder {
            return Err(format!(
                "references its own late fact '{fact_name}', which a configuration file of a {} cannot wait for",
                holder_kind.name(),
            ));
        }
    }
    if name == holder {
        if matches!(field, ConfigField::Param(_)) {
            return Err(format!(
                "references its own late fact '{fact_name}', which a parameter cannot wait for"
            ));
        }
        if participant_type.substituted_late_facts(realization).contains(fact) {
            return Ok(None);
        }
        return Err(format!(
            "references its own late fact '{fact_name}', which type '{}' does not substitute into its environment as {kind}",
            participant_type.name(),
        ));
    }
    if *fact != LateFact::ContainerId {
        return Err(format!(
            "references late fact '{fact_name}' of participant '{name}'; configuration can use another participant's late fact only as the container_id of a participant known once created"
        ));
    }
    match timing {
        LateFactTiming::AfterCreate => Ok(None),
        LateFactTiming::AfterStart => Err(format!(
            "references late fact '{fact_name}' of participant '{name}', which a {kind} knows only after it starts"
        )),
        LateFactTiming::Fixed(_) => unreachable!("fixed facts were substituted"),
    }
}

pub(super) fn check_address_references(
    file: &CaseFile, templates: &FileTemplates, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
    ports: &mut RunnerHostPorts<'_>,
) -> Result<(), String> {
    for (name, participant) in endpoints {
        walk_config_strings(&participant.values.env, &participant.values.params, |field, text| {
            let template: Template = text
                .parse()
                .expect("substituted configuration retains valid references");
            check_template_addresses(file, layout, endpoints, name, field, &template, ports)
        })?;
        for (file_name, source) in participant.values.files.iter().filter(|(_, source)| source.template) {
            check_template_addresses(
                file,
                layout,
                endpoints,
                name,
                ConfigField::File(file_name),
                templates.get(&source.from),
                ports,
            )?;
        }
    }
    Ok(())
}

fn check_template_addresses(
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>, name: &str,
    field: ConfigField<'_>, template: &Template, ports: &mut RunnerHostPorts<'_>,
) -> Result<(), String> {
    let realization = layout.get(name).expect("loaded participants have a layout");
    let entry = &file.participants[name];
    let holder = describe_participant(name, &entry.participant_type, realization);
    for (written, reference) in template.written_references() {
        let context = format!("{holder} {field} '{written}'");
        let (kind, part, allocation_target) = match reference {
            Reference::Endpoint {
                participant,
                endpoint,
                part,
            } => {
                let target = participant.name(name);
                let served = &endpoints[target].served;
                let Some(served_endpoint) = served.get(endpoint) else {
                    return Err(format!(
                        "{context} references endpoint '{endpoint}' of participant '{target}', which it does not serve; it serves {}",
                        declared_names(&served.keys().map(String::as_str).collect::<Vec<_>>()),
                    ));
                };
                let allocation_target = if target != name {
                    let target_realization = layout.get(target).expect("referenced participants have a layout");
                    let route = route(served_endpoint.kind, realization, target_realization).map_err(|constraint| format!(
                        "{context} references endpoint '{endpoint}' of participant '{target}', which it cannot reach: {constraint}"
                    ))?;
                    if route == Route::SharedFilesystem {
                        let Listener::Path(path) = &served_endpoint.listener else {
                            unreachable!("shared-filesystem routes use socket paths");
                        };
                        let server = &endpoints[target];
                        check_consumed_socket(path, &server.shared_dir, server.os).map_err(|reason| format!(
                            "{context} references endpoint '{endpoint}' of participant '{target}', a {} socket served at '{path}'{reason}",
                            served_endpoint.kind.name()
                        ))?;
                    }
                    (route == Route::RunnerHostPort).then_some((target, endpoint))
                } else {
                    None
                };
                (served_endpoint.kind, part, allocation_target)
            }
            Reference::Connection { consumed, part } => {
                let target = &entry.connect[consumed];
                (endpoints[&target.participant].served[&target.endpoint].kind, part, None)
            }
            _ => continue,
        };
        let (allowed, parts) = match kind {
            EndpointKind::Tcp | EndpointKind::Udp | EndpointKind::Http | EndpointKind::Grpc => (
                matches!(part, AddressPart::Url | AddressPart::Host | AddressPart::Port),
                "url, host or port",
            ),
            EndpointKind::UnixDatagram | EndpointKind::UnixStream | EndpointKind::NamedPipe => {
                (matches!(part, AddressPart::Url | AddressPart::Path), "url or path")
            }
        };
        if !allowed {
            return Err(format!(
                "{context}: a {} endpoint has no '{part}'; use {parts}",
                kind.name()
            ));
        }
        if let Some((target, endpoint)) = allocation_target {
            ports.for_endpoint(target, endpoint).map_err(|error| format!(
                "{context} references endpoint '{endpoint}' of participant '{target}', which needs a runner-host port: {error}"
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::{
        artifact::ArtifactRole,
        case::{
            fixtures::{minimal_procedure, FixtureTypes, Server, STEPS},
            CaseLoader, Registries,
        },
        endpoint::{ConsumedEndpoint, Endpoint},
        layout::{Os, RealizationKind},
        participant::{EffectiveConfig, FactDeclaration},
        plan::{
            fixtures::{compile, compile_files, FILE_CONFIG},
            Planner,
        },
    };

    fn case(config: &str, os: &str) -> String {
        format!("timeout: 1s\nsubject_os: [{os}]\nparticipants:\n  subject: {{type: server, subject: true, config: {config}}}\n{}\n", minimal_procedure!())
    }

    #[track_caller]
    fn error(yaml: &str, os: &str, expected: &str) {
        assert_eq!(
            compile(yaml, Os::Linux).err().unwrap().to_string(),
            format!("case 'alpha' in cases/alpha/config.yaml: on subject OS '{os}', {expected}")
        );
    }

    struct Observer;

    impl ParticipantType for Observer {
        fn name(&self) -> &'static str {
            "observer"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            Server.supports()
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn params(&self) -> &'static [&'static str] {
            &["mode"]
        }

        fn validate_config(&self, _realization: Realization, config: &CaseValues) -> Result<(), String> {
            assert_eq!(config.env["PORT"], "17");
            assert_eq!(
                config.params["mode"],
                json!([{"nested": ["/logs/output.log", false, 7]}])
            );
            Ok(())
        }

        fn default_env(&self, _realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
            BTreeMap::from([
                ("COPY".into(), config.env["PORT"].clone()),
                ("RAW".into(), "{{product}}".into()),
            ])
        }

        fn endpoints(
            &self, realization: Realization, config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            Server.endpoints(realization, config)
        }

        fn consumes(&self, _realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            assert_eq!(config.env["PORT"], "17");
            assert_eq!(config.params["mode"][0]["nested"][0], "/logs/output.log");
            BTreeMap::new()
        }

        fn facts(&self, _realization: Realization) -> &'static [FactDeclaration] {
            &[FactDeclaration {
                name: "port",
                value: "17",
            }]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    #[test]
    fn fact_references_in_env_and_params_are_substituted_before_participant_type_hooks_see_them() {
        let registries = Registries {
            participant_types: &[&Observer, &Server],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        let yaml = concat!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: observer, subject: true, config: {env: {PORT: '{{ self.facts.port }}'}, params: {mode: [{nested: ['{{z-other.facts.file}}', false, 7]}]}}}\n  z-other: {type: server}\n", minimal_procedure!(), "\n");
        let case = loader
            .load_content(yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .unwrap();
        let config = plan.effective_config("subject").unwrap();
        assert_eq!(
            config.env,
            BTreeMap::from([
                ("PORT".into(), "17".into()),
                ("COPY".into(), "17".into()),
                ("RAW".into(), "{{product}}".into())
            ])
        );
        assert_eq!(
            config.params["mode"],
            json!([{"nested": ["/logs/output.log", false, 7]}])
        );
    }

    #[test]
    fn participant_type_hooks_see_recipe_time_references_as_written() {
        for reference in ["{{ self.late.ip }}", "{{self.endpoints.api.port}}"] {
            error(&case(&format!("{{env: {{PORT: '{reference}'}}}}"), "linux"), "linux", &format!(
                "participant 'subject' of type 'server' as linux-container on linux has invalid configuration: PORT '{reference}' is not a number"
            ));
        }
        let yaml = case(
            "{env: {KEY: '{{  self.late.ip  }}/{{self.endpoints.api.url}}'}}",
            "linux",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("subject").unwrap().env["KEY"],
            "{{  self.late.ip  }}/{{self.endpoints.api.url}}"
        );
    }

    #[test]
    fn a_fact_reference_in_config_fails_on_the_subject_os_whose_realization_lacks_it() {
        for config in [
            "{env: {KEY: '{{ self.facts.file }}'}}",
            "{params: {mode: {nested: ['{{ self.facts.file }}']}}}",
        ] {
            let field = if config.contains("env:") {
                "environment variable 'KEY'"
            } else {
                "parameter 'mode'"
            };
            error(&case(config, "windows, macos, linux"), "macos", &format!(
                "participant 'subject' of type 'server' as host-process-group on macos {field} '{{{{ self.facts.file }}}}' references fact 'file' of participant 'subject', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
            ));
        }
        error(&case("{env: {KEY: '{{ self.facts.unknown }}'}}", "linux"), "linux",
            "participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{ self.facts.unknown }}' references fact 'unknown' of participant 'subject', which type 'server' does not provide as linux-container on linux; it provides it as none");
    }

    #[test]
    fn a_host_process_groups_ip_resolves_to_loopback_in_env_and_params() {
        for name in ["self", "subject", "other"] {
            let yaml = case(&format!("{{env: {{KEY: 'prefix/{{{{ {name}.late.ip }}}}'}}, params: {{mode: [{{nested: '{{{{{name}.late.ip}}}}'}}]}}}}"), "macos")
                .replace(minimal_procedure!(), concat!("  other: {type: server, config: {env: {PORT: '8081', METRICS: 'off'}}}\n", minimal_procedure!()));
            let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
            let config = plan.effective_config("subject").unwrap();
            assert_eq!(config.env["KEY"], "prefix/127.0.0.1");
            assert_eq!(config.params["mode"], json!([{"nested": "127.0.0.1"}]));
        }
        // Loopback stays within the referencing participant's network namespace.
        for (os, kind) in [
            ("linux", "linux-container"),
            ("linux", "host-process-group"),
            ("windows", "windows-container"),
            ("linux", "kubernetes-pod"),
        ] {
            for (field, config) in [
                ("environment variable 'KEY'", "{env: {KEY: '{{other.late.ip}}'}}"),
                ("parameter 'mode'", "{params: {mode: ['{{other.late.ip}}']}}"),
            ] {
                let yaml = case(config, os)
                    .replace("type: server", "type: every-kind-server")
                    .replace(
                        "subject: true",
                        &format!("subject: true, realization: {{kind: {kind}}}"),
                    )
                    .replace(
                        minimal_procedure!(),
                        concat!("  other: {type: server, os: macos}\n", minimal_procedure!()),
                    );
                error(&yaml, os, &format!("participant 'subject' of type 'every-kind-server' as {kind} on {os} {field} '{{{{other.late.ip}}}}' references late fact 'ip' of participant 'other', which a host-process-group on macos reports only to participants on its own host"));
            }
        }
    }

    #[test]
    fn a_containers_own_ip_in_env_needs_a_type_that_substitutes_it() {
        let yaml = case("{env: {KEY: '{{ self.late.ip }}'}}", "linux");
        assert_eq!(
            compile(&yaml, Os::Linux)
                .unwrap()
                .unwrap()
                .effective_config("subject")
                .unwrap()
                .env["KEY"],
            "{{ self.late.ip }}"
        );
        error(&yaml.replace("[linux]", "[linux, windows]"), "windows",
            "participant 'subject' of type 'server' as windows-container on windows environment variable 'KEY' '{{ self.late.ip }}' references its own late fact 'ip', which type 'server' does not substitute into its environment as windows-container");
        for (fact, reason) in [
            ("container_id", "references its own late fact 'container_id', which type 'server' does not substitute into its environment as linux-container"),
            ("pod_uid", "references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip"),
        ] {
            error(&case(&format!("{{env: {{KEY: '{{{{ self.late.{fact} }}}}'}}}}"), "linux"), "linux", &format!(
                "participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{{{ self.late.{fact} }}}}' {reason}"
            ));
        }
    }

    #[test]
    fn own_late_facts_in_parameters_are_rejected_unless_fixed() {
        for (fact, reason) in [
            (
                "container_id",
                "references its own late fact 'container_id', which a parameter cannot wait for",
            ),
            (
                "ip",
                "references its own late fact 'ip', which a parameter cannot wait for",
            ),
            (
                "pod_uid",
                "references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip",
            ),
        ] {
            error(&case(&format!("{{params: {{mode: [{{nested: '{{{{self.late.{fact}}}}}'}}]}}}}"), "linux"), "linux", &format!(
                "participant 'subject' of type 'server' as linux-container on linux parameter 'mode' '{{{{self.late.{fact}}}}}' {reason}"
            ));
        }
        let plan = compile(&case("{params: {mode: '{{self.late.ip}}'}}", "macos"), Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(plan.effective_config("subject").unwrap().params["mode"], "127.0.0.1");
    }

    #[test]
    fn a_host_process_groups_missing_late_facts_are_reported_before_substitution() {
        for fact in ["container_id", "pod_uid"] {
            for (field, config) in [
                (
                    "environment variable 'KEY'",
                    format!("{{env: {{KEY: '{{{{self.late.{fact}}}}}'}}}}"),
                ),
                (
                    "parameter 'mode'",
                    format!("{{params: {{mode: '{{{{self.late.{fact}}}}}'}}}}"),
                ),
            ] {
                error(&case(&config, "macos"), "macos", &format!("participant 'subject' of type 'server' as host-process-group on macos {field} '{{{{self.late.{fact}}}}}' references late fact '{fact}' of participant 'subject', which a host-process-group does not have; a host-process-group has ip"));
            }
        }
    }

    #[test]
    fn another_participants_container_id_is_accepted_only_when_known_after_create() {
        for (os, kind, reason) in [
            ("linux", "linux-container", None),
            ("windows", "windows-container", None),
            ("linux", "kubernetes-pod", Some("knows only after it starts")),
            (
                "macos",
                "host-process-group",
                Some("does not have; a host-process-group has ip"),
            ),
        ] {
            for (config, field) in [
                (
                    "{env: {KEY: '{{  other.late.container_id  }}'}}",
                    "environment variable 'KEY'",
                ),
                (
                    "{params: {mode: ['{{  other.late.container_id  }}']}}",
                    "parameter 'mode'",
                ),
            ] {
                let yaml = case(config, "linux").replace(
                    minimal_procedure!(),
                    &format!(
                        "  other: {{type: every-kind-server, os: {os}, realization: {{kind: {kind}}}}}\n{}",
                        minimal_procedure!()
                    ),
                );
                if let Some(reason) = reason {
                    error(&yaml, "linux", &format!(
                        "participant 'subject' of type 'server' as linux-container on linux {field} '{{{{  other.late.container_id  }}}}' references late fact 'container_id' of participant 'other', which a {kind} {reason}"
                    ));
                } else {
                    let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
                    let config = plan.effective_config("subject").unwrap();
                    if field.starts_with("environment") {
                        assert_eq!(config.env["KEY"], "{{  other.late.container_id  }}");
                    } else {
                        assert_eq!(config.params["mode"], json!(["{{  other.late.container_id  }}"]));
                    }
                }
            }
        }
    }

    #[test]
    fn another_participants_ip_and_pod_uid_are_rejected_in_config() {
        for kind in ["linux-container", "kubernetes-pod"] {
            for fact in ["ip", "pod_uid"] {
                for (field, config) in [
                    (
                        "environment variable 'KEY'",
                        format!("{{env: {{KEY: '{{{{other.late.{fact}}}}}'}}}}"),
                    ),
                    (
                        "parameter 'mode'",
                        format!("{{params: {{mode: ['{{{{other.late.{fact}}}}}']}}}}"),
                    ),
                ] {
                    let yaml = case(&config, "linux").replace(
                        minimal_procedure!(),
                        &format!(
                            "  other: {{type: every-kind-server, realization: {{kind: {kind}}}}}\n{}",
                            minimal_procedure!()
                        ),
                    );
                    let reason = if kind == "linux-container" && fact == "pod_uid" {
                        "references late fact 'pod_uid' of participant 'other', which a linux-container does not have; a linux-container has container_id, ip"
                            .to_string()
                    } else {
                        format!("references late fact '{fact}' of participant 'other'; configuration can use another participant's late fact only as the container_id of a participant known once created")
                    };
                    error(&yaml, "linux", &format!(
                        "participant 'subject' of type 'server' as linux-container on linux {field} '{{{{other.late.{fact}}}}}' {reason}"
                    ));
                }
            }
        }
    }

    #[test]
    fn naming_the_holder_is_the_same_as_self() {
        for name in ["self", "subject"] {
            let yaml = case(&format!("{{env: {{FILE: '{{{{{name}.facts.file}}}}', KEY: '{{{{{name}.late.ip}}}}/{{{{{name}.endpoints.api.url}}}}'}}}}"), "linux");
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            let config = plan.effective_config("subject").unwrap();
            assert_eq!(config.env["FILE"], "/logs/output.log");
            assert_eq!(
                config.env["KEY"],
                format!("{{{{{name}.late.ip}}}}/{{{{{name}.endpoints.api.url}}}}")
            );
            error(&case(&format!("{{params: {{mode: '{{{{{name}.late.container_id}}}}'}}}}"), "linux"), "linux", &format!(
                "participant 'subject' of type 'server' as linux-container on linux parameter 'mode' '{{{{{name}.late.container_id}}}}' references its own late fact 'container_id', which a parameter cannot wait for"
            ));
        }
    }

    #[test]
    fn endpoint_references_need_a_served_endpoint_and_a_route_from_the_holder() {
        let yaml = case("{env: {KEY: '{{ other.endpoints.metrics.url }}'}}", "linux").replace(
            minimal_procedure!(),
            concat!(
                "  other: {type: server, config: {env: {METRICS: 'off'}}}\n",
                minimal_procedure!()
            ),
        );
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{ other.endpoints.metrics.url }}' references endpoint 'metrics' of participant 'other', which it does not serve; it serves api");
        let yaml = yaml.replace("metrics.url", "api.url").replace(
            "other: {type: server, config",
            "other: {type: server, os: macos, config",
        );
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{ other.endpoints.api.url }}' references endpoint 'api' of participant 'other', which it cannot reach: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container");
        let yaml = yaml.replace("os: macos, ", "");
        assert_eq!(
            compile(&yaml, Os::Linux)
                .unwrap()
                .unwrap()
                .effective_config("subject")
                .unwrap()
                .env["KEY"],
            "{{ other.endpoints.api.url }}"
        );
    }

    #[test]
    fn endpoint_references_to_the_holder_skip_the_route_check() {
        for name in ["self", "subject"] {
            let yaml = case(
                &format!("{{env: {{KEY: '{{{{{name}.endpoints.pipe.path}}}}'}}}}"),
                "windows",
            )
            .replace("type: server", "type: every-kind-server");
            assert_eq!(
                compile(&yaml, Os::Windows)
                    .unwrap()
                    .unwrap()
                    .effective_config("subject")
                    .unwrap()
                    .env["KEY"],
                format!("{{{{{name}.endpoints.pipe.path}}}}")
            );
        }
    }

    #[test]
    fn address_parts_must_suit_the_endpoint_kind_for_endpoint_and_connection_references() {
        for (endpoint, kind, network) in [
            ("tcp", "tcp", true),
            ("udp", "udp", true),
            ("http", "http", true),
            ("grpc", "grpc", true),
            ("datagram", "unix-datagram", false),
            ("stream", "unix-stream", false),
            ("pipe", "named-pipe", false),
        ] {
            for part in ["url", "host", "port", "path"] {
                for connection in [false, true] {
                    if connection && endpoint == "pipe" {
                        continue;
                    }
                    let reference = if connection {
                        format!("{{{{self.connections.target.{part}}}}}")
                    } else {
                        format!("{{{{self.endpoints.{endpoint}.{part}}}}}")
                    };
                    let os = if endpoint == "pipe" { "windows" } else { "linux" };
                    let selected = if endpoint == "pipe" { Os::Windows } else { Os::Linux };
                    let (type_name, config) = if connection {
                        ("every-kind-client", format!("{{env: {{KEY: '{reference}'}}}}"))
                    } else {
                        (
                            "every-kind-server",
                            format!("{{params: {{mode: [{{nested: '{reference}'}}]}}}}"),
                        )
                    };
                    let mut yaml = case(&config, os).replace("type: server", &format!("type: {type_name}"));
                    if connection {
                        yaml = yaml
                            .replace(
                                "subject: true",
                                &format!("subject: true, connect: {{target: other.{endpoint}}}"),
                            )
                            .replace(minimal_procedure!(), concat!("  other: {type: every-kind-server, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}}\n", minimal_procedure!()));
                    }
                    let valid = part == "url" || (network && part != "path") || (!network && part == "path");
                    if valid {
                        let plan = compile(&yaml, selected).unwrap().unwrap();
                        let config = plan.effective_config("subject").unwrap();
                        if connection {
                            assert_eq!(config.env["KEY"], reference);
                        } else {
                            assert_eq!(config.params["mode"][0]["nested"], reference);
                        }
                    } else {
                        let field = if connection {
                            "environment variable 'KEY'"
                        } else {
                            "parameter 'mode'"
                        };
                        let allowed = if network { "url, host or port" } else { "url or path" };
                        let realization = if endpoint == "pipe" {
                            "windows-container"
                        } else {
                            "linux-container"
                        };
                        error(&yaml, os, &format!("participant 'subject' of type '{type_name}' as {realization} on {os} {field} '{reference}': a {kind} endpoint has no '{part}'; use {allowed}"));
                    }
                }
            }
        }
    }

    #[test]
    fn an_overlay_value_is_checked_on_its_os_instead_of_the_base_value_it_replaces() {
        let yaml = case("{env: {KEY: '{{ self.facts.file }}'}, params: {mode: ['{{self.facts.file}}']}, os: {macos: {env: {KEY: '{{self.late.ip}}'}, params: {mode: {host: '{{self.late.ip}}'}}}}}", "linux, macos, windows");
        for (os, expected, params) in [
            (Os::Linux, "/logs/output.log", json!(["/logs/output.log"])),
            (Os::MacOs, "127.0.0.1", json!({"host": "127.0.0.1"})),
            (Os::Windows, r"C:\logs\output.log", json!([r"C:\logs\output.log"])),
        ] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            let config = plan.effective_config("subject").unwrap();
            assert_eq!(config.env["KEY"], expected);
            assert_eq!(config.params["mode"], params);
        }
        let yaml = case(
            "{env: {KEY: 'base'}, os: {windows: {env: {KEY: '{{ self.late.ip }}'}}}}",
            "linux, windows",
        );
        error(&yaml, "windows", "participant 'subject' of type 'server' as windows-container on windows environment variable 'KEY' '{{ self.late.ip }}' references its own late fact 'ip', which type 'server' does not substitute into its environment as windows-container");
    }

    #[test]
    fn a_non_subject_participants_overlay_uses_its_own_os() {
        let yaml = case("{env: {KEY: '{{other.late.ip}}'}}", "linux").replace(minimal_procedure!(),
            concat!("  other: {type: server, os: macos, config: {env: {KEY: '{{self.facts.file}}'}, params: {mode: '{{self.facts.file}}'}, os: {macos: {env: {KEY: '{{self.late.ip}}'}, params: {mode: '{{self.late.ip}}'}}}}}\n", minimal_procedure!()));
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{other.late.ip}}' references late fact 'ip' of participant 'other', which a host-process-group on macos reports only to participants on its own host");
        let yaml = yaml.replace("{{other.late.ip}}", "literal");
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject").unwrap().env["KEY"], "literal");
        let config = plan.effective_config("other").unwrap();
        assert_eq!(config.env["KEY"], "127.0.0.1");
        assert_eq!(config.params["mode"], "127.0.0.1");
    }

    #[test]
    fn nested_parameter_strings_are_checked_depth_first_in_key_order() {
        for reference_kind in ["facts", "endpoints"] {
            let mut yaml = case("{params: {mode: {z: '{{self.facts.last}}', a: ['{{self.facts.first}}', {z: '{{self.facts.third}}', a: '{{self.facts.second}}'}]}}}", "linux");
            if reference_kind == "endpoints" {
                yaml = yaml.replace(".facts.", ".endpoints.").replace("}}'", ".url}}'");
            }
            for name in ["first", "second", "third", "last"] {
                let (reference, reason) = if reference_kind == "facts" {
                    (format!("{{{{self.facts.{name}}}}}"), format!("references fact '{name}' of participant 'subject', which type 'server' does not provide as linux-container on linux; it provides it as none"))
                } else {
                    (format!("{{{{self.endpoints.{name}.url}}}}"), format!("references endpoint '{name}' of participant 'subject', which it does not serve; it serves api, metrics"))
                };
                error(&yaml, "linux", &format!("participant 'subject' of type 'server' as linux-container on linux parameter 'mode' '{reference}' {reason}"));
                yaml = yaml.replace(&reference, "literal");
            }
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject").unwrap().params["mode"],
                json!({"z": "literal", "a": ["literal", {"z": "literal", "a": "literal"}]})
            );
        }
    }

    #[test]
    fn config_reference_errors_follow_the_documented_order() {
        let mut yaml = case("{env: {Z: '{{self.facts.z}}', PORT: broken, A: '{{self.facts.a}}'}, params: {mode: '{{self.facts.param}}'}}", "linux");
        for (key, fact) in [("A", "a"), ("Z", "z")] {
            error(&yaml, "linux", &format!("participant 'subject' of type 'server' as linux-container on linux environment variable '{key}' '{{{{self.facts.{fact}}}}}' references fact '{fact}' of participant 'subject', which type 'server' does not provide as linux-container on linux; it provides it as none"));
            yaml = yaml.replace(&format!("{{{{self.facts.{fact}}}}}"), "literal");
        }
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux parameter 'mode' '{{self.facts.param}}' references fact 'param' of participant 'subject', which type 'server' does not provide as linux-container on linux; it provides it as none");
        yaml = yaml.replace("{{self.facts.param}}", "literal");
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux has invalid configuration: PORT 'broken' is not a number");

        let yaml = case(
            "{env: {KEY: '{{self.facts.missing}}'}, params: {mode: broken}}",
            "windows",
        )
        .replace("type: server", "type: configured");
        error(&yaml, "windows", "participant 'subject' of type 'configured' as windows-container on windows environment variable 'KEY' '{{self.facts.missing}}' references fact 'missing' of participant 'subject', which type 'configured' does not provide as windows-container on windows; it provides it as none");

        let mut yaml = case(
            "{env: {KEY: '{{self.endpoints.missing.url}}'}, params: {mode: '{{self.endpoints.other.url}}'}}",
            "linux",
        )
        .replace("subject: true", "subject: true, connect: {typo: other.api}")
        .replace(
            minimal_procedure!(),
            "  other: {type: server}\nprocedure: [{step: probe, on: subject, path: '{{ subject.late.pod_uid }}'}]",
        );
        error(&yaml, "linux", "participant 'subject' of type 'server' as linux-container on linux connects 'typo', which it does not consume; it consumes none");
        yaml = yaml.replace(", connect: {typo: other.api}", "");
        for (field, endpoint) in [("environment variable 'KEY'", "missing"), ("parameter 'mode'", "other")] {
            error(&yaml, "linux", &format!("participant 'subject' of type 'server' as linux-container on linux {field} '{{{{self.endpoints.{endpoint}.url}}}}' references endpoint '{endpoint}' of participant 'subject', which it does not serve; it serves api, metrics"));
            yaml = yaml.replace(&format!("{{{{self.endpoints.{endpoint}.url}}}}"), "literal");
        }
        error(&yaml, "linux", "step 1 references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip");
    }

    fn file_case(os: &str) -> String {
        case(FILE_CONFIG, os).replace("type: server", "type: file-server")
    }

    #[track_caller]
    fn file_error(yaml: &str, text: &str, os: &str, expected: &str) {
        assert_eq!(
            compile_files(yaml, &[("source.yaml", text)], Os::Linux).err().unwrap(),
            format!("case 'alpha' in cases/alpha/config.yaml: on subject OS '{os}', {expected}")
        );
    }

    #[test]
    fn template_file_facts_are_checked_from_loaded_contents_on_every_subject_os() {
        for name in ["self", "subject"] {
            let text = format!("path: {{{{ {name}.facts.file }}}}");
            file_error(&file_case("windows, macos, linux"), &text, "macos", &format!(
                "participant 'subject' of type 'file-server' as host-process-group on macos configuration file 'settings.yaml' '{{{{ {name}.facts.file }}}}' references fact 'file' of participant 'subject', which type 'file-server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"));
            let plan = compile_files(&file_case("linux, windows"), &[("source.yaml", &text)], Os::Linux).unwrap();
            assert_eq!(
                plan.effective_config("subject").unwrap().files["settings.yaml"],
                crate::case::FileSource {
                    from: "source.yaml".into(),
                    template: true
                }
            );
            assert_eq!(
                plan.case().file_templates().get(Path::new("source.yaml")),
                &text.parse().unwrap()
            );
        }
    }

    #[test]
    fn shared_template_self_references_are_checked_for_each_holder() {
        let yaml = file_case("macos").replace(
            minimal_procedure!(),
            &format!(
                "  other: {{type: file-server, os: linux, config: {FILE_CONFIG}}}\n{}",
                minimal_procedure!()
            ),
        );
        file_error(&yaml, "{{ self.facts.file }}", "macos",
            "participant 'subject' of type 'file-server' as host-process-group on macos configuration file 'settings.yaml' '{{ self.facts.file }}' references fact 'file' of participant 'subject', which type 'file-server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows");
    }

    #[test]
    fn only_template_files_in_effect_for_the_participants_os_are_checked() {
        for replacement in ["{from: host.yaml, template: true}", "host.yaml"] {
            let config = format!("{{files: {{settings.yaml: {{from: source.yaml, template: true}}}}, os: {{macos: {{files: {{settings.yaml: {replacement}}}}}}}}}");
            let yaml = case(&config, "linux, macos, windows").replace("type: server", "type: file-server");
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let plan = compile_files(
                    &yaml,
                    &[
                        ("source.yaml", "{{self.facts.file}}"),
                        ("host.yaml", "{{self.late.ip}}"),
                    ],
                    os,
                )
                .unwrap();
                let source = &plan.effective_config("subject").unwrap().files["settings.yaml"];
                assert_eq!(
                    source.from,
                    Path::new(if os == Os::MacOs { "host.yaml" } else { "source.yaml" })
                );
                assert_eq!(source.template, os != Os::MacOs || replacement.contains("true"));
            }
            let yaml = file_case("linux").replace(
                minimal_procedure!(),
                &format!(
                    "  other: {{type: file-server, os: macos, config: {config}}}\n{}",
                    minimal_procedure!()
                ),
            );
            let plan = compile_files(
                &yaml,
                &[
                    ("source.yaml", "{{self.facts.file}}"),
                    ("host.yaml", "{{self.late.ip}}"),
                ],
                Os::Linux,
            )
            .unwrap();
            assert_eq!(
                plan.effective_config("other").unwrap().files["settings.yaml"].from,
                Path::new("host.yaml")
            );
        }
    }

    #[test]
    fn pod_template_files_accept_their_own_and_other_participants_existing_late_facts() {
        for target_kind in ["kubernetes-pod", "linux-container"] {
            let yaml = file_case("linux")
                .replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}")
                .replace(
                    minimal_procedure!(),
                    &format!(
                        "  other: {{type: file-server, realization: {{kind: {target_kind}}}}}\n{}",
                        minimal_procedure!()
                    ),
                );
            let text = format!("{{{{self.late.ip}}}} {{{{subject.late.container_id}}}} {{{{self.late.pod_uid}}}} {{{{other.late.ip}}}} {{{{other.late.container_id}}}} {}", if target_kind == "kubernetes-pod" { "{{other.late.pod_uid}}" } else { "" });
            let plan = compile_files(&yaml, &[("source.yaml", &text)], Os::Linux).unwrap();
            assert_eq!(
                plan.case().file_templates().get(Path::new("source.yaml")),
                &text.parse().unwrap()
            );
            if target_kind == "linux-container" {
                file_error(&yaml, "{{other.late.pod_uid}}", "linux", "participant 'subject' of type 'file-server' as kubernetes-pod on linux configuration file 'settings.yaml' '{{other.late.pod_uid}}' references late fact 'pod_uid' of participant 'other', which a linux-container does not have; a linux-container has container_id, ip");
            }
        }
    }

    #[test]
    fn non_pod_template_files_reject_their_own_nonfixed_late_facts() {
        for (os, kind) in [
            ("linux", "linux-container"),
            ("windows", "windows-container"),
            ("macos", "host-process-group"),
        ] {
            for name in ["self", "subject"] {
                for fact in ["ip", "container_id", "pod_uid"] {
                    if os == "macos" && fact == "ip" {
                        continue;
                    }
                    let reason = if fact == "pod_uid" || os == "macos" {
                        let names = if os == "macos" { "ip" } else { "container_id, ip" };
                        format!("references late fact '{fact}' of participant 'subject', which a {kind} does not have; a {kind} has {names}")
                    } else {
                        format!("references its own late fact '{fact}', which a configuration file of a {kind} cannot wait for")
                    };
                    file_error(&file_case(os), &format!("{{{{{name}.late.{fact}}}}}"), os, &format!("participant 'subject' of type 'file-server' as {kind} on {os} configuration file 'settings.yaml' '{{{{{name}.late.{fact}}}}}' {reason}"));
                }
            }
        }
    }

    #[test]
    fn non_pod_template_files_accept_another_containers_id_but_not_after_start_facts() {
        for (os, kind) in [
            ("linux", "linux-container"),
            ("windows", "windows-container"),
            ("linux", "kubernetes-pod"),
        ] {
            let yaml = file_case("linux").replace(
                minimal_procedure!(),
                &format!(
                    "  other: {{type: file-server, os: {os}, realization: {{kind: {kind}}}}}\n{}",
                    minimal_procedure!()
                ),
            );
            for fact in ["container_id", "ip", "pod_uid"] {
                let text = format!("{{{{other.late.{fact}}}}}");
                if fact == "container_id" && kind != "kubernetes-pod" {
                    let plan = compile_files(&yaml, &[("source.yaml", &text)], Os::Linux).unwrap();
                    assert!(plan.effective_config("subject").unwrap().files["settings.yaml"].template);
                    continue;
                }
                let reason = if fact == "container_id" {
                    format!("references late fact '{fact}' of participant 'other', which a {kind} knows only after it starts")
                } else if fact == "pod_uid" && kind != "kubernetes-pod" {
                    format!("references late fact '{fact}' of participant 'other', which a {kind} does not have; a {kind} has container_id, ip")
                } else {
                    format!("references late fact '{fact}' of participant 'other'; configuration can use another participant's late fact only as the container_id of a participant known once created")
                };
                file_error(&yaml, &text, "linux", &format!("participant 'subject' of type 'file-server' as linux-container on linux configuration file 'settings.yaml' '{text}' {reason}"));
            }
        }
    }

    #[test]
    fn a_host_process_groups_ip_in_a_template_file_requires_the_same_realization() {
        let yaml = file_case("macos").replace(
            minimal_procedure!(),
            concat!(
                "  other: {type: file-server, config: {env: {PORT_OFFSET: '100'}}}\n",
                minimal_procedure!()
            ),
        );
        let text = "{{self.late.ip}} {{subject.late.ip}} {{other.late.ip}}";
        let plan = compile_files(&yaml, &[("source.yaml", text)], Os::MacOs).unwrap();
        assert_eq!(
            plan.case().file_templates().get(Path::new("source.yaml")),
            &text.parse().unwrap()
        );
        for (os, kind) in [
            ("linux", "linux-container"),
            ("linux", "host-process-group"),
            ("windows", "windows-container"),
            ("linux", "kubernetes-pod"),
        ] {
            let yaml = file_case(os)
                .replace(
                    "subject: true",
                    &format!("subject: true, realization: {{kind: {kind}}}"),
                )
                .replace(
                    minimal_procedure!(),
                    concat!("  other: {type: file-server, os: macos}\n", minimal_procedure!()),
                );
            file_error(&yaml, "{{other.late.ip}}", os, &format!("participant 'subject' of type 'file-server' as {kind} on {os} configuration file 'settings.yaml' '{{{{other.late.ip}}}}' references late fact 'ip' of participant 'other', which a host-process-group on macos reports only to participants on its own host"));
        }
    }

    #[test]
    fn template_endpoint_references_need_a_served_endpoint_and_a_route() {
        let yaml = file_case("linux").replace(
            minimal_procedure!(),
            concat!(
                "  other: {type: file-server, config: {env: {TCP: 'off'}}}\n",
                minimal_procedure!()
            ),
        );
        file_error(&yaml, "{{other.endpoints.tcp.url}}", "linux", "participant 'subject' of type 'file-server' as linux-container on linux configuration file 'settings.yaml' '{{other.endpoints.tcp.url}}' references endpoint 'tcp' of participant 'other', which it does not serve; it serves datagram, grpc, http, pipe, stream, udp");
        let yaml = yaml.replace("TCP: 'off'", "TCP: 'on'");
        let text = "{{other.endpoints.tcp.url}}";
        assert_eq!(
            compile_files(&yaml, &[("source.yaml", text)], Os::Linux)
                .unwrap()
                .case()
                .file_templates()
                .get(Path::new("source.yaml")),
            &text.parse().unwrap()
        );
        let yaml = yaml.replace("other: {type: file-server", "other: {type: file-server, os: macos");
        file_error(&yaml, text, "linux", "participant 'subject' of type 'file-server' as linux-container on linux configuration file 'settings.yaml' '{{other.endpoints.tcp.url}}' references endpoint 'tcp' of participant 'other', which it cannot reach: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container");
    }

    #[test]
    fn template_connection_references_are_checked_after_connection_targets_and_routes() {
        let yaml = file_case("linux")
            .replace("subject: true", "subject: true, connect: {target: other.tcp}")
            .replace(
                minimal_procedure!(),
                concat!(
                    "  other: {type: file-server, config: {env: {TCP: 'off'}}}\n",
                    minimal_procedure!()
                ),
            );
        let text = "{{self.connections.target.path}}";
        file_error(&yaml, text, "linux", "participant 'subject' connects 'target' to 'other.tcp', which participant 'other' of type 'file-server' as linux-container on linux does not serve; it serves datagram, grpc, http, pipe, stream, udp");
        let yaml = yaml.replace("TCP: 'off'", "TCP: 'on'");
        file_error(&yaml, text, "linux", "participant 'subject' of type 'file-server' as linux-container on linux configuration file 'settings.yaml' '{{self.connections.target.path}}': a tcp endpoint has no 'path'; use url, host or port");
        let yaml = yaml.replace("other: {type: file-server", "other: {type: file-server, os: macos");
        file_error(&yaml, text, "linux", "participant 'subject' connects 'target' to 'other.tcp', an endpoint of kind tcp; participant 'subject' of type 'file-server' as linux-container on linux cannot reach participant 'other' of type 'file-server' as host-process-group on macos: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container");
    }

    #[test]
    fn template_address_checks_use_only_the_files_selected_by_the_os_overlay() {
        let config = "{files: {settings.yaml: {from: source.yaml, template: true}}, os: {macos: {env: {TCP: 'off'}, files: {settings.yaml: {from: host.yaml, template: true}}}}}";
        let yaml = case(config, "linux, macos, windows").replace("type: server", "type: file-server");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile_files(
                &yaml,
                &[
                    ("source.yaml", "{{self.endpoints.tcp.url}}"),
                    ("host.yaml", "{{self.endpoints.udp.url}}"),
                ],
                os,
            )
            .unwrap();
            assert_eq!(
                plan.effective_config("subject").unwrap().files["settings.yaml"].from,
                Path::new(if os == Os::MacOs { "host.yaml" } else { "source.yaml" })
            );
        }
    }

    #[test]
    fn template_endpoint_and_connection_parts_must_match_the_endpoint_kind() {
        for (endpoint, kind, network) in [
            ("tcp", "tcp", true),
            ("udp", "udp", true),
            ("http", "http", true),
            ("grpc", "grpc", true),
            ("datagram", "unix-datagram", false),
            ("stream", "unix-stream", false),
            ("pipe", "named-pipe", false),
        ] {
            for part in ["url", "host", "port", "path"] {
                for connection in [false, true] {
                    if connection && endpoint == "pipe" {
                        continue;
                    }
                    let (os, selected, realization) = if endpoint == "pipe" {
                        ("windows", Os::Windows, "windows-container")
                    } else {
                        ("linux", Os::Linux, "linux-container")
                    };
                    let mut yaml = file_case(os);
                    let text = if connection {
                        yaml = yaml
                            .replace(
                                "subject: true",
                                &format!("subject: true, connect: {{target: other.{endpoint}}}"),
                            )
                            .replace(minimal_procedure!(), concat!("  other: {type: file-server, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}}\n", minimal_procedure!()));
                        format!("{{{{self.connections.target.{part}}}}}")
                    } else {
                        format!("{{{{self.endpoints.{endpoint}.{part}}}}}")
                    };
                    if part == "url" || (network && part != "path") || (!network && part == "path") {
                        let plan = compile_files(&yaml, &[("source.yaml", &text)], selected).unwrap();
                        assert_eq!(
                            plan.case().file_templates().get(Path::new("source.yaml")),
                            &text.parse().unwrap()
                        );
                    } else {
                        let allowed = if network { "url, host or port" } else { "url or path" };
                        file_error(&yaml, &text, os, &format!("participant 'subject' of type 'file-server' as {realization} on {os} configuration file 'settings.yaml' '{text}': a {kind} endpoint has no '{part}'; use {allowed}"));
                    }
                }
            }
        }
    }

    #[test]
    fn template_files_follow_env_and_params_in_fact_and_address_checks() {
        for addresses in [false, true] {
            let reference = if addresses {
                "{{self.endpoints.missing.url}}"
            } else {
                "{{self.facts.missing}}"
            };
            let reason = if addresses {
                "references endpoint 'missing' of participant 'subject', which it does not serve; it serves datagram, grpc, http, pipe, stream, tcp, udp"
            } else {
                "references fact 'missing' of participant 'subject', which type 'file-server' does not provide as linux-container on linux; it provides it as none"
            };
            let config = format!("{{env: {{KEY: '{reference}'}}, params: {{mode: '{reference}'}}, files: {{z.yaml: {{from: z.yaml, template: true}}, settings.yaml: {{from: source.yaml, template: true}}}}}}");
            let mut yaml = case(&config, "linux").replace("type: server", "type: file-server");
            let mut sources = [("source.yaml", reference), ("z.yaml", reference)];
            for field in [
                "environment variable 'KEY'",
                "parameter 'mode'",
                "configuration file 'settings.yaml'",
                "configuration file 'z.yaml'",
            ] {
                assert_eq!(compile_files(&yaml, &sources, Os::Linux).err().unwrap(), format!("case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'subject' of type 'file-server' as linux-container on linux {field} '{reference}' {reason}"));
                match field {
                    "environment variable 'KEY'" => yaml = yaml.replace(&format!("KEY: '{reference}'"), "KEY: literal"),
                    "parameter 'mode'" => yaml = yaml.replace(&format!("mode: '{reference}'"), "mode: literal"),
                    "configuration file 'settings.yaml'" => sources[0].1 = "literal",
                    _ => sources[1].1 = "literal",
                }
            }
            assert_eq!(
                compile_files(&yaml, &sources, Os::Linux)
                    .unwrap()
                    .effective_config("subject")
                    .unwrap()
                    .files
                    .len(),
                2
            );
        }
        let yaml = file_case("linux").replace("config: {files:", "config: {env: {PORT: broken}, files:");
        file_error(&yaml, "{{self.facts.missing}}", "linux", "participant 'subject' of type 'file-server' as linux-container on linux configuration file 'settings.yaml' '{{self.facts.missing}}' references fact 'missing' of participant 'subject', which type 'file-server' does not provide as linux-container on linux; it provides it as none");
        file_error(&yaml, "literal", "linux", "participant 'subject' of type 'file-server' as linux-container on linux has invalid configuration: PORT 'broken' is not a number");
    }
}

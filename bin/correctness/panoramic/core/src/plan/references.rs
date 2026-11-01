//! Checks configuration references and substitutes facts available from the layout.

use std::collections::BTreeMap;

use super::{connections::ParticipantEndpoints, routes::check_route};
use crate::{
    backend::{late_facts, LateFact, LateFactTiming},
    case::{
        config_references::{walk_config_strings, walk_config_strings_mut, ConfigField},
        declared_names,
        template::{AddressPart, Reference, Template},
        CaseFile,
    },
    endpoint::EndpointKind,
    layout::{describe_participant, describe_realizations, Layout, Realization},
    participant::{find_type, CaseValues, ParticipantType},
};

pub(super) fn resolve_config_references(
    file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType], name: &str, values: &mut CaseValues,
) -> Result<(), String> {
    let realization = layout.get(name).expect("loaded participants have a layout");
    let holder = describe_participant(name, &file.participants[name].participant_type, realization);
    walk_config_strings_mut(&mut values.env, &mut values.params, |field, text| {
        let template: Template = text.parse().expect("configuration templates were checked at load");
        *text = template
            .substitute(|written, reference| {
                resolve_reference(file, layout, types, name, field, reference)
                    .map_err(|reason| format!("'{written}' {reason}"))
            })
            .map_err(|reason| format!("{holder} {field} {reason}"))?;
        Ok(())
    })
}

fn resolve_reference(
    file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType], holder: &str, field: ConfigField<'_>,
    reference: &Reference,
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
        if let Some(declaration) = participant_type
            .facts(realization)
            .iter()
            .find(|value| value.name == fact)
        {
            return Ok(Some(declaration.value.into()));
        }
        let provided = describe_realizations(participant_type.supports().iter().copied().filter(|&(os, kind)| {
            participant_type
                .facts(Realization { os, kind })
                .iter()
                .any(|value| value.name == fact)
        }));
        return Err(format!(
            "references fact '{fact}' of participant '{name}', which type '{}' does not provide as {} on {}; it provides it as {provided}",
            participant_type.name(), realization.kind.name(), realization.os.name(),
        ));
    }
    let Reference::Late { fact, .. } = reference else {
        unreachable!("only late references remain");
    };
    let fact_name = fact.name();
    let kind = realization.kind.name();
    let Some((_, timing)) = late_facts(realization.kind)
        .iter()
        .find(|(provided, _)| provided == fact)
    else {
        return Err(format!(
            "references late fact '{fact_name}' of participant '{name}', which a {kind} does not have"
        ));
    };
    if let LateFactTiming::Fixed(value) = timing {
        let holder_realization = layout.get(holder).expect("loaded participants have a layout");
        if holder_realization != realization {
            return Err(format!(
                "references late fact '{fact_name}' of participant '{name}', which a {kind} on {} reports only to participants on its own host",
                realization.os.name(),
            ));
        }
        return Ok(Some((*value).into()));
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
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
) -> Result<(), String> {
    for (name, participant) in endpoints {
        let realization = layout.get(name).expect("loaded participants have a layout");
        let entry = &file.participants[name];
        let holder = describe_participant(name, &entry.participant_type, realization);
        walk_config_strings(&participant.values.env, &participant.values.params, |field, text| {
            let template: Template = text
                .parse()
                .expect("substituted configuration retains valid references");
            for (written, reference) in template.written_references() {
                let context = format!("{holder} {field} '{written}'");
                let (kind, part) = match reference {
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
                        if target != name {
                            let target_realization = layout.get(target).expect("referenced participants have a layout");
                            check_route(served_endpoint.kind, realization, target_realization).map_err(|constraint| format!(
                                "{context} references endpoint '{endpoint}' of participant '{target}', which it cannot reach: {constraint}"
                            ))?;
                        }
                        (served_endpoint.kind, part)
                    }
                    Reference::Connection { consumed, part } => {
                        let target = &entry.connect[consumed];
                        (endpoints[&target.participant].served[&target.endpoint].kind, part)
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
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::{
        case::{
            fixtures::{FixtureTypes, Server},
            CaseLoader, Registries,
        },
        endpoint::{ConsumedEndpoint, Endpoint},
        layout::{Os, RealizationKind},
        participant::{EffectiveConfig, FactDeclaration},
        plan::{tests::compile, CasePlan},
    };

    fn case(config: &str, os: &str) -> String {
        format!("timeout: 1s\nsubject_os: [{os}]\nparticipants:\n  subject: {{type: server, subject: true, config: {config}}}\nprocedure: []\n")
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
            steps: &[],
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        let yaml = "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: observer, subject: true, config: {env: {PORT: '{{ self.facts.port }}'}, params: {mode: [{nested: ['{{z-other.facts.file}}', false, 7]}]}}}\n  z-other: {type: server}\nprocedure: []\n";
        let case = loader
            .load_content(yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(case, Os::Linux, &registries).unwrap().unwrap();
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
                .replace("procedure: []", "  other: {type: server}\nprocedure: []");
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
                    .replace("procedure: []", "  other: {type: server, os: macos}\nprocedure: []");
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
            ("pod_uid", "references late fact 'pod_uid' of participant 'subject', which a linux-container does not have"),
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
                "references late fact 'pod_uid' of participant 'subject', which a linux-container does not have",
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
                error(&case(&config, "macos"), "macos", &format!("participant 'subject' of type 'server' as host-process-group on macos {field} '{{{{self.late.{fact}}}}}' references late fact '{fact}' of participant 'subject', which a host-process-group does not have"));
            }
        }
    }

    #[test]
    fn another_participants_container_id_is_accepted_only_when_known_after_create() {
        for (os, kind, reason) in [
            ("linux", "linux-container", None),
            ("windows", "windows-container", None),
            ("linux", "kubernetes-pod", Some("knows only after it starts")),
            ("macos", "host-process-group", Some("does not have")),
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
                    "procedure: []",
                    &format!(
                        "  other: {{type: every-kind-server, os: {os}, realization: {{kind: {kind}}}}}\nprocedure: []"
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
                        "procedure: []",
                        &format!("  other: {{type: every-kind-server, realization: {{kind: {kind}}}}}\nprocedure: []"),
                    );
                    let reason = if kind == "linux-container" && fact == "pod_uid" {
                        "references late fact 'pod_uid' of participant 'other', which a linux-container does not have"
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
            "procedure: []",
            "  other: {type: server, config: {env: {METRICS: 'off'}}}\nprocedure: []",
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
                            .replace("procedure: []", "  other: {type: every-kind-server}\nprocedure: []");
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
        let yaml = case("{env: {KEY: '{{other.late.ip}}'}}", "linux").replace("procedure: []",
            "  other: {type: server, os: macos, config: {env: {KEY: '{{self.facts.file}}'}, params: {mode: '{{self.facts.file}}'}, os: {macos: {env: {KEY: '{{self.late.ip}}'}, params: {mode: '{{self.late.ip}}'}}}}}\nprocedure: []");
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
            "procedure: []",
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
}
